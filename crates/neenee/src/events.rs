//! Event-sourced session persistence (C11 foundation).
//!
//! The event log is the authoritative history for each project. `SessionStore`
//! replays the log on load to rebuild the in-memory [`SessionData`] snapshot,
//! and appends a new event for every mutation. The snapshot file is kept as a
//! cache so readers that do not need the full replay path can still open it,
//! but on a conflict the log wins.
//!
//! Events are stored as JSON Lines. Each line is an [`EventEnvelope`] carrying
//! a monotonic sequence number, a wall-clock timestamp, and the event payload.

use crate::fsutil;
use crate::session::{CompactionCheckpoint, LoopCheckpoint};
use neenee_core::Message;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// A single change to a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Session was created or opened from a prior snapshot.
    Started {
        id: String,
        parent_id: Option<String>,
        created_at: u64,
        project_root: PathBuf,
        schema_version: u32,
    },
    /// The active message list was replaced (e.g. after a turn, on open, or
    /// after tool-result pruning).
    MessagesReplaced {
        messages: Vec<Message>,
    },
    /// The autonomous-loop checkpoint changed.
    CheckpointSet {
        checkpoint: Option<LoopCheckpoint>,
    },
    /// A compaction archived older turns and replaced the active window.
    CompactionCommitted {
        archived: Vec<Message>,
        active: Vec<Message>,
        checkpoint: CompactionCheckpoint,
    },
    /// Messages were moved into the archived list without a compaction.
    Archived {
        messages: Vec<Message>,
    },
    /// The active session was reset to a fresh empty session.
    Reset {
        id: String,
    },
    /// The current session was forked: the active id changed and a parent link
    /// was recorded. Any archived messages are preserved by a preceding
    /// `Archived` event.
    Forked {
        id: String,
        parent_id: String,
    },
}

/// Wrapper around a [`SessionEvent`] that adds metadata for ordering and
/// debugging. Stored as one JSON object per line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub seq: u64,
    pub timestamp: u64,
    #[serde(flatten)]
    pub event: SessionEvent,
}

/// Append-only event log for one project.
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Read all events in log order.
    pub fn load(&self) -> Result<Vec<EventEnvelope>, String> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Ok(Vec::new()),
        };
        let reader = BufReader::new(file);
        let mut events = Vec::new();
        for (line_number, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| format!("could not read event line: {e}"))?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<EventEnvelope>(&line) {
                Ok(envelope) => events.push(envelope),
                Err(error) => {
                    tracing::warn!(
                        path = %self.path.display(),
                        line = line_number + 1,
                        error = %error,
                        "skipping malformed event line"
                    );
                }
            }
        }
        events.sort_by_key(|e| e.seq);
        Ok(events)
    }

    /// Append a single event atomically-ish: the line is written with
    /// `O_APPEND` and fsynced. A crash between write and fsync may leave a
    /// partial line; readers skip malformed lines.
    pub fn append(&self, event: SessionEvent) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let next_seq = self
            .load()?
            .last()
            .map(|e| e.seq + 1)
            .unwrap_or(0);
        let envelope = EventEnvelope {
            seq: next_seq,
            timestamp: crate::session::unix_timestamp(),
            event,
        };
        let line = serde_json::to_vec(&envelope).map_err(|e| e.to_string())?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("could not open event log: {e}"))?;
        file.write_all(&line)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("could not append event: {e}"))?;
        Ok(())
    }

    /// Replace the entire log with the given events. Used when compacting the
    /// log into a seed snapshot or when pruning old events.
    pub fn rewrite(&self, events: Vec<EventEnvelope>) -> Result<(), String> {
        let mut lines = Vec::new();
        for envelope in events {
            let mut line = serde_json::to_vec(&envelope).map_err(|e| e.to_string())?;
            line.push(b'\n');
            lines.extend(line);
        }
        fsutil::atomic_write_bytes(&self.path, &lines)
            .map_err(|e| format!("could not rewrite event log: {e}"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "neenee-events-test-{}", uuid::Uuid::new_v4()));
        let log = EventLog::new(dir.join("events.jsonl"));

        log.append(SessionEvent::Reset {
            id: "a".to_string(),
        })
        .unwrap();
        log.append(SessionEvent::MessagesReplaced {
            messages: vec![neenee_core::Message::new(neenee_core::Role::User, "hi")],
        })
        .unwrap();

        let loaded = log.load().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].seq, 0);
        assert_eq!(loaded[1].seq, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn event_log_skips_malformed_lines() {
        let dir = std::env::temp_dir().join(format!(
            "neenee-events-corrupt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        std::fs::write(
            &path,
            "{\"seq\":0,\"timestamp\":1,\"type\":\"reset\",\"id\":\"x\"}\nnot-json\n",
        )
        .unwrap();
        let log = EventLog::new(path);
        let loaded = log.load().unwrap();
        assert_eq!(loaded.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}

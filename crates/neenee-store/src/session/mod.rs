//! Event-sourced session persistence (ADR-0017 / ADR-0022).
//!
//! Each session is an append-only JSONL event log (`sessions/<id>.jsonl`)
//! plus a JSON snapshot cache (`sessions/<id>.json`) with a CRC32C checksum
//! and a `schema_version` for lazy on-load migration; the log wins on
//! conflict. Large payloads are offloaded to the content-addressed
//! [`crate::blobs::BlobStore`]. Sessions are bucketed per project under
//! `projects/<sha256(cwd)[..16]>/sessions/`. [`SessionStore`] is the facade
//! for load/save/resume/fork and for committing model-context projections
//! (pruning and compaction)
//! checkpoints; it also drives the one-shot legacy layout migrations.

use crate::blobs::BlobStore;
use crate::events::{EventLog, SessionEvent};
use crate::fsutil;
use crate::paths;
use neenee_core::{InjectionKind, InjectionOrigin, Message, Provider, Pursuit, Role};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

// Re-export the cheap context estimators so callers keep using
// `session::estimate_chars` / `session::estimate_tokens`.
pub use neenee_core::{estimate_chars, estimate_tokens};

/// C2 (ADR-0022): added `title` and `title_manual`. C3 (ADR-0032): added
/// `pursuit`. C4 (ADR-0034): added `Message::origin` (`Option<InjectionOrigin>`)
/// for structured injection provenance. All three are structural no-ops for
/// legacy snapshots, which load with the new fields at their `#[serde(default)]`
/// values (`None` / `false`).
const CURRENT_SCHEMA_VERSION: u32 = 4;

/// Sentinel value for `PursuitCheckpoint::max_iterations` indicating an uncapped
/// run. `/pursue` runs until the model emits the completion marker, the user
/// runs `/pursue stop`, an error aborts the pursuit, or a newer request
/// supersedes it. Stored on the checkpoint so legacy snapshots that carry a
/// finite `max_iterations` from pre-ADR-0009 versions still load cleanly.
pub const UNCAPPED_ITERATIONS: usize = usize::MAX;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PursuitCheckpoint {
    // Serde key kept as `goal` so pre-rename session snapshots still load.
    #[serde(rename = "goal")]
    pub pursuit: String,
    pub iteration: usize,
    pub max_iterations: usize,
    pub status: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContextProjectionKind {
    /// Legacy snapshots/events did not record whether the projection was prune
    /// or compact. Keep that uncertainty explicit instead of guessing on load.
    #[default]
    Unknown,
    Prune,
    Compact,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextProjectionCheckpoint {
    #[serde(default)]
    pub operation: ContextProjectionKind,
    pub archived_messages: usize,
    pub active_messages: usize,
    pub before_chars: usize,
    pub after_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct SessionData {
    id: String,
    parent_id: Option<String>,
    created_at: u64,
    updated_at: u64,
    #[serde(rename = "model_window", alias = "messages")]
    model_window: Vec<Message>,
    #[serde(rename = "archived_transcript", alias = "archived_messages")]
    archived_transcript: Vec<Message>,
    loop_checkpoint: Option<PursuitCheckpoint>,
    /// Stats of the most recent model-context projection (prune or compaction).
    /// `alias` keeps snapshots written before the rename loadable.
    #[serde(
        rename = "last_projection",
        alias = "last_relief",
        alias = "compaction"
    )]
    last_projection: Option<ContextProjectionCheckpoint>,
    /// Working directory this session belongs to. Phase 2 (project isolation)
    /// uses this to route archived sessions to the right per-project bucket
    /// during the one-shot legacy migration. Legacy snapshots missing the
    /// field default to the current cwd.
    project_root: PathBuf,
    /// Unified task list, mirrored from `Agent::todos`. The single source of
    /// truth for "what is left to do." An empty list means
    /// no active task list. `#[serde(default)]` so legacy snapshots load as
    /// an empty list with no migration.
    #[serde(default)]
    todos: neenee_core::TodoList,
    /// Schema version of this session file. Migrations increment this and are
    /// applied lazily on load.
    schema_version: u32,
    /// CRC32C checksum of the canonical JSON payload (excluding this field).
    /// `None` for legacy files written before C10; new writes always populate
    /// it so `neenee doctor` and future loaders can detect corruption.
    checksum: Option<u32>,
    /// AI-generated session title (ADR-0022). Displayed in the session picker
    /// in preference to the first-user-message fallback. `None` for legacy
    /// snapshots and for sessions that have not yet generated a title.
    #[serde(default)]
    title: Option<String>,
    /// Whether `title` was set manually via `/title <text>` and must not be
    /// overwritten by automatic or on-demand AI generation (ADR-0022).
    /// `false` for legacy snapshots and AI-generated titles.
    #[serde(default)]
    title_manual: bool,
    /// The durable per-session pursuit (ADR-0032). `None` means no pursuit is
    /// set. Mirrors the runtime [`neenee_core::Pursuit`] carried by
    /// `crate::pursuit_state::PursuitState` (the in-memory stop-gate view);
    /// this field is the durable authority read on resume and written by the
    /// `/pursue` slash command and the harness completion path. `#[serde(default)]`
    /// so legacy snapshots load with `pursuit = None` and no migration is needed.
    #[serde(default)]
    pursuit: Option<Pursuit>,
}

impl Default for SessionData {
    fn default() -> Self {
        let now = unix_timestamp();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: None,
            created_at: now,
            updated_at: now,
            model_window: Vec::new(),
            archived_transcript: Vec::new(),
            loop_checkpoint: None,
            last_projection: None,
            project_root: default_project_root(),
            todos: neenee_core::TodoList::default(),
            schema_version: CURRENT_SCHEMA_VERSION,
            checksum: None,
            title: None,
            title_manual: false,
            pursuit: None,
        }
    }
}

/// Serde default for [`SessionData::project_root`]. Resolves to the current
/// process cwd so legacy snapshots (which predate the field) load with the
/// closest-to-correct project binding on first deserialisation.
fn default_project_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Apply one-shot schema migrations to a [`SessionData`] loaded from disk.
/// Each migration is guarded by the incoming `schema_version` so repeated
/// calls are idempotent. The returned value always has
/// `schema_version == CURRENT_SCHEMA_VERSION`.
fn migrate_session_data(mut data: SessionData) -> SessionData {
    // C8: initial schema-version field. No structural migration required yet;
    // future changes add guarded blocks here.
    // C2 (ADR-0022): title fields were added with `#[serde(default)]`, so a
    // legacy snapshot already loads with `title = None` / `title_manual =
    // false`; no payload transformation is needed, only the version bump.
    // C3 (ADR-0032): `pursuit` was added with `#[serde(default)]`, so a legacy
    // snapshot already loads with `pursuit = None`; no payload transformation
    // is needed, only the version bump.
    // C4 (ADR-0034): `Message::origin` (`Option<InjectionOrigin>`) was added
    // with `#[serde(default, skip_serializing_if = "Option::is_none")]`, so a
    // legacy snapshot and event-log lines already load with `origin = None`
    // for every message; no payload transformation is needed, only the version
    // bump. Provenance is henceforth stamped at each injection site going
    // forward — pre-C4 messages are simply unattributed.
    data.schema_version = CURRENT_SCHEMA_VERSION;
    data
}

/// Compute the CRC32C checksum that should be stored for `data`. The checksum
/// covers the canonical JSON representation of all fields except `checksum`,
/// which is set to `null` during computation so later verification can read
/// the stored value and compare against the same payload.
fn compute_checksum(data: &SessionData) -> u32 {
    let mut value = match serde_json::to_value(data) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert("checksum".to_string(), serde_json::Value::Null);
    }
    let bytes = match serde_json::to_vec(&value) {
        Ok(b) => b,
        Err(_) => return 0,
    };
    crc32c::crc32c(&bytes)
}

/// Verify the stored checksum on `data`, if present. Returns `Ok(())` when the
/// checksum matches or when the file predates checksums. Returns an error
/// describing the mismatch otherwise.
fn verify_checksum(data: &SessionData) -> Result<(), String> {
    let Some(stored) = data.checksum else {
        return Ok(());
    };
    let expected = compute_checksum(data);
    if expected == stored {
        Ok(())
    } else {
        Err(format!(
            "checksum mismatch: stored {stored:#010x}, computed {expected:#010x}"
        ))
    }
}

/// Characters above which a message content is moved to the blob store.
const BLOB_OFFLOAD_THRESHOLD: usize = 4_096;

/// Write `data` to `path` with a freshly computed checksum, offloading large
/// inline content to the blob store before serialization.
fn write_session_file(
    path: &Path,
    data: &SessionData,
    blob_store: &BlobStore,
) -> Result<(), String> {
    let mut data = data.clone();
    offload_session_blobs(&mut data, blob_store)?;
    data.checksum = Some(compute_checksum(&data));
    fsutil::atomic_write_json(path, &data)
}

/// Move large `Message.content` strings into the blob store and replace them
/// with a `content_blob` reference. Operates recursively on nested children.
fn offload_session_blobs(data: &mut SessionData, blob_store: &BlobStore) -> Result<(), String> {
    for message in data
        .model_window
        .iter_mut()
        .chain(data.archived_transcript.iter_mut())
    {
        offload_message_blobs(message, blob_store)?;
    }
    Ok(())
}

fn offload_message_blobs(message: &mut Message, blob_store: &BlobStore) -> Result<(), String> {
    if message.content.len() > BLOB_OFFLOAD_THRESHOLD && message.content_blob.is_none() {
        let hash = blob_store.put(message.content.as_bytes())?;
        message.content_blob = Some(hash);
        message.content.clear();
    }
    if let Some(children) = message.children.as_mut() {
        for child in children.iter_mut() {
            offload_message_blobs(child, blob_store)?;
        }
    }
    Ok(())
}

/// Rehydrate `content` from `content_blob` references after loading.
fn load_session_blobs(data: &mut SessionData, blob_store: &BlobStore) -> Result<(), String> {
    for message in data
        .model_window
        .iter_mut()
        .chain(data.archived_transcript.iter_mut())
    {
        load_message_blobs(message, blob_store)?;
    }
    Ok(())
}

fn load_message_blobs(message: &mut Message, blob_store: &BlobStore) -> Result<(), String> {
    if let Some(hash) = message.content_blob.take() {
        let bytes = blob_store
            .get(&hash)
            .ok_or_else(|| format!("missing content blob {hash}"))?;
        message.content = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    }
    if let Some(children) = message.children.as_mut() {
        for child in children.iter_mut() {
            load_message_blobs(child, blob_store)?;
        }
    }
    Ok(())
}

/// Emit a [`SessionEvent::Started`] event if the log is currently empty.
/// Every session must begin with this event so replay reconstructs the id,
/// parent link, and timestamps.
fn ensure_event_log_started(event_log: &EventLog, data: &SessionData) -> Result<(), String> {
    if event_log.load()?.is_empty() {
        event_log.append(SessionEvent::Started {
            id: data.id.clone(),
            parent_id: data.parent_id.clone(),
            created_at: data.created_at,
            project_root: data.project_root.clone(),
            schema_version: data.schema_version,
        })?;
    }
    Ok(())
}

/// Apply a sequence of events to a fresh or existing [`SessionData`].
fn apply_events(data: &mut SessionData, envelopes: &[crate::events::EventEnvelope]) {
    for envelope in envelopes {
        match &envelope.event {
            SessionEvent::Started {
                id,
                parent_id,
                created_at,
                project_root,
                schema_version,
            } => {
                data.id = id.clone();
                data.parent_id = parent_id.clone();
                data.created_at = *created_at;
                data.project_root = project_root.clone();
                data.schema_version = *schema_version;
            }
            SessionEvent::MessagesReplaced { messages } => data.model_window = messages.clone(),
            SessionEvent::MessagesAppended { messages } => {
                data.model_window.extend(messages.clone())
            }
            SessionEvent::CheckpointSet { checkpoint } => data.loop_checkpoint = checkpoint.clone(),
            SessionEvent::ContextProjectionCommitted {
                archived_originals,
                model_window,
                checkpoint,
            } => {
                data.archived_transcript.extend(archived_originals.clone());
                data.model_window = model_window.clone();
                data.last_projection = Some(checkpoint.clone());
            }
            SessionEvent::Archived { messages } => {
                data.archived_transcript.extend(messages.clone())
            }
            SessionEvent::TodosSet { todos } => {
                data.todos = todos.clone();
            }
            SessionEvent::TitleSet { title, manual } => {
                data.title = title.clone();
                data.title_manual = *manual;
            }
            SessionEvent::PursuitSet { pursuit } => {
                data.pursuit = pursuit.clone();
            }
            SessionEvent::Reset { id } => {
                let project_root = data.project_root.clone();
                let schema_version = data.schema_version;
                *data = SessionData::default();
                data.id = id.clone();
                data.project_root = project_root;
                data.schema_version = schema_version;
            }
            SessionEvent::Forked { id, parent_id } => {
                data.id = id.clone();
                data.parent_id = Some(parent_id.clone());
                data.loop_checkpoint = None;
                // A forked side session starts without the parent's pursuit
                // (ADR-0032). The old per-thread store keyed by session id, so
                // a fresh id had no pursuit row; the session field mirrors that.
                data.pursuit = None;
            }
        }
        data.updated_at = envelope.timestamp;
    }
}

/// Convert a snapshot into a seed event sequence so legacy files can be
/// imported into the event log without losing information.
fn snapshot_to_events(data: &SessionData) -> Vec<crate::events::EventEnvelope> {
    let mut events = vec![crate::events::EventEnvelope {
        seq: 0,
        timestamp: data.created_at,
        event: SessionEvent::Started {
            id: data.id.clone(),
            parent_id: data.parent_id.clone(),
            created_at: data.created_at,
            project_root: data.project_root.clone(),
            schema_version: data.schema_version,
        },
    }];
    if let Some(checkpoint) = &data.last_projection {
        events.push(crate::events::EventEnvelope {
            seq: events.len() as u64,
            timestamp: data.updated_at,
            event: SessionEvent::ContextProjectionCommitted {
                archived_originals: data.archived_transcript.clone(),
                model_window: data.model_window.clone(),
                checkpoint: checkpoint.clone(),
            },
        });
    } else {
        if !data.archived_transcript.is_empty() {
            events.push(crate::events::EventEnvelope {
                seq: events.len() as u64,
                timestamp: data.updated_at,
                event: SessionEvent::Archived {
                    messages: data.archived_transcript.clone(),
                },
            });
        }
        events.push(crate::events::EventEnvelope {
            seq: events.len() as u64,
            timestamp: data.updated_at,
            event: SessionEvent::MessagesReplaced {
                messages: data.model_window.clone(),
            },
        });
    }
    if let Some(checkpoint) = &data.loop_checkpoint {
        events.push(crate::events::EventEnvelope {
            seq: events.len() as u64,
            timestamp: data.updated_at,
            event: SessionEvent::CheckpointSet {
                checkpoint: Some(checkpoint.clone()),
            },
        });
    }
    if !data.todos.is_empty() {
        events.push(crate::events::EventEnvelope {
            seq: events.len() as u64,
            timestamp: data.updated_at,
            event: SessionEvent::TodosSet {
                todos: data.todos.clone(),
            },
        });
    }
    if data.title.is_some() {
        events.push(crate::events::EventEnvelope {
            seq: events.len() as u64,
            timestamp: data.updated_at,
            event: SessionEvent::TitleSet {
                title: data.title.clone(),
                manual: data.title_manual,
            },
        });
    }
    if let Some(pursuit) = &data.pursuit {
        events.push(crate::events::EventEnvelope {
            seq: events.len() as u64,
            timestamp: data.updated_at,
            event: SessionEvent::PursuitSet {
                pursuit: Some(pursuit.clone()),
            },
        });
    }
    events
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub parent_id: Option<String>,
    pub message_count: usize,
    pub updated_at: u64,
    pub created_at: u64,
    /// Short description of what the session is about (first user message or
    /// the active pursuit), already truncated for display.
    pub overview: String,
    pub active: bool,
}

/// The mutable bits a [`SessionStore`] pins to one session file: the snapshot
/// path, its event log, and the in-memory session data. Grouped under a single
/// [`tokio::sync::Mutex`] so repointing the store (reset / fork / open) — which
/// swaps both the path and the event log — is atomic with respect to every
/// reader and writer. There is no second lock to deadlock against.
struct SessionState {
    /// Absolute path of this session's snapshot: `<sessions_dir>/<id>.json`.
    path: PathBuf,
    /// This session's append-only event log at `<sessions_dir>/<id>.jsonl`.
    event_log: EventLog,
    /// In-memory session, authoritative between writes; the event log is the
    /// durable authority across restarts.
    data: SessionData,
}

pub struct SessionStore {
    project_root: PathBuf,
    /// Directory holding every session file for this project (or, for
    /// [`SessionStore::for_path`], the parent of the pinned snapshot). All
    /// `reset` / `fork` / `open` targets live here, so the store never writes
    /// outside it.
    sessions_dir: PathBuf,
    blob_store: BlobStore,
    state: Mutex<SessionState>,
}

impl SessionStore {
    /// Open a per-project store pinned to a **fresh** session file.
    ///
    /// As of ADR-0018 the project bucket no longer keeps a single shared
    /// `session.json` "active pointer": every running `neenee` instance mints
    /// its own `sessions/<id>.json` + `sessions/<id>.jsonl`, so two instances
    /// in the same project never share a mutable file. To continue a previous
    /// session the caller picks one via the `/sessions` picker or
    /// [`Self::open`] / [`Self::resume`].
    pub fn load_for_project(project_root: PathBuf) -> Self {
        let dirs = paths::get();
        let sessions_dir = dirs.project_sessions_dir(&project_root);
        if let Err(e) = std::fs::create_dir_all(&sessions_dir) {
            tracing::warn!(error = %e, "could not create project sessions dir");
        }
        let blob_store = BlobStore::new(dirs.blobs_dir());

        Self::pin_fresh(project_root, sessions_dir, blob_store)
    }

    /// Backwards-compatible alias for [`Self::load_for_project`] using the
    /// current process cwd.
    #[allow(dead_code)]
    pub fn load() -> Self {
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::load_for_project(project_root)
    }

    /// Open a `SessionStore` pinned to an explicit snapshot `path`. The
    /// session's event log lives at `path.with_extension("jsonl")`, and its
    /// sibling session files (forks, archives) live in `path.parent()` — i.e.
    /// the parent directory plays the role of the project's `sessions/` dir.
    ///
    /// This is the low-level constructor used by envoys / side
    /// conversations (ADR-0017) and by tests that want a throwaway file
    /// without wiring up the global paths table. Production startup uses
    /// [`Self::load_for_project`].
    pub fn for_path(path: PathBuf) -> Self {
        let sessions_dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let event_log_path = path.with_extension("jsonl");
        let project_root = sessions_dir.clone();
        let blob_store = BlobStore::new(sessions_dir.join("blobs"));
        let data = load_or_seed(&path, &event_log_path, &blob_store, &project_root);
        let event_log = EventLog::new(event_log_path);
        Self {
            project_root,
            sessions_dir,
            blob_store,
            state: Mutex::new(SessionState {
                path,
                event_log,
                data,
            }),
        }
    }

    /// Construct a store pinned to a brand-new, empty session file in
    /// `sessions_dir`. The file is **not** written until the session gains
    /// real content, so a `neenee` that starts and exits without a turn
    /// leaves no empty-file litter behind.
    fn pin_fresh(project_root: PathBuf, sessions_dir: PathBuf, blob_store: BlobStore) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let path = sessions_dir.join(format!("{id}.json"));
        let event_log = EventLog::new(path.with_extension("jsonl"));
        let data = SessionData {
            id,
            project_root: project_root.clone(),
            ..Default::default()
        };
        Self {
            project_root,
            sessions_dir,
            blob_store,
            state: Mutex::new(SessionState {
                path,
                event_log,
                data,
            }),
        }
    }

    /// Project root this store is bound to.
    #[allow(dead_code)]
    pub fn project_root(&self) -> &std::path::Path {
        &self.project_root
    }

    pub async fn id(&self) -> String {
        self.state.lock().await.data.id.clone()
    }

    pub async fn model_window(&self) -> Vec<Message> {
        self.state.lock().await.data.model_window.clone()
    }

    pub async fn full_transcript(&self) -> Vec<Message> {
        let state = self.state.lock().await;
        let mut messages = state.data.archived_transcript.clone();
        messages.extend(state.data.model_window.clone());
        messages
    }

    pub async fn checkpoint(&self) -> Option<PursuitCheckpoint> {
        self.state.lock().await.data.loop_checkpoint.clone()
    }

    /// The unified task list, mirrored from `Agent::todos`. Empty means no
    /// active task list. Read on resume to seed the agent and the sticky
    /// panel.
    pub async fn todos(&self) -> neenee_core::TodoList {
        self.state.lock().await.data.todos.clone()
    }

    /// Replace the task list. Persists both the snapshot and the event log so
    /// resume restores the same list (and so per-item history is retained in
    /// the log).
    pub async fn set_todos(&self, todos: neenee_core::TodoList) -> Result<(), String> {
        let mut state = self.state.lock().await;
        state.data.todos = todos.clone();
        state.data.updated_at = unix_timestamp();
        ensure_event_log_started(&state.event_log, &state.data)?;
        state.event_log.append(SessionEvent::TodosSet { todos })?;
        self.persist_locked(&state)
    }

    pub async fn last_projection(&self) -> Option<ContextProjectionCheckpoint> {
        self.state.lock().await.data.last_projection.clone()
    }

    /// The current session title and whether it was manually set (ADR-0022).
    /// `(None, false)` for a session that has not yet generated a title; the
    /// caller then falls back to the first-user-message overview. A `true`
    /// `manual` flag means automatic and on-demand AI generation must not
    /// overwrite the stored title.
    pub async fn title(&self) -> (Option<String>, bool) {
        let state = self.state.lock().await;
        (state.data.title.clone(), state.data.title_manual)
    }

    /// Replace the session title. `manual = true` marks a user-set title
    /// (`/title <text>`) that AI generation will not overwrite; the AI runner
    /// and on-demand refresh always pass `false`. Pass `title = None` with
    /// `manual = false` to clear. Persists both the snapshot and the event log
    /// so resume restores the same title.
    pub async fn set_title(&self, title: Option<String>, manual: bool) -> Result<(), String> {
        let mut state = self.state.lock().await;
        state.data.title = title.clone();
        state.data.title_manual = manual;
        state.data.updated_at = unix_timestamp();
        ensure_event_log_started(&state.event_log, &state.data)?;
        state
            .event_log
            .append(SessionEvent::TitleSet { title, manual })?;
        self.persist_locked(&state)
    }

    pub async fn archived_transcript_count(&self) -> usize {
        self.state.lock().await.data.archived_transcript.len()
    }

    pub async fn parent_id(&self) -> Option<String> {
        self.state.lock().await.data.parent_id.clone()
    }

    /// The durable per-session pursuit (ADR-0032). `None` means no pursuit is
    /// set. Read on resume to seed the agent's runtime `PursuitState` and by
    /// `/pursue status` / `/pursue` (empty, re-arm).
    pub async fn pursuit(&self) -> Option<Pursuit> {
        self.state.lock().await.data.pursuit.clone()
    }

    /// Replace the pursuit (or clear it with `None`). Persists both the
    /// snapshot and the event log so resume restores the same pursuit. The
    /// single write path for the pursuit primitive; `mark_pursuit_complete`
    /// and `update_pursuit_objective` delegate here.
    pub async fn set_pursuit(&self, pursuit: Option<Pursuit>) -> Result<(), String> {
        let mut state = self.state.lock().await;
        state.data.pursuit = pursuit.clone();
        state.data.updated_at = unix_timestamp();
        ensure_event_log_started(&state.event_log, &state.data)?;
        state
            .event_log
            .append(SessionEvent::PursuitSet { pursuit })?;
        self.persist_locked(&state)
    }

    /// Flip `is_complete = true` on the current pursuit, if any. Returns the
    /// updated pursuit, or `None` if no pursuit was set. Called by the harness
    /// completion path after the model emits `[NEENEE_PURSUIT_COMPLETE]` and by
    /// the `/pursue done` slash command.
    pub async fn mark_pursuit_complete(&self) -> Result<Option<Pursuit>, String> {
        let current = self.pursuit().await;
        if let Some(mut pursuit) = current {
            pursuit.is_complete = true;
            self.set_pursuit(Some(pursuit.clone())).await?;
            Ok(Some(pursuit))
        } else {
            Ok(None)
        }
    }

    /// Rewrite the objective on the current pursuit, if any. Returns the
    /// updated pursuit, or `None` if no pursuit was set. Called by the
    /// `/pursue edit` slash command.
    pub async fn update_pursuit_objective(
        &self,
        objective: &str,
    ) -> Result<Option<Pursuit>, String> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("pursuit objective must not be empty".to_string());
        }
        if objective.chars().count() > 4000 {
            return Err("pursuit objective must be at most 4000 characters".to_string());
        }
        let current = self.pursuit().await;
        if let Some(mut pursuit) = current {
            pursuit.objective = objective.to_string();
            self.set_pursuit(Some(pursuit.clone())).await?;
            Ok(Some(pursuit))
        } else {
            Ok(None)
        }
    }

    pub async fn replace_messages(&self, messages: Vec<Message>) -> Result<(), String> {
        let mut state = self.state.lock().await;
        state.data.model_window = messages;
        state.data.updated_at = unix_timestamp();
        ensure_event_log_started(&state.event_log, &state.data)?;
        state.event_log.append(SessionEvent::MessagesReplaced {
            messages: state.data.model_window.clone(),
        })?;
        self.persist_locked(&state)
    }

    /// Incrementally persist new messages appended since the last durable
    /// write, without rewriting the full snapshot (ADR-0035).
    ///
    /// The caller passes the *current full* turn history. This method diffs it
    /// against the messages already durable in `data.model_window` and appends only
    /// the tail as a `MessagesAppended` event to the append-only log — O(delta),
    /// not O(history). The snapshot cache (`session.json`) is intentionally
    /// **not** rewritten here: it stays at the last turn boundary and is
    /// refreshed by `replace_messages` at turn end. On resume, `load_or_seed`
    /// replays the log (authoritative), so the appended tail is recovered.
    ///
    /// This is the mid-turn save point: a crash after a side-effecting tool
    /// call leaves the transcript in sync with the filesystem instead of
    /// rewinding to the previous turn. If `current` is no longer than the
    /// durable prefix (e.g. a compaction already replaced messages) this is a
    /// no-op.
    pub async fn append_round(&self, current: &[Message]) -> Result<(), String> {
        let mut state = self.state.lock().await;
        let baseline = state.data.model_window.len();
        // Only the strictly-new tail is the delta. If `current` is shorter or
        // equal (compaction rewrote the window, or nothing changed), there is
        // nothing to append.
        let delta: Vec<Message> = if current.len() > baseline {
            // Guard against a divergent history: the durable prefix must match
            // the incoming prefix. A mismatch means the caller and the store
            // disagree on state (a bug, or a compaction rewrote the window
            // without going through `replace_messages`); fall back to a full
            // replace so the log never records a corrupt splice. `Message` has
            // no `PartialEq`, so we compare the identity-bearing fields.
            let diverged = current[..baseline]
                .iter()
                .zip(state.data.model_window[..].iter())
                .any(|(incoming, durable)| {
                    incoming.role != durable.role
                        || incoming.content != durable.content
                        || incoming.tool_call_id != durable.tool_call_id
                });
            if diverged {
                tracing::warn!(
                    baseline,
                    incoming = current.len(),
                    "append_round: incoming prefix diverged from durable state; full replace"
                );
                state.data.model_window = current.to_vec();
                state.data.updated_at = unix_timestamp();
                ensure_event_log_started(&state.event_log, &state.data)?;
                state.event_log.append(SessionEvent::MessagesReplaced {
                    messages: state.data.model_window.clone(),
                })?;
                return self.persist_locked(&state);
            }
            current[baseline..].to_vec()
        } else {
            return Ok(());
        };
        // Advance the in-memory state and append the delta event. The snapshot
        // cache is not touched (stays at the turn boundary).
        state.data.model_window.extend(delta.clone());
        state.data.updated_at = unix_timestamp();
        ensure_event_log_started(&state.event_log, &state.data)?;
        state
            .event_log
            .append(SessionEvent::MessagesAppended { messages: delta })?;
        Ok(())
    }

    pub async fn set_checkpoint(
        &self,
        checkpoint: Option<PursuitCheckpoint>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().await;
        state.data.loop_checkpoint = checkpoint;
        state.data.updated_at = unix_timestamp();
        ensure_event_log_started(&state.event_log, &state.data)?;
        state.event_log.append(SessionEvent::CheckpointSet {
            checkpoint: state.data.loop_checkpoint.clone(),
        })?;
        self.persist_locked(&state)
    }

    pub async fn commit_context_projection(
        &self,
        result: ContextProjectionResult,
    ) -> Result<(), String> {
        let mut state = self.state.lock().await;
        state
            .data
            .archived_transcript
            .extend(result.archived_originals.clone());
        state.data.model_window = result.model_window.clone();
        state.data.last_projection = Some(result.checkpoint.clone());
        state.data.updated_at = unix_timestamp();
        ensure_event_log_started(&state.event_log, &state.data)?;
        state
            .event_log
            .append(SessionEvent::ContextProjectionCommitted {
                archived_originals: result.archived_originals,
                model_window: result.model_window,
                checkpoint: result.checkpoint,
            })?;
        self.persist_locked(&state)
    }

    /// Start a brand-new session and repoint this store at it. The previous
    /// session's file is left intact on disk (it was already persisted on
    /// every mutation) and stays reachable through [`Self::list`] /
    /// [`Self::resume`]. Returns the new session id.
    ///
    /// Under ADR-0018 this no longer mutates a shared "active" file: it simply
    /// mints a new `sessions/<id>.{json,jsonl}` and switches this process to
    /// writing it, so a concurrent instance cannot clobber the previous
    /// session.
    pub async fn reset(&self) -> Result<String, String> {
        let project_root = self.project_root.clone();
        let mut state = self.state.lock().await;
        let sessions_dir = self.sessions_dir.clone();
        let id = uuid::Uuid::new_v4().to_string();
        let path = sessions_dir.join(format!("{id}.json"));
        let event_log = EventLog::new(path.with_extension("jsonl"));
        let data = SessionData {
            project_root,
            ..Default::default()
        };
        ensure_event_log_started(&event_log, &data)?;
        state.path = path;
        state.event_log = event_log;
        state.data = data;
        // Do not persist an empty snapshot — a session that never gains
        // content leaves no empty-file litter (see ADR-0018).
        Ok(id)
    }

    pub async fn resume(&self, id: Option<&str>) -> Result<String, String> {
        let target = match id {
            Some(id) => id.to_string(),
            None => self
                .list()
                .await?
                .into_iter()
                .find(|session| !session.active && session.message_count > 0)
                .map(|session| session.id)
                .ok_or_else(|| "No previous session is available to resume.".to_string())?,
        };
        self.open(&target).await?;
        Ok(self.state.lock().await.data.id.clone())
    }

    /// Fork the current session: write its state to a new child file and
    /// repoint this store at the child. The parent's file is untouched
    /// (already current) and remains reachable. Returns `(child_id,
    /// parent_id)`.
    pub async fn fork(&self) -> Result<(String, String), String> {
        let mut state = self.state.lock().await;
        if state.data.model_window.is_empty() && state.data.archived_transcript.is_empty() {
            return Err("Cannot fork an empty session.".to_string());
        }
        let parent_id = state.data.id.clone();
        let now = unix_timestamp();

        // Build the child snapshot from the parent's current state.
        let mut child = state.data.clone();
        let fork_id = uuid::Uuid::new_v4().to_string();
        child.id = fork_id.clone();
        child.parent_id = Some(parent_id.clone());
        child.created_at = now;
        child.updated_at = now;
        child.loop_checkpoint = None;

        let child_path = self.sessions_dir.join(format!("{fork_id}.json"));
        let child_log = EventLog::new(child_path.with_extension("jsonl"));
        // Persist the child snapshot and seed its own event log so it is
        // independently resumable (one store = one file = one log).
        persist_to(&child_path, &child, &self.blob_store)?;
        let _ = child_log.rewrite(snapshot_to_events(&child));

        // Repoint this store at the child; the parent file is already current.
        state.path = child_path;
        state.event_log = child_log;
        state.data = child;
        Ok((fork_id, parent_id))
    }

    /// Fork the current session into a **self-contained side file** without
    /// disturbing this store's active pointer (ADR-0017). Unlike `fork`,
    /// the primary keeps running: this method only *reads* the current
    /// snapshot, writes a sibling `sessions/<side_id>.{json,jsonl}`, and
    /// returns `(side_id, parent_id)`. The primary's `state` is left
    /// untouched, so a concurrent parent turn is not clobbered.
    ///
    /// Load the side into its own live store with `open_side`.
    pub async fn fork_to_side(&self) -> Result<(String, String), String> {
        let state = self.state.lock().await;
        if state.data.model_window.is_empty() && state.data.archived_transcript.is_empty() {
            return Err("Cannot fork an empty session.".to_string());
        }
        let parent_id = state.data.id.clone();
        let now = unix_timestamp();

        // Build the side snapshot from the primary's current state.
        let mut side = state.data.clone();
        let side_id = uuid::Uuid::new_v4().to_string();
        side.id = side_id.clone();
        side.parent_id = Some(parent_id.clone());
        side.created_at = now;
        side.updated_at = now;
        side.loop_checkpoint = None;

        let side_path = self.sessions_dir.join(format!("{side_id}.json"));
        let side_log = EventLog::new(side_path.with_extension("jsonl"));
        // Persist the side snapshot and seed its own event log so it is
        // independently resumable (one store = one file = one log), exactly
        // like `fork`. The primary's files are never touched.
        persist_to(&side_path, &side, &self.blob_store)?;
        let _ = side_log.rewrite(snapshot_to_events(&side));

        // Deliberately do NOT mutate `state` — the primary keeps its active
        // pointer, history, and in-flight turn intact.
        Ok((side_id, parent_id))
    }

    /// Construct a live [`SessionStore`] pinned to a side session file that
    /// lives in this store's `sessions_dir` (written by `fork_to_side`). The
    /// returned store shares the primary's project root, sessions dir, and blob
    /// store root, so inherited content (including image blobs) resolves the
    /// same way as in the primary. It writes only its own `sessions/<id>.*`
    /// files, so the two stores never race on the same file.
    pub async fn open_side(&self, side_id: &str) -> Result<SessionStore, String> {
        let side_path = self.sessions_dir.join(format!("{side_id}.json"));
        if !side_path.exists() {
            return Err(format!("Side session '{side_id}' was not found."));
        }
        let event_log_path = side_path.with_extension("jsonl");
        let project_root = self.project_root.clone();
        let blob_store = BlobStore::new(self.blob_store.root().to_path_buf());
        let data = load_or_seed(&side_path, &event_log_path, &blob_store, &project_root);
        let event_log = EventLog::new(event_log_path);
        Ok(SessionStore {
            project_root,
            sessions_dir: self.sessions_dir.clone(),
            blob_store,
            state: Mutex::new(SessionState {
                path: side_path,
                event_log,
                data,
            }),
        })
    }

    /// Switch this store to an existing session file by id (or 4+-char hex
    /// prefix). The session's state is reloaded from its own event log (the
    /// durable authority), so `open` always reflects the latest on-disk
    /// content — even if another process wrote it.
    pub async fn open(&self, id: &str) -> Result<(), String> {
        let (resolved, path) = {
            let state = self.state.lock().await;
            self.resolve_session(id, &state)?
        };
        // No-op when the caller asks for the session we already hold.
        let already_active = self.state.lock().await.data.id == resolved;
        if already_active {
            return Ok(());
        }

        let event_log_path = path.with_extension("jsonl");
        let project_root = self.project_root.clone();
        let data = load_or_seed(&path, &event_log_path, &self.blob_store, &project_root);
        let mut state = self.state.lock().await;
        state.path = path;
        state.event_log = EventLog::new(event_log_path);
        state.data = data;
        Ok(())
    }

    /// Delete a session by id or short id prefix. Deleting the active session
    /// removes its snapshot and event log, then repoints the store at a fresh
    /// empty session; other sessions just have their two files removed from
    /// the sessions directory.
    pub async fn delete(&self, id: &str) -> Result<(), String> {
        let (resolved, snapshot, is_active) = {
            let state = self.state.lock().await;
            let (resolved, path) = self.resolve_session(id, &state)?;
            (resolved.clone(), path, state.data.id == resolved)
        };

        let log = snapshot.with_extension("jsonl");
        let existed = snapshot.exists() || log.exists();
        let _ = fs::remove_file(&snapshot);
        let _ = fs::remove_file(&log);

        if !existed {
            return Err(format!(
                "Could not delete session '{}': files not found.",
                resolved
            ));
        }
        // Repoint at a fresh session so the store stays usable after the
        // active session is removed.
        if is_active {
            self.reset().await?;
        }
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<SessionSummary>, String> {
        let active_id = self.state.lock().await.data.id.clone();
        let mut summaries = Vec::new();
        if self.sessions_dir.exists() {
            for entry in fs::read_dir(&self.sessions_dir).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let Ok(content) = fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(session) = serde_json::from_str::<SessionData>(&content) else {
                    continue;
                };
                summaries.push(summary(&session, session.id == active_id));
            }
        }
        summaries.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
        Ok(summaries)
    }

    /// Persist the in-memory state to its pinned snapshot path. Assumes the
    /// caller already holds the state lock.
    fn persist_locked(&self, state: &SessionState) -> Result<(), String> {
        persist_to(&state.path, &state.data, &self.blob_store)
    }

    /// Write `data` to `sessions_dir/<data.id>.json`. Used to materialise a
    /// session file for a snapshot that is not (or not yet) the pinned one —
    /// for example seeding an archived branch in tests.
    #[allow(dead_code)]
    fn persist_archive(&self, data: &SessionData) -> Result<(), String> {
        let path = self.sessions_dir.join(format!("{}.json", data.id));
        persist_to(&path, data, &self.blob_store)
    }

    /// Resolve `input` (a 4+ char hex id or prefix) to the full session id
    /// **and the file path** that holds it. Identity is matched against the
    /// `id` field stored inside each snapshot, not the filename, so a session
    /// pinned via [`SessionStore::for_path`] under an arbitrary name (e.g. a
    /// test's `session.json`, or a not-yet-migrated legacy active file) is
    /// found just as reliably as a canonical `sessions/<id>.json`. The active
    /// session is matched against its in-memory id first so a prefix of the
    /// current session resolves without touching disk.
    fn resolve_session(
        &self,
        input: &str,
        active: &SessionState,
    ) -> Result<(String, PathBuf), String> {
        if input.len() < 4
            || !input
                .chars()
                .all(|character| character.is_ascii_hexdigit() || character == '-')
        {
            return Err(format!(
                "Invalid session id prefix '{}'. Use at least 4 hexadecimal characters.",
                input
            ));
        }
        let mut matches: Vec<(String, PathBuf)> = Vec::new();
        if active.data.id.starts_with(input) {
            matches.push((active.data.id.clone(), active.path.clone()));
        }
        if self.sessions_dir.exists() {
            for entry in fs::read_dir(&self.sessions_dir).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let Ok(content) = fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(session) = serde_json::from_str::<SessionData>(&content) else {
                    continue;
                };
                if session.id.starts_with(input) && !matches.iter().any(|(id, _)| id == &session.id)
                {
                    matches.push((session.id, path));
                }
            }
        }
        match matches.as_slice() {
            [(id, path)] => Ok((id.clone(), path.clone())),
            [] => Err(format!("No session matches '{}'.", input)),
            _ => Err(format!(
                "Session prefix '{}' is ambiguous ({} matches).",
                input,
                matches.len()
            )),
        }
    }
}

/// Write `data` to `path`, creating its parent directory first.
fn persist_to(path: &Path, data: &SessionData, blob_store: &BlobStore) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    write_session_file(path, data, blob_store)
}

/// Load the session for `path` from its event log when one exists; otherwise
/// import from the snapshot file (seeding a fresh log from it), or start from
/// an empty session when neither exists. This is the single load path shared
/// by [`SessionStore::for_path`] and [`SessionStore::open`], and it also
/// lazily seeds event logs for legacy archived snapshots that predate the
/// per-session log layout (ADR-0018).
fn load_or_seed(
    path: &Path,
    event_log_path: &Path,
    blob_store: &BlobStore,
    project_root: &Path,
) -> SessionData {
    let event_log = EventLog::new(event_log_path.to_path_buf());
    if let Ok(envelopes) = event_log.load()
        && !envelopes.is_empty()
    {
        let mut data = SessionData::default();
        apply_events(&mut data, &envelopes);
        if let Err(error) = load_session_blobs(&mut data, blob_store) {
            tracing::warn!(error = %error, "could not load session blobs from event log");
        }
        if let Err(error) = verify_checksum(&data) {
            tracing::warn!(path = %path.display(), error = %error, "session checksum failed");
        }
        return data;
    }
    // No event log: import from the snapshot, or start fresh.
    let mut data = fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<SessionData>(&content).ok())
        .unwrap_or_else(|| SessionData {
            project_root: project_root.to_path_buf(),
            ..Default::default()
        });
    if let Err(error) = load_session_blobs(&mut data, blob_store) {
        tracing::warn!(error = %error, "could not load session blobs from snapshot");
    }
    if let Err(error) = verify_checksum(&data) {
        tracing::warn!(path = %path.display(), error = %error, "session checksum failed");
    }
    if data.schema_version < CURRENT_SCHEMA_VERSION {
        data = migrate_session_data(data);
    }
    let _ = event_log.rewrite(snapshot_to_events(&data));
    data
}

fn summary(data: &SessionData, active: bool) -> SessionSummary {
    SessionSummary {
        id: data.id.clone(),
        parent_id: data.parent_id.clone(),
        message_count: data.model_window.len() + data.archived_transcript.len(),
        updated_at: data.updated_at,
        created_at: data.created_at,
        overview: session_overview(data),
        active,
    }
}

/// Derive a short, human-readable description of a session. Precedence
/// (ADR-0022): a stored title (AI or manual) wins; otherwise the first user
/// message; then the active pursuit; then a placeholder. A title is already
/// ≤ [`neenee_core::TITLE_MAX_LEN`] chars, so it is returned verbatim; the
/// fallback paths are still truncated to the picker-row budget.
fn session_overview(data: &SessionData) -> String {
    const MAX: usize = 64;
    if let Some(title) = data.title.as_deref().filter(|t| !t.trim().is_empty()) {
        return truncate_preview(title, MAX);
    }
    if let Some(message) = data
        .model_window
        .iter()
        .chain(data.archived_transcript.iter())
        .find(|message| message.role == neenee_core::Role::User)
    {
        return truncate_preview(&message.content, MAX);
    }
    if let Some(checkpoint) = &data.loop_checkpoint {
        return truncate_preview(&checkpoint.pursuit, MAX);
    }
    "(empty session)".to_string()
}

fn truncate_preview(text: &str, max: usize) -> String {
    let text = text.trim();
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let head: String = chars.into_iter().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

pub(crate) fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct ContextProjectionResult {
    pub model_window: Vec<Message>,
    pub archived_originals: Vec<Message>,
    pub checkpoint: ContextProjectionCheckpoint,
}

/// Header prepended to every compaction checkpoint message. Doubles as the
/// classifier that excludes checkpoints from the user-turn count and lets a
/// later compaction extract the previous summary for incremental updates.
const CHECKPOINT_HEADER: &str = "[Conversation checkpoint]\n\
     Earlier complete turns were compacted. Treat this as durable context, not a new user request.\n\n";

/// Per-message excerpt cap used by the deterministic excerpt fallback.
const EXCERPT_CAP: usize = 1_500;

pub struct CompactionSelection {
    /// Older complete turns moved out of the model-visible window.
    pub archived: Vec<Message>,
    /// Recent turns preserved verbatim after the checkpoint.
    pub tail: Vec<Message>,
    /// Body of a prior checkpoint message, when present, fed forward as the
    /// anchored summary so each compaction updates rather than restarts.
    pub previous_summary: Option<String>,
}

/// Split a message list into the archived head and the verbatim tail. Returns
/// `None` when there are not enough complete user turns to compact.
pub fn select_compaction(
    messages: &[Message],
    preserve_turns: usize,
) -> Option<CompactionSelection> {
    let user_indices = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == Role::User && !message.content.starts_with("[Conversation checkpoint]")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if user_indices.len() <= preserve_turns {
        return None;
    }

    let keep_from = user_indices[user_indices.len() - preserve_turns];
    let archived = messages[..keep_from]
        .iter()
        .filter(|message| message.role != Role::System)
        .cloned()
        .collect::<Vec<_>>();
    if archived.is_empty() {
        return None;
    }
    let tail = messages[keep_from..].to_vec();

    // A prior checkpoint message (hidden user, `[Conversation checkpoint]`
    // prefix) carries the previous summary; surface it for incremental updates.
    let previous_summary = messages.iter().rev().find_map(|message| {
        if message.role == Role::User
            && message.hidden
            && message.content.starts_with("[Conversation checkpoint]")
        {
            message
                .content
                .strip_prefix(CHECKPOINT_HEADER)
                .map(|body| body.trim().to_string())
                .filter(|body| !body.is_empty())
        } else {
            None
        }
    });

    Some(CompactionSelection {
        archived,
        tail,
        previous_summary,
    })
}

/// Character budget for the compaction summary, derived from the post-
/// compaction token target. The summary may fill the target (the preserved
/// tail sits alongside it), bounded to a sane range so huge windows do not
/// produce enormous summaries and tiny windows still get a useful digest.
fn summary_char_budget(target_tokens: usize) -> usize {
    (target_tokens * neenee_core::CHARS_PER_TOKEN).clamp(8_000, 96_000)
}

fn label_for(role: Role) -> Option<&'static str> {
    match role {
        Role::User => Some("User"),
        Role::Assistant => Some("Assistant"),
        Role::Tool => Some("Tool"),
        Role::System => None,
    }
}

/// Build a checkpoint message wrapping `summary` with the durable header.
pub fn checkpoint_message(summary: &str) -> Message {
    Message::injected(
        Role::User,
        format!("{CHECKPOINT_HEADER}{summary}"),
        InjectionOrigin::new(InjectionKind::CompactionCheckpoint),
    )
}

/// Assemble the final [`ContextProjectionResult`] from a selection and a summary.
pub fn build_compaction_result(
    before_chars: usize,
    selection: CompactionSelection,
    summary: String,
) -> ContextProjectionResult {
    let CompactionSelection { archived, tail, .. } = selection;
    let mut model_window = Vec::with_capacity(tail.len() + 1);
    model_window.push(checkpoint_message(&summary));
    model_window.extend(tail);
    let after_chars = estimate_chars(&model_window);
    ContextProjectionResult {
        checkpoint: ContextProjectionCheckpoint {
            operation: ContextProjectionKind::Compact,
            archived_messages: archived.len(),
            active_messages: model_window.len(),
            before_chars,
            after_chars,
        },
        model_window,
        archived_originals: archived,
    }
}

/// Deterministic excerpt fallback used when no provider is available or the
/// LLM summarization call fails. Budget is allocated **newest-first** so recent
/// context is never crowded out by older verbose messages; selected excerpts
/// are then emitted in chronological order for readability. When a previous
/// summary exists it is carried forward as anchored context.
pub fn build_excerpt_summary(
    archived: &[Message],
    max_chars: usize,
    previous_summary: Option<&str>,
) -> String {
    // Pass 1 (newest-first): pick which messages fit the remaining budget.
    let mut used = 0usize;
    let mut chosen: Vec<usize> = Vec::new();
    for (index, message) in archived.iter().enumerate().rev() {
        let Some(label) = label_for(message.role) else {
            continue;
        };
        let content = message.content.trim();
        if content.is_empty() {
            continue;
        }
        let remaining = max_chars.saturating_sub(used);
        if remaining < 64 {
            break;
        }
        let cost = content.len().min(EXCERPT_CAP) + label.len() + 4;
        used += cost;
        chosen.push(index);
    }
    chosen.reverse(); // chronological

    // Pass 2: render the chosen messages in order, hard-truncating each.
    let mut output = String::new();
    for index in chosen {
        let message = &archived[index];
        // Skip roles without a render label (e.g. System). Pass 1 above already
        // filters these out of `chosen`, so this is defensive — but it keeps the
        // two passes consistent and avoids a panic if the selection ever diverges.
        let Some(label) = label_for(message.role) else {
            continue;
        };
        let content = message.content.trim();
        let remaining = max_chars.saturating_sub(output.len());
        if remaining < 64 {
            break;
        }
        let excerpt = truncate_utf8(content, remaining.min(EXCERPT_CAP));
        output.push_str(label);
        output.push_str(": ");
        output.push_str(excerpt);
        output.push_str("\n\n");
    }
    let history = output.trim_end().to_string();

    if let Some(previous) = previous_summary.map(str::trim).filter(|s| !s.is_empty()) {
        let previous_budget = (max_chars / 4).clamp(500, 4_000);
        let previous_excerpt = truncate_utf8(previous, previous_budget);
        format!("[Previous summary]\n{previous_excerpt}\n\n[Recent history]\n{history}")
    } else {
        history
    }
}

/// Pure, provider-less compaction using the deterministic excerpt fallback.
/// Kept as a testable building block and as the ultimate fallback when LLM
/// summarization is disabled or unavailable.
#[allow(dead_code)]
pub fn compact_messages(
    messages: &[Message],
    target_tokens: usize,
    preserve_turns: usize,
) -> Option<ContextProjectionResult> {
    let before_chars = estimate_chars(messages);
    let selection = select_compaction(messages, preserve_turns)?;
    let budget_chars = summary_char_budget(target_tokens);
    let summary = build_excerpt_summary(
        &selection.archived,
        budget_chars,
        selection.previous_summary.as_deref(),
    );
    Some(build_compaction_result(before_chars, selection, summary))
}

// ---------------------------------------------------------------------------
// LLM-based summarization
// ---------------------------------------------------------------------------

const SUMMARIZATION_SYSTEM_PROMPT: &str = "\
You are an anchored context summarization assistant for coding sessions.\n\
Summarize only the conversation history you are given. The newest turns may be \
kept verbatim outside your summary, so focus on the older context that still \
matters for continuing the work.\n\
If a <previous-summary> block is included, treat it as the current anchored \
summary: preserve still-true details, remove stale details, and merge in new \
facts.\n\
Always follow the exact output structure requested. Keep every section, \
preserve exact file paths and identifiers when known, and prefer terse bullets \
over paragraphs.\n\
Do not answer the conversation itself. Do not mention that you are summarizing \
or compacting. Respond in the same language as the conversation.";

const SUMMARY_TEMPLATE: &str = "\
Output exactly the Markdown structure shown inside <template> and keep the \
section order unchanged. Do not include the <template> tags in your response.\n\
<template>\n\
## Pursuit\n\
- [single-sentence task summary]\n\
\n\
## Constraints & Preferences\n\
- [user constraints, preferences, specs, or \"(none)\"]\n\
\n\
## Progress\n\
### Done\n\
- [completed work or \"(none)\"]\n\
\n\
### In Progress\n\
- [current work or \"(none)\"]\n\
\n\
### Blocked\n\
- [blockers or \"(none)\"]\n\
\n\
## Key Decisions\n\
- [decision and why, or \"(none)\"]\n\
\n\
## Next Steps\n\
- [ordered next actions or \"(none)\"]\n\
\n\
## Critical Context\n\
- [important technical facts, errors, open questions, or \"(none)\"]\n\
\n\
## Relevant Files\n\
- [file or directory path: why it matters, or \"(none)\"]\n\
</template>\n\
\n\
Rules:\n\
- Keep every section, even when empty.\n\
- Use terse bullets, not prose paragraphs.\n\
- Preserve exact file paths, commands, error strings, and identifiers when known.\n\
- Do not mention the summary process or that context was compacted.";

/// Cap applied to each tool-result when serializing history for the summarizer.
const SUMMARY_TOOL_OUTPUT_CAP: usize = 1_500;

/// Render `archived` as a readable transcript for the summarizer, capping tool
/// outputs and dropping the oldest messages when the result exceeds `budget`.
pub fn serialize_for_summary(archived: &[Message], budget: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for message in archived {
        let Some(label) = label_for(message.role) else {
            continue;
        };
        let mut body = message.content.trim().to_string();
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                body.push_str(&format!("\n[tool call: {}({})]", call.name, call.arguments));
            }
        }
        if message.role == Role::Tool {
            body = truncate_utf8(body.trim(), SUMMARY_TOOL_OUTPUT_CAP).to_string();
        }
        // Envoy transcripts: render a bounded view of the nested work so
        // the summarizer can capture what each `task` call actually did
        // (otherwise the LLM only sees "[task result]:\n<final text>" and
        // cannot decide whether the envoy's tool usage is worth mentioning
        // in the anchored summary). The nested view is hard-capped to avoid
        // blowing the budget on a single envoy that ran for 30 tool rounds.
        if let Some(children) = &message.children
            && !children.is_empty()
        {
            let nested = serialize_envoy_transcript_for_summary(children, SUMMARY_ENVOY_CAP);
            if !nested.is_empty() {
                body.push_str("\n[envoy transcript]\n");
                body.push_str(&nested);
            }
        }
        if body.trim().is_empty() {
            continue;
        }
        lines.push(format!("{label}: {body}"));
    }

    let joined = lines.join("\n\n");
    if joined.len() <= budget {
        return joined;
    }

    // Over budget: keep the most recent lines that fit.
    let mut kept: Vec<&String> = Vec::new();
    let mut total = 0usize;
    for line in lines.iter().rev() {
        if total + line.len() + 2 > budget {
            break;
        }
        total += line.len() + 2;
        kept.push(line);
    }
    kept.reverse();
    let kept_str: Vec<&str> = kept.iter().map(|s| s.as_str()).collect();
    format!(
        "...[earlier history omitted]...\n\n{}",
        kept_str.join("\n\n")
    )
}

/// Per-envoy character cap when rendering the nested transcript into the
/// summarizer prompt. Large enough to surface the envoy's task, its key
/// tool calls, and its conclusion; small enough that a turn with five
/// envoys cannot crowd out the rest of the conversation.
const SUMMARY_ENVOY_CAP: usize = 2_000;

/// Render an envoy's nested transcript as a compact summarizer-facing view.
/// Recursive: an envoy's own `task` results (sub-envoys) are rendered
/// one level deeper with an even smaller cap. Depth is bounded in practice by
/// the `EnvoyTool` excluding itself from the sub-toolset.
fn serialize_envoy_transcript_for_summary(children: &[Message], budget: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for message in children {
        let Some(label) = label_for(message.role) else {
            continue;
        };
        let mut body = message.content.trim().to_string();
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                body.push_str(&format!("\n[tool call: {}({})]", call.name, call.arguments));
            }
        }
        if message.role == Role::Tool {
            body = truncate_utf8(body.trim(), SUMMARY_TOOL_OUTPUT_CAP).to_string();
        }
        // One level deeper, with a much smaller cap, so we never spend more
        // than ~25% of the parent envoy's budget on a single sub-envoy.
        if let Some(nested) = &message.children
            && !nested.is_empty()
        {
            let inner = serialize_envoy_transcript_for_summary(nested, (budget / 4).max(500));
            if !inner.is_empty() {
                body.push_str("\n[sub-envoy transcript]\n");
                body.push_str(&inner);
            }
        }
        if body.trim().is_empty() {
            continue;
        }
        lines.push(format!("  {label}: {body}"));
    }
    let joined = lines.join("\n");
    if joined.len() <= budget {
        joined
    } else {
        format!("{}...[truncated]", truncate_utf8(&joined, budget))
    }
}

fn build_summarization_user_prompt(
    transcript: &str,
    previous_summary: Option<&str>,
    extra_context: &[String],
) -> String {
    let mut parts = Vec::new();
    match previous_summary.map(str::trim).filter(|s| !s.is_empty()) {
        Some(previous) => parts.push(format!(
            "Update the anchored summary below using the conversation history that \
             follows. Preserve still-true details, remove stale details, and merge in \
             new facts.\n<previous-summary>\n{previous}\n</previous-summary>"
        )),
        None => parts
            .push("Create a new anchored summary from the conversation history below.".to_string()),
    }
    parts.push(SUMMARY_TEMPLATE.to_string());
    for context in extra_context {
        let context = context.trim();
        if !context.is_empty() {
            parts.push(context.to_string());
        }
    }
    parts.push(format!("Conversation history:\n{transcript}"));
    parts.join("\n\n")
}

/// Ask `provider` to summarize `archived`. Returns the summary text, or an
/// error that the caller maps to the deterministic excerpt fallback.
pub async fn summarize_with_provider(
    provider: &Arc<dyn Provider>,
    archived: &[Message],
    previous_summary: Option<&str>,
    extra_context: &[String],
    budget: usize,
) -> Result<String, String> {
    let transcript = serialize_for_summary(archived, budget);
    let user_prompt = build_summarization_user_prompt(&transcript, previous_summary, extra_context);
    let messages = vec![
        Message::new(Role::System, SUMMARIZATION_SYSTEM_PROMPT),
        Message::new(Role::User, user_prompt),
    ];
    // Bound the summarization call so a stalled or overloaded provider
    // triggers the excerpt fallback instead of hanging the turn (and the
    // entire frontend) forever. Two minutes is generous for a single
    // summarization response.
    const SUMMARIZATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    let response = match tokio::time::timeout(SUMMARIZATION_TIMEOUT, provider.chat(messages)).await
    {
        Ok(result) => result?,
        Err(_elapsed) => {
            return Err(format!(
                "Summarization timed out after {} seconds; using excerpt fallback.",
                SUMMARIZATION_TIMEOUT.as_secs()
            ));
        }
    };
    let summary = response.content.trim().to_string();
    if summary.is_empty() {
        return Err("Summarization returned an empty summary.".to_string());
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Compaction orchestrator
// ---------------------------------------------------------------------------

/// Run a compaction over `history` in place.
///
/// When `provider` is `Some`, an LLM produces an anchored structured summary
/// (with the previous summary carried forward for incremental updates); on any
/// failure it falls back to the deterministic excerpt summary. When `provider`
/// is `None`, the excerpt summary is used directly.
pub async fn run_compaction(
    history: &mut Vec<Message>,
    target_tokens: usize,
    preserve_turns: usize,
    provider: Option<Arc<dyn Provider>>,
    extra_context: Vec<String>,
) -> Result<Option<ContextProjectionResult>, String> {
    let before_chars = estimate_chars(history);
    let before_tokens = estimate_tokens(history);
    let Some(selection) = select_compaction(history, preserve_turns) else {
        return Ok(None);
    };

    let budget_chars = summary_char_budget(target_tokens);
    let summary = match provider.as_ref() {
        Some(provider) => {
            match summarize_with_provider(
                provider,
                &selection.archived,
                selection.previous_summary.as_deref(),
                &extra_context,
                budget_chars,
            )
            .await
            {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "LLM summarization failed; falling back to excerpt compaction"
                    );
                    build_excerpt_summary(
                        &selection.archived,
                        budget_chars,
                        selection.previous_summary.as_deref(),
                    )
                }
            }
        }
        None => build_excerpt_summary(
            &selection.archived,
            budget_chars,
            selection.previous_summary.as_deref(),
        ),
    };

    let result = build_compaction_result(before_chars, selection, summary);
    tracing::debug!(
        before_chars,
        after_chars = result.checkpoint.after_chars,
        before_tokens,
        "compaction complete"
    );
    let model_window = result.model_window.clone();
    *history = model_window;
    Ok(Some(result))
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Diagnostic scan of stored session files. When `project_root` is `None`
/// every project bucket is inspected; when supplied only that project's bucket
/// is checked. Prints one line per file and a summary.
pub async fn run_doctor(project_root: Option<&std::path::Path>) -> Result<(), String> {
    struct Report {
        examined: usize,
        corrupt: usize,
    }

    impl Report {
        fn record(&mut self, path: &std::path::Path, result: Result<&SessionData, String>) {
            self.examined += 1;
            match result {
                Ok(data) => {
                    let message_count = data.model_window.len() + data.archived_transcript.len();
                    println!(
                        "ok       {} (schema {}, checksum={}, {} messages)",
                        path.display(),
                        data.schema_version,
                        data.checksum
                            .map(|c| format!("{:#010x}", c))
                            .unwrap_or_else(|| "none".to_string()),
                        message_count
                    );
                }
                Err(error) => {
                    self.corrupt += 1;
                    println!("corrupt  {}: {}", path.display(), error);
                }
            }
        }
    }

    fn inspect(path: &std::path::Path, report: &mut Report) {
        let raw = match fs::read_to_string(path) {
            Ok(r) => r,
            Err(error) => {
                report.record(path, Err(error.to_string()));
                return;
            }
        };
        let result = serde_json::from_str::<SessionData>(&raw)
            .map_err(|error| error.to_string())
            .and_then(|data| verify_checksum(&data).map(|_| data));
        match result {
            Ok(data) => report.record(path, Ok(&data)),
            Err(error) => report.record(path, Err(error)),
        }
    }

    fn scan_bucket(path: &std::path::Path, report: &mut Report) {
        // ADR-0018: every session lives under `sessions/<id>.json` with its
        // matching `<id>.jsonl` log. A stray root `session.json` (left by an
        // older layout) is still reported so the operator can spot it.
        let legacy_active = path.join("session.json");
        if legacy_active.exists() {
            inspect(&legacy_active, report);
        }
        let sessions_dir = path.join("sessions");
        if let Ok(entries) = fs::read_dir(&sessions_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                inspect(&path, report);
                // Verify the matching event log exists; flag its absence as a
                // soft note rather than corruption (it will be seeded on open).
                let log = path.with_extension("jsonl");
                if !log.exists() {
                    println!("note     {} (no event log; seeded on open)", log.display());
                }
            }
        }
    }

    let dirs = paths::get();
    let mut report = Report {
        examined: 0,
        corrupt: 0,
    };

    if let Some(root) = project_root {
        scan_bucket(&dirs.project_dir(root), &mut report);
    } else {
        if let Ok(entries) = fs::read_dir(dirs.projects_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan_bucket(&path, &mut report);
                }
            }
        }
    }

    println!("---");
    println!("examined: {}, corrupt: {}", report.examined, report.corrupt);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use neenee_core::async_trait;

    /// Tests that touch process-global state (`paths::set_test_default` or
    /// process env vars) cannot run in parallel. We serialise them through
    /// this lock; pure-computation tests skip the guard.
    static GLOBAL_GUARD: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    macro_rules! locked {
        ($body:block) => {{
            let _guard = GLOBAL_GUARD.lock().await;
            $body
        }};
    }

    #[tokio::test]
    async fn session_data_round_trips() {
        let directory =
            std::env::temp_dir().join(format!("neenee-session-test-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());
        let messages = vec![Message::new(neenee_core::Role::User, "hello")];
        store.replace_messages(messages.clone()).await.unwrap();
        store
            .set_checkpoint(Some(PursuitCheckpoint {
                pursuit: "test".to_string(),
                iteration: 2,
                max_iterations: 8,
                status: "running".to_string(),
            }))
            .await
            .unwrap();

        let data: SessionData = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(data.model_window[0].content, messages[0].content);
        assert_eq!(data.loop_checkpoint.unwrap().iteration, 2);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn schema_version_defaults_to_current_and_serialises() {
        let data = SessionData::default();
        assert_eq!(data.schema_version, CURRENT_SCHEMA_VERSION);
        let raw = serde_json::to_string(&data).unwrap();
        assert!(raw.contains("\"schema_version\":"));
    }

    #[test]
    fn legacy_session_without_schema_version_loads_as_current() {
        let data: SessionData = serde_json::from_str(
            r#"{
                "id": "00000000-0000-0000-0000-000000000001",
                "messages": [],
                "archived_messages": [],
                "loop_checkpoint": null,
                "compaction": null
            }"#,
        )
        .unwrap();
        // Missing fields inherit the serde default, which is the current
        // schema version, so legacy files appear up-to-date.
        assert_eq!(data.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn schema_migration_bumps_version() {
        let data = SessionData {
            schema_version: 0,
            ..SessionData::default()
        };
        let migrated = migrate_session_data(data);
        assert_eq!(migrated.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn checksum_computes_and_verifies() {
        let mut data = SessionData::default();
        data.model_window = vec![Message::new(neenee_core::Role::User, "hello")];
        data.checksum = Some(compute_checksum(&data));
        assert!(verify_checksum(&data).is_ok());

        // Tamper with a field: verification must fail.
        data.model_window[0].content = "goodbye".to_string();
        assert!(verify_checksum(&data).is_err());
    }

    #[test]
    fn checksum_is_none_for_legacy_files() {
        let data: SessionData = serde_json::from_str(
            r#"{
                "id": "00000000-0000-0000-0000-000000000001",
                "messages": [],
                "archived_messages": [],
                "schema_version": 1
            }"#,
        )
        .unwrap();
        assert!(data.checksum.is_none());
        assert!(
            verify_checksum(&data).is_ok(),
            "missing checksum is allowed"
        );
    }

    #[tokio::test]
    async fn session_persists_envoy_children_round_trip() {
        // End-to-end persistence contract: a session that contains a `task`
        // tool call must round-trip the envoy's nested transcript through
        // session.json, so a subsequent `SessionStore::load_for_project` (the
        // production resume path) restores the children intact. Before Phase 3
        // children were silently dropped because `Message::children` did not
        // exist and the harness only persisted the textual summary.
        let directory =
            std::env::temp_dir().join(format!("neenee-envoy-persist-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());

        let call = neenee_core::ToolCall {
            id: "call_sub1".to_string(),
            name: "envoy".to_string(),
            arguments: r#"{"description":"d","prompt":"p"}"#.to_string(),
        };
        let assistant = Message::new(neenee_core::Role::Assistant, "")
            .with_attribution("kimi-code", "kimi-k2.7-code");
        let assistant = Message {
            tool_calls: Some(vec![call.clone()]),
            ..assistant
        };
        let envoy_transcript = vec![
            Message::new(neenee_core::Role::User, "find foo"),
            Message::new(neenee_core::Role::Assistant, "looking..."),
            Message::new(neenee_core::Role::Assistant, "foo is at src/foo.rs"),
        ];
        let tool = Message::tool_result(&call, "[task result]:\nfoo is at src/foo.rs")
            .with_children(envoy_transcript);
        store
            .replace_messages(vec![
                Message::new(neenee_core::Role::User, "where is foo?"),
                assistant,
                tool,
            ])
            .await
            .unwrap();

        // Reload from disk as production code would.
        let loaded = fs::read_to_string(&path).unwrap();
        let data: SessionData = serde_json::from_str(&loaded).unwrap();
        let tool_msg = data
            .model_window
            .iter()
            .find(|m| m.role == neenee_core::Role::Tool)
            .expect("tool result message persisted");
        let children = tool_msg.children.as_ref().expect("children persisted");
        assert_eq!(children.len(), 3);
        assert!(children.iter().any(|m| m.content == "find foo"));
        assert!(children.iter().any(|m| m.content.contains("src/foo.rs")));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn legacy_single_session_snapshot_migrates_with_defaults() {
        let data: SessionData = serde_json::from_str(
            r#"{
                "id": "00000000-0000-0000-0000-000000000001",
                "messages": [],
                "archived_messages": [],
                "loop_checkpoint": null,
                "compaction": null
            }"#,
        )
        .unwrap();

        assert_eq!(data.parent_id, None);
        assert!(data.created_at > 0);
        assert!(data.updated_at > 0);
        // Phase 2: project_root defaults to current cwd for legacy snapshots
        // missing the field.
        assert!(!data.project_root.as_os_str().is_empty());
    }

    #[tokio::test]
    async fn load_for_project_isolates_sessions_per_cwd() {
        locked!({
            let root =
                std::env::temp_dir().join(format!("neenee-proj-iso-{}", uuid::Uuid::new_v4()));
            let dirs = paths::Dirs::resolve(&paths::PathsOverride {
                data_dir: Some(root.join("data")),
                state_dir: Some(root.join("state")),
                config_dir: Some(root.join("config")),
                cache_dir: Some(root.join("cache")),
            });
            dirs.ensure().unwrap();
            paths::set_test_default(Some(dirs.clone()));
            // Build two stores bound to different project roots.
            let store_a = SessionStore::load_for_project(PathBuf::from("/projects/alpha"));
            let store_b = SessionStore::load_for_project(PathBuf::from("/projects/beta"));

            store_a
                .replace_messages(vec![Message::new(Role::User, "alpha work")])
                .await
                .unwrap();
            store_b
                .replace_messages(vec![Message::new(Role::User, "beta work")])
                .await
                .unwrap();

            let bucket_a = crate::paths::project_bucket_name(&PathBuf::from("/projects/alpha"));
            let bucket_b = crate::paths::project_bucket_name(&PathBuf::from("/projects/beta"));
            assert_ne!(bucket_a, bucket_b);

            // ADR-0018: each instance pins its own `sessions/<id>.json`. There
            // is no longer a project-root `session.json`.
            let id_a = store_a.id().await;
            let id_b = store_b.id().await;
            assert!(
                dirs.project_sessions_dir(&PathBuf::from("/projects/alpha"))
                    .join(format!("{id_a}.json"))
                    .exists()
            );
            assert!(
                dirs.project_sessions_dir(&PathBuf::from("/projects/beta"))
                    .join(format!("{id_b}.json"))
                    .exists()
            );

            // Reloading alpha starts fresh but the prior session is resumable,
            // and alpha never sees beta's messages.
            let reloaded_a = SessionStore::load_for_project(PathBuf::from("/projects/alpha"));
            reloaded_a.resume(Some(&id_a)).await.unwrap();
            assert_eq!(reloaded_a.model_window().await[0].content, "alpha work");
            let reloaded_b = SessionStore::load_for_project(PathBuf::from("/projects/beta"));
            reloaded_b.resume(Some(&id_b)).await.unwrap();
            assert_eq!(reloaded_b.model_window().await[0].content, "beta work");

            // list() is scoped per project — alpha only sees its own session.
            let alpha_sessions = reloaded_a.list().await.unwrap();
            assert!(alpha_sessions.iter().all(|s| !s.overview.contains("beta")));
            let beta_sessions = reloaded_b.list().await.unwrap();
            assert!(beta_sessions.iter().all(|s| !s.overview.contains("alpha")));

            paths::set_test_default(None);
            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[tokio::test]
    async fn fork_preserves_both_durable_branches() {
        let directory =
            std::env::temp_dir().join(format!("neenee-session-fork-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());
        store
            .replace_messages(vec![Message::new(neenee_core::Role::User, "parent")])
            .await
            .unwrap();
        let parent_id = store.id().await;

        let (fork_id, source_id) = store.fork().await.unwrap();
        assert_eq!(source_id, parent_id);
        assert_eq!(store.parent_id().await.as_deref(), Some(parent_id.as_str()));
        store
            .replace_messages(vec![Message::new(neenee_core::Role::User, "fork")])
            .await
            .unwrap();

        let sessions = store.list().await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|item| item.id == parent_id));
        assert!(
            sessions
                .iter()
                .any(|item| item.id == fork_id && item.active)
        );

        store.open(&parent_id[..8]).await.unwrap();
        assert_eq!(store.model_window().await[0].content, "parent");
        store.open(&fork_id[..8]).await.unwrap();
        assert_eq!(store.model_window().await[0].content, "fork");
        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn fork_to_side_leaves_primary_active_pointer_intact() {
        // ADR-0017: a side fork must NOT repoint the primary's active pointer.
        // The primary keeps its id, history, and (by construction) any in-flight
        // turn; only a self-contained sibling file is written.
        let directory =
            std::env::temp_dir().join(format!("neenee-session-side-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());
        store
            .replace_messages(vec![Message::new(neenee_core::Role::User, "parent")])
            .await
            .unwrap();
        let parent_id = store.id().await;

        let (side_id, source_id) = store.fork_to_side().await.unwrap();
        assert_eq!(source_id, parent_id);
        assert_ne!(side_id, parent_id);

        // The primary is untouched: same id, still holds "parent", and has no
        // parent link (it did not become a child).
        assert_eq!(store.id().await, parent_id);
        assert_eq!(store.model_window().await[0].content, "parent");
        assert!(store.parent_id().await.is_none());

        // The side loads into its own store with the inherited history and the
        // parent lineage recorded.
        let side = store.open_side(&side_id).await.unwrap();
        assert_eq!(side.id().await, side_id);
        assert_eq!(side.parent_id().await.as_deref(), Some(parent_id.as_str()));
        assert_eq!(side.model_window().await[0].content, "parent");

        // Writing to the side never reaches the primary.
        side.replace_messages(vec![Message::new(neenee_core::Role::User, "side")])
            .await
            .unwrap();
        assert_eq!(store.model_window().await[0].content, "parent");

        // The side is independently resumable from disk (self-contained file).
        let reopened = store.open_side(&side_id).await.unwrap();
        assert_eq!(reopened.model_window().await[0].content, "side");

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn list_skips_active_session_when_it_has_no_content() {
        // Regression: a fresh empty active session used to be appended to
        // every `list()` call. Because `updated_at` is bumped on startup,
        // it sorted to the top and permanently showed "(empty session)".
        // The picker should only surface the active session once it has
        // real content (messages, archived messages, a loop checkpoint, or
        // a compaction marker).
        let directory = std::env::temp_dir().join(format!(
            "neenee-session-list-empty-{}",
            uuid::Uuid::new_v4()
        ));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());

        // Seed one archived session so the picker has something to show,
        // then keep the active session empty (the default state).
        let archived = SessionData {
            project_root: directory.clone(),
            model_window: vec![Message::new(neenee_core::Role::User, "archived branch")],
            ..Default::default()
        };
        store.persist_archive(&archived).unwrap();

        let sessions = store.list().await.unwrap();
        assert_eq!(
            sessions.len(),
            1,
            "empty active session must not appear in the list"
        );
        assert_eq!(sessions[0].id, archived.id);
        assert!(!sessions[0].active);

        // Once the active session gets content it should reappear, marked
        // active so the picker can badge it.
        store
            .replace_messages(vec![Message::new(neenee_core::Role::User, "live branch")])
            .await
            .unwrap();
        let sessions = store.list().await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|item| item.active));
        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn todos_round_trip_through_disk() {
        let directory =
            std::env::temp_dir().join(format!("neenee-todos-state-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());
        assert!(store.todos().await.is_empty());

        // Seed via reconcile and persist.
        let mut list = neenee_core::TodoList::new();
        list.reconcile(
            &[
                ("Summary".to_string(), neenee_core::TodoStatus::Pending),
                ("Key Changes".to_string(), neenee_core::TodoStatus::Pending),
                ("Test Plan".to_string(), neenee_core::TodoStatus::Pending),
            ],
            1000,
            3,
        );
        store.set_todos(list.clone()).await.unwrap();

        // Mutate (mark progress) and persist again — identity must survive.
        list.update("summary", neenee_core::TodoStatus::Completed, 2000, 4);
        store.set_todos(list.clone()).await.unwrap();

        // Reload from disk via the event log + snapshot and confirm round-trip.
        let reloaded = SessionStore::for_path(path.clone());
        let loaded = reloaded.todos().await;
        assert_eq!(loaded.len(), 3, "all items round-trip through disk");
        assert_eq!(loaded.items[0].content, "Summary");
        assert_eq!(loaded.items[0].status, neenee_core::TodoStatus::Completed);
        assert_eq!(loaded.updated_at_turn, 4);
        // Identity is stable: the first item's id is unchanged after the update.
        assert_eq!(loaded.items[0].id, list.items[0].id);

        // Clearing persists (empty list is the "no active list" state).
        reloaded
            .set_todos(neenee_core::TodoList::default())
            .await
            .unwrap();
        assert!(reloaded.todos().await.is_empty());

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn startup_new_session_can_resume_most_recent_cache() {
        let directory =
            std::env::temp_dir().join(format!("neenee-session-resume-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());
        store
            .replace_messages(vec![Message::new(neenee_core::Role::User, "previous")])
            .await
            .unwrap();
        let previous_id = store.id().await;

        let new_id = store.reset().await.unwrap();
        assert_ne!(new_id, previous_id);
        assert!(store.model_window().await.is_empty());

        let resumed_id = store.resume(None).await.unwrap();
        assert_eq!(resumed_id, previous_id);
        assert_eq!(store.model_window().await[0].content, "previous");
        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn event_log_is_authoritative_on_reload() {
        let directory =
            std::env::temp_dir().join(format!("neenee-events-reload-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());
        store
            .replace_messages(vec![Message::new(neenee_core::Role::User, "first")])
            .await
            .unwrap();
        let first_id = store.id().await;

        // Corrupt the snapshot cache: the event log must still restore state.
        let mut corrupted: SessionData =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        corrupted.model_window[0].content = "tampered".to_string();
        let test_blobs = BlobStore::new(directory.join("blobs"));
        write_session_file(&path, &corrupted, &test_blobs).unwrap();

        // Re-open: for_path replays the event log, not the snapshot.
        let reloaded = SessionStore::for_path(path.clone());
        assert_eq!(reloaded.id().await, first_id);
        assert_eq!(reloaded.model_window().await[0].content, "first");

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn append_round_persists_delta_and_survives_reload() {
        // The mid-turn save point (ADR-0035): `append_round` writes only the
        // new tail as a `MessagesAppended` event, and a fresh `SessionStore`
        // at the same path must replay it to recover the full history. This
        // is the resume-after-crash contract — the whole point of the feature.
        let directory =
            std::env::temp_dir().join(format!("neenee-append-round-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());

        // Turn opens with one user message, durably written.
        store
            .replace_messages(vec![Message::new(neenee_core::Role::User, "user prompt")])
            .await
            .unwrap();

        // Round 1 adds an assistant response + a tool result. The caller
        // passes the *full* current history; the store appends only the tail.
        let round1 = vec![
            Message::new(neenee_core::Role::User, "user prompt"),
            Message::new(neenee_core::Role::Assistant, "I will run a tool"),
            Message::new(neenee_core::Role::Tool, "tool output"),
        ];
        store.append_round(&round1).await.unwrap();

        // Round 2 adds more. The snapshot cache is still at the turn-open
        // state (one message); only the event log has grown.
        let round2 = vec![
            Message::new(neenee_core::Role::User, "user prompt"),
            Message::new(neenee_core::Role::Assistant, "I will run a tool"),
            Message::new(neenee_core::Role::Tool, "tool output"),
            Message::new(neenee_core::Role::Assistant, "done"),
        ];
        store.append_round(&round2).await.unwrap();

        // The live in-memory state reflects all appends.
        let live = store.model_window().await;
        assert_eq!(live.len(), 4);
        assert_eq!(live[3].content, "done");

        // A brand-new store replays the event log and recovers everything,
        // including the appended tail the snapshot never recorded.
        let reloaded = SessionStore::for_path(path.clone());
        let recovered = reloaded.model_window().await;
        assert_eq!(recovered.len(), 4, "appended rounds survive reload");
        assert_eq!(recovered[2].content, "tool output");
        assert_eq!(recovered[3].content, "done");

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn append_round_is_noop_when_nothing_new() {
        // Passing a history no longer than the durable baseline (e.g. right
        // after a compaction rewrote the window via `replace_messages`) must
        // not corrupt anything or write a spurious event.
        let directory =
            std::env::temp_dir().join(format!("neenee-append-noop-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());
        let messages = vec![Message::new(neenee_core::Role::User, "hi")];
        store.replace_messages(messages.clone()).await.unwrap();

        // Same length, same content → no-op.
        store.append_round(&messages).await.unwrap();
        assert_eq!(store.model_window().await.len(), 1);

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn append_round_falls_back_to_replace_on_divergent_prefix() {
        // If the incoming prefix disagrees with the durable state (a bug or a
        // compaction that bypassed `replace_messages`), `append_round` must
        // fall back to a full replace rather than splice a corrupt tail.
        let directory =
            std::env::temp_dir().join(format!("neenee-append-diverge-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());
        store
            .replace_messages(vec![Message::new(neenee_core::Role::User, "original")])
            .await
            .unwrap();

        // Incoming history where the durable prefix was *rewritten* — the
        // first message content differs.
        let divergent = vec![
            Message::new(neenee_core::Role::User, "rewritten"),
            Message::new(neenee_core::Role::Assistant, "new"),
        ];
        store.append_round(&divergent).await.unwrap();

        // The fallback replaced everything with the incoming history.
        let live = store.model_window().await;
        assert_eq!(live.len(), 2);
        assert_eq!(live[0].content, "rewritten");
        assert_eq!(live[1].content, "new");

        // And a reload recovers the replaced state, not a corrupt splice.
        let reloaded = SessionStore::for_path(path.clone());
        let recovered = reloaded.model_window().await;
        assert_eq!(recovered[0].content, "rewritten");

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn projection_snapshot_import_does_not_duplicate_archive() {
        let directory =
            std::env::temp_dir().join(format!("neenee-projection-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("session.json");
        let blob_store = BlobStore::new(directory.join("blobs"));
        let snapshot = SessionData {
            model_window: vec![Message::new(neenee_core::Role::User, "live window")],
            archived_transcript: vec![Message::new(neenee_core::Role::Assistant, "archived")],
            last_projection: Some(ContextProjectionCheckpoint {
                operation: ContextProjectionKind::Compact,
                archived_messages: 1,
                active_messages: 1,
                before_chars: 100,
                after_chars: 20,
            }),
            ..Default::default()
        };
        write_session_file(&path, &snapshot, &blob_store).unwrap();

        let store = SessionStore::for_path(path.clone());
        let transcript = store.full_transcript().await;
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].content, "archived");
        assert_eq!(transcript[1].content, "live window");
        assert_eq!(
            store.last_projection().await.unwrap().operation,
            ContextProjectionKind::Compact
        );

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn large_message_content_is_offloaded_to_blob_store() {
        let directory =
            std::env::temp_dir().join(format!("neenee-blob-session-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());
        let big = "x".repeat(8_192);
        store
            .replace_messages(vec![Message::new(neenee_core::Role::User, &big)])
            .await
            .unwrap();

        // Snapshot on disk should reference the blob.
        let raw = fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("content_blob"),
            "large content should be offloaded"
        );
        assert!(
            !raw.contains(&big),
            "raw content should not appear in snapshot"
        );

        // Replaying the event log rehydrates content from the blob store.
        let reloaded = SessionStore::for_path(path.clone());
        let messages = reloaded.model_window().await;
        assert_eq!(messages[0].content, big);
        assert!(
            messages[0].content_blob.is_none(),
            "memory uses inline content"
        );

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn injection_origin_survives_persist_and_reload() {
        // A harness-injected message's provenance must round-trip through the
        // snapshot cache AND the event-log replay path — the contract that lets
        // a resumed session faithfully reconstruct what was injected and why.
        // This is the end-to-end (store-layer) companion to the message-level
        // round-trip test in neenee-core.
        use neenee_core::{HookEventKind, InjectionKind, InjectionOrigin};
        let directory =
            std::env::temp_dir().join(format!("neenee-origin-session-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore::for_path(path.clone());
        let injected = Message::injected(
            neenee_core::Role::User,
            "setup context",
            InjectionOrigin::new(InjectionKind::Hook(HookEventKind::SessionStart))
                .with_reason("onstart.sh"),
        );
        store.replace_messages(vec![injected]).await.unwrap();

        // The snapshot file carries the origin object.
        let raw = fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"origin\""),
            "snapshot must persist origin: {raw}"
        );
        // HookEventKind serialises in PascalCase (no rename_all), so the wire
        // tag is "SessionStart". Pretty-printed with a space after the colon.
        assert!(
            raw.contains("\"hook\": \"SessionStart\""),
            "snapshot must persist the hook kind: {raw}"
        );

        // Reload via the event-log path (authoritative) rehydrates it intact.
        let reloaded = SessionStore::for_path(path.clone());
        let messages = reloaded.model_window().await;
        assert_eq!(messages.len(), 1);
        let origin = messages[0].origin.as_ref().expect("origin reloaded");
        assert_eq!(
            origin.kind,
            InjectionKind::Hook(HookEventKind::SessionStart)
        );
        assert_eq!(origin.reason.as_deref(), Some("onstart.sh"));

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn legacy_snapshot_without_origin_loads_as_none() {
        // A pre-C4 snapshot file (no `origin` key on any message) must load
        // with `origin: None` for every message — the store-layer side of the
        // backward-compat contract. Provenance is simply absent for old data.
        let directory =
            std::env::temp_dir().join(format!("neenee-legacy-origin-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("session.json");
        // Minimal pre-C4 snapshot: no origin key, schema_version 3.
        let legacy = serde_json::json!({
            "id": "legacy",
            "parent_id": null,
            "created_at": 1u64,
            "updated_at": 1u64,
            "messages": [
                {"role":"User","content":"old user input","hidden":false},
                {"role":"Assistant","content":"old reply","hidden":false}
            ],
            "archived_messages": [],
            "loop_checkpoint": null,
            "last_relief": null,
            "project_root": ".",
            "todos": [],
            "schema_version": 3,
            "checksum": null,
            "title": null,
            "title_manual": false,
            "pursuit": null
        });
        fs::write(&path, legacy.to_string()).unwrap();

        let store = SessionStore::for_path(path.clone());
        let messages = store.model_window().await;
        assert_eq!(messages.len(), 2);
        for (i, m) in messages.iter().enumerate() {
            assert!(
                m.origin.is_none(),
                "legacy message {i} must load with origin None, got {:?}",
                m.origin
            );
        }

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn compaction_keeps_recent_complete_turns() {
        let messages = vec![
            Message::new(neenee_core::Role::System, "system"),
            Message::new(neenee_core::Role::User, "old question"),
            Message::new(neenee_core::Role::Assistant, "old answer"),
            Message::new(neenee_core::Role::Tool, "old tool result"),
            Message::new(neenee_core::Role::User, "recent question"),
            Message::new(neenee_core::Role::Assistant, "recent answer"),
        ];

        let result = compact_messages(&messages, 10_000, 1).unwrap();

        assert_eq!(result.checkpoint.operation, ContextProjectionKind::Compact);
        assert_eq!(result.model_window[0].role, neenee_core::Role::User);
        assert!(result.model_window[0].hidden);
        assert_eq!(result.model_window[1].content, "recent question");
        assert_eq!(result.model_window[2].content, "recent answer");
        assert!(
            result
                .archived_originals
                .iter()
                .any(|message| message.content == "old tool result")
        );
        assert!(
            !result
                .archived_originals
                .iter()
                .any(|message| message.role == neenee_core::Role::System)
        );
    }

    #[test]
    fn compaction_requires_an_older_complete_turn() {
        let messages = vec![
            Message::new(neenee_core::Role::User, "question"),
            Message::new(neenee_core::Role::Assistant, "answer"),
        ];
        assert!(compact_messages(&messages, 10_000, 1).is_none());
    }

    #[test]
    fn excerpt_summary_keeps_recent_first_under_budget() {
        // A large old tool result and a small recent user message. With a tiny
        // budget only the recent message (chosen newest-first) survives; the old
        // verbose tool result is omitted instead of crowding it out.
        let archived = vec![
            Message::new(Role::Tool, "X".repeat(3_000)),
            Message::new(Role::User, "recent critical detail"),
        ];

        let summary = build_excerpt_summary(&archived, 90, None);

        assert!(summary.contains("recent critical detail"));
        assert!(!summary.contains('X'));
    }

    #[test]
    fn excerpt_summary_carries_forward_previous_summary() {
        let archived = vec![Message::new(Role::User, "what is 2+2")];
        let summary = build_excerpt_summary(&archived, 4_000, Some("prev anchored facts"));

        assert!(summary.starts_with("[Previous summary]\n"));
        assert!(summary.contains("prev anchored facts"));
        assert!(summary.contains("[Recent history]"));
        assert!(summary.contains("what is 2+2"));
    }

    #[test]
    fn select_compaction_extracts_previous_summary() {
        let prior = checkpoint_message("prev summary body");
        let messages = vec![
            Message::new(Role::System, "system"),
            prior,
            Message::new(Role::User, "q1"),
            Message::new(Role::Assistant, "a1"),
            Message::new(Role::User, "q2"),
            Message::new(Role::Assistant, "a2"),
        ];

        let selection = select_compaction(&messages, 1).unwrap();
        assert_eq!(
            selection.previous_summary.as_deref(),
            Some("prev summary body")
        );
        // The prior checkpoint lands in the archived head, not the tail.
        assert!(
            selection
                .archived
                .iter()
                .any(|message| message.content.starts_with("[Conversation checkpoint]"))
        );
        assert_eq!(selection.tail.last().unwrap().content, "a2");
    }

    #[tokio::test]
    async fn run_compaction_uses_provider_summary() {
        use neenee_providers::MockProvider;

        let mut history = vec![
            Message::new(Role::System, "system"),
            Message::new(Role::User, "old question"),
            Message::new(Role::Assistant, "old answer"),
            Message::new(Role::User, "recent question"),
            Message::new(Role::Assistant, "recent answer"),
        ];
        let provider: Arc<dyn Provider> = Arc::new(MockProvider);

        let result = run_compaction(&mut history, 10_000, 1, Some(provider), Vec::new())
            .await
            .unwrap()
            .unwrap();

        // The mock provider's canned reply becomes the checkpoint summary.
        assert!(result.model_window[0].content.contains("mock AI"));
        assert_eq!(result.model_window[1].content, "recent question");
        assert!(result.model_window[0].hidden);
    }

    #[tokio::test]
    async fn run_compaction_falls_back_when_provider_errors() {
        use neenee_providers::MockProvider;

        // MockProvider succeeds, so to exercise the fallback we instead pass a
        // provider that always errors and assert we still get an excerpt-based
        // checkpoint.
        struct FailingProvider;
        #[async_trait]
        impl Provider for FailingProvider {
            async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
                Err("boom".to_string())
            }
            async fn stream_chat(
                &self,
                _messages: Vec<Message>,
            ) -> Result<futures::stream::BoxStream<'static, Result<String, String>>, String>
            {
                Err("boom".to_string())
            }
        }

        let mut history = vec![
            Message::new(Role::User, "old question"),
            Message::new(Role::Assistant, "old answer"),
            Message::new(Role::User, "recent question"),
            Message::new(Role::Assistant, "recent answer"),
        ];
        let provider: Arc<dyn Provider> = Arc::new(FailingProvider);

        let result = run_compaction(&mut history, 10_000, 1, Some(provider), Vec::new())
            .await
            .unwrap()
            .unwrap();

        // Fallback excerpt summary references the old question.
        assert!(result.model_window[0].content.contains("old question"));
        // Silence the unused MockProvider import warning while keeping the path
        // documented for the success-case test above.
        let _: &dyn Provider = &MockProvider;
    }
}

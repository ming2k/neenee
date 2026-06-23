use crate::blobs::BlobStore;
use crate::events::{EventLog, SessionEvent};
use crate::fsutil;
use crate::paths;
use neenee_core::async_trait;
use neenee_core::{Message, Provider, Role};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

// Re-export the cheap context estimators so callers keep using
// `session::estimate_chars` / `session::estimate_tokens`.
pub use neenee_core::{estimate_chars, estimate_tokens};

const CURRENT_SCHEMA_VERSION: u32 = 1;

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


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionCheckpoint {
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
    messages: Vec<Message>,
    archived_messages: Vec<Message>,
    loop_checkpoint: Option<PursuitCheckpoint>,
    compaction: Option<CompactionCheckpoint>,
    /// Working directory this session belongs to. Phase 2 (project isolation)
    /// uses this to route archived sessions to the right per-project bucket
    /// during the one-shot legacy migration. Legacy snapshots missing the
    /// field default to the current cwd.
    project_root: PathBuf,
    /// Path to the plan file most recently approved via `plan_exit`. Mirrored
    /// from `Agent::active_plan_path` so resume restores the Build-mode
    /// "you are implementing X" hint. `None` for legacy snapshots.
    #[serde(default)]
    active_plan_path: Option<PathBuf>,
    /// Live plan progress snapshot, mirrored from `Agent::plan_progress`.
    /// Drives the sticky panel above the input box on resume.
    #[serde(default)]
    plan_progress: Option<neenee_core::PlanProgress>,
    /// Unified task list, mirrored from `Agent::todos`. The single source of
    /// truth for "what is left to do" — absorbs the former plan-progress
    /// section tracker and the former scratchpad `todo` tool state. An empty
    /// list means no active task list. `#[serde(default)]` so legacy
    /// snapshots load as an empty list with no migration.
    #[serde(default)]
    todos: neenee_core::TodoList,
    /// Schema version of this session file. Migrations increment this and are
    /// applied lazily on load.
    schema_version: u32,
    /// CRC32C checksum of the canonical JSON payload (excluding this field).
    /// `None` for legacy files written before C10; new writes always populate
    /// it so `neenee doctor` and future loaders can detect corruption.
    checksum: Option<u32>,
}

impl Default for SessionData {
    fn default() -> Self {
        let now = unix_timestamp();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: None,
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            archived_messages: Vec::new(),
            loop_checkpoint: None,
            compaction: None,
            project_root: default_project_root(),
            active_plan_path: None,
            plan_progress: None,
            todos: neenee_core::TodoList::default(),
            schema_version: CURRENT_SCHEMA_VERSION,
            checksum: None,
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
    // future changes add guarded blocks here, e.g.:
    // if data.schema_version < 2 { ... }
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
        .messages
        .iter_mut()
        .chain(data.archived_messages.iter_mut())
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
        .messages
        .iter_mut()
        .chain(data.archived_messages.iter_mut())
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
            SessionEvent::MessagesReplaced { messages } => data.messages = messages.clone(),
            SessionEvent::CheckpointSet { checkpoint } => data.loop_checkpoint = checkpoint.clone(),
            SessionEvent::CompactionCommitted {
                archived,
                active,
                checkpoint,
            } => {
                data.archived_messages.extend(archived.clone());
                data.messages = active.clone();
                data.compaction = Some(checkpoint.clone());
            }
            SessionEvent::Archived { messages } => data.archived_messages.extend(messages.clone()),
            SessionEvent::ActivePlanPathSet { path } => {
                data.active_plan_path = path.clone();
            }
            SessionEvent::PlanProgressSet { progress } => {
                data.plan_progress = progress.clone();
            }
            SessionEvent::TodosSet { todos } => {
                data.todos = todos.clone();
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
    if !data.archived_messages.is_empty() {
        events.push(crate::events::EventEnvelope {
            seq: events.len() as u64,
            timestamp: data.updated_at,
            event: SessionEvent::Archived {
                messages: data.archived_messages.clone(),
            },
        });
    }
    events.push(crate::events::EventEnvelope {
        seq: events.len() as u64,
        timestamp: data.updated_at,
        event: SessionEvent::MessagesReplaced {
            messages: data.messages.clone(),
        },
    });
    if let Some(checkpoint) = &data.loop_checkpoint {
        events.push(crate::events::EventEnvelope {
            seq: events.len() as u64,
            timestamp: data.updated_at,
            event: SessionEvent::CheckpointSet {
                checkpoint: Some(checkpoint.clone()),
            },
        });
    }
    if let Some(plan_path) = &data.active_plan_path {
        events.push(crate::events::EventEnvelope {
            seq: events.len() as u64,
            timestamp: data.updated_at,
            event: SessionEvent::ActivePlanPathSet {
                path: Some(plan_path.clone()),
            },
        });
    }
    if let Some(progress) = &data.plan_progress {
        events.push(crate::events::EventEnvelope {
            seq: events.len() as u64,
            timestamp: data.updated_at,
            event: SessionEvent::PlanProgressSet {
                progress: Some(progress.clone()),
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
    if let Some(checkpoint) = &data.compaction {
        events.push(crate::events::EventEnvelope {
            seq: events.len() as u64,
            timestamp: data.updated_at,
            event: SessionEvent::CompactionCommitted {
                archived: data.archived_messages.clone(),
                active: data.messages.clone(),
                checkpoint: checkpoint.clone(),
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

pub struct SessionStore {
    project_root: PathBuf,
    path: PathBuf,
    archive_dir: PathBuf,
    event_log: EventLog,
    blob_store: BlobStore,
    data: Mutex<SessionData>,
}

impl SessionStore {
    /// Load the active session for `project_root`. The on-disk layout is
    /// `data_dir/projects/<sha256(cwd)[..16]>/{session.json, sessions/}` —
    /// each project's sessions live under their own bucket so two working
    /// directories never see each other's history.
    ///
    /// On the first launch after the Phase 2 upgrade the legacy flat
    /// `data_dir/sessions/*.json` archives are lazily migrated into this
    /// project's bucket (each one routed by its own `project_root` field,
    /// defaulting to the current cwd when missing).
    pub fn load_for_project(project_root: PathBuf) -> Self {
        let dirs = paths::get();
        let project_dir = dirs.project_dir(&project_root);
        if let Err(e) = std::fs::create_dir_all(project_dir.join("sessions")) {
            tracing::warn!(error = %e, "could not create project session dir");
        }
        let path = project_dir.join("session.json");
        let archive_dir = project_dir.join("sessions");
        let event_log = EventLog::new(project_dir.join("events.jsonl"));
        let blob_store = BlobStore::new(dirs.blobs_dir());
        // Lazy one-shot migration of the legacy flat layout. See
        // [`migrate_flat_sessions_to_project_buckets`].
        let _ = migrate_flat_sessions_to_project_buckets(&dirs, &blob_store);

        let mut data = match event_log.load() {
            Ok(envelopes) if !envelopes.is_empty() => {
                let mut data = SessionData::default();
                apply_events(&mut data, &envelopes);
                if let Err(error) = load_session_blobs(&mut data, &blob_store) {
                    tracing::warn!(error = %error, "could not load session blobs from event log");
                }
                if let Err(error) = verify_checksum(&data) {
                    tracing::warn!(path = %path.display(), error = %error, "session checksum failed");
                }
                data
            }
            _ => {
                // No event log yet: import from the snapshot file or start fresh.
                let mut data = fs::read_to_string(&path)
                    .ok()
                    .and_then(|content| serde_json::from_str::<SessionData>(&content).ok())
                    .unwrap_or_else(|| SessionData {
                        project_root: project_root.clone(),
                        ..Default::default()
                    });
                if let Err(error) = load_session_blobs(&mut data, &blob_store) {
                    tracing::warn!(error = %error, "could not load session blobs from snapshot");
                }
                if let Err(error) = verify_checksum(&data) {
                    tracing::warn!(path = %path.display(), error = %error, "session checksum failed");
                }
                let events = snapshot_to_events(&data);
                let _ = event_log.rewrite(events);
                data
            }
        };

        if data.schema_version < CURRENT_SCHEMA_VERSION {
            data = migrate_session_data(data);
        }
        let _ = write_session_file(&path, &data, &blob_store);
        Self {
            project_root,
            path,
            archive_dir,
            event_log,
            blob_store,
            data: Mutex::new(data),
        }
    }

    /// Project root this store is bound to.
    #[allow(dead_code)]
    pub fn project_root(&self) -> &std::path::Path {
        &self.project_root
    }

    /// Backwards-compatible alias for [`Self::load_for_project`] using the
    /// current process cwd. New code should call `load_for_project` explicitly.
    #[allow(dead_code)]
    pub fn load() -> Self {
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::load_for_project(project_root)
    }

    /// Open a `SessionStore` against an explicit `session.json` path.
    ///
    /// This is the low-level constructor: most callers want
    /// [`SessionStore::load_for_project`], which resolves paths through the
    /// global [`crate::paths`] table. Kept `pub` so external crates' tests
    /// (e.g. the binary crate's retry tests) can point at a throwaway file
    /// without re-wiring the global paths table.
    pub fn for_path(path: PathBuf) -> Self {
        let archive_dir = session_archive_dir(&path);
        let project_root = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let event_log = EventLog::new(path.with_extension("jsonl"));
        let blob_store = BlobStore::new(
            path.parent()
                .map(|p| p.join("blobs"))
                .unwrap_or_else(|| PathBuf::from("blobs")),
        );
        let data = match event_log.load() {
            Ok(envelopes) if !envelopes.is_empty() => {
                let mut data = SessionData::default();
                apply_events(&mut data, &envelopes);
                let _ = load_session_blobs(&mut data, &blob_store);
                data
            }
            _ => SessionData::default(),
        };
        Self {
            project_root,
            archive_dir,
            path: path.clone(),
            event_log,
            blob_store,
            data: Mutex::new(data),
        }
    }

    pub async fn id(&self) -> String {
        self.data.lock().await.id.clone()
    }

    pub async fn messages(&self) -> Vec<Message> {
        self.data.lock().await.messages.clone()
    }

    pub async fn transcript(&self) -> Vec<Message> {
        let data = self.data.lock().await;
        let mut messages = data.archived_messages.clone();
        messages.extend(data.messages.clone());
        messages
    }

    pub async fn checkpoint(&self) -> Option<PursuitCheckpoint> {
        self.data.lock().await.loop_checkpoint.clone()
    }

    /// Path to the plan file most recently approved via `plan_exit`. Mirrored
    /// from `Agent::active_plan_path` so a resumed session restores the
    /// Build-mode "you are implementing X" hint.
    pub async fn active_plan_path(&self) -> Option<PathBuf> {
        self.data.lock().await.active_plan_path.clone()
    }

    /// Replace the active plan path. Pass `None` to clear (used when the
    /// agent re-enters Plan mode). Persists both the snapshot and the event
    /// log so resume restores the same path.
    pub async fn set_active_plan_path(&self, path: Option<PathBuf>) -> Result<(), String> {
        let mut data = self.data.lock().await;
        data.active_plan_path = path.clone();
        data.updated_at = unix_timestamp();
        ensure_event_log_started(&self.event_log, &data)?;
        self.event_log
            .append(SessionEvent::ActivePlanPathSet { path })?;
        self.persist(&data)
    }

    /// Live plan progress snapshot, mirrored from `Agent::plan_progress` so
    /// resume restores the sticky panel above the input box.
    pub async fn plan_progress(&self) -> Option<neenee_core::PlanProgress> {
        self.data.lock().await.plan_progress.clone()
    }

    /// Replace the plan progress snapshot. Persists both the snapshot and
    /// the event log so resume restores the same picture.
    pub async fn set_plan_progress(
        &self,
        progress: Option<neenee_core::PlanProgress>,
    ) -> Result<(), String> {
        let mut data = self.data.lock().await;
        data.plan_progress = progress.clone();
        data.updated_at = unix_timestamp();
        ensure_event_log_started(&self.event_log, &data)?;
        self.event_log
            .append(SessionEvent::PlanProgressSet { progress })?;
        self.persist(&data)
    }

    /// The unified task list, mirrored from `Agent::todos`. Empty means no
    /// active task list. Read on resume to seed the agent and the sticky
    /// panel.
    pub async fn todos(&self) -> neenee_core::TodoList {
        self.data.lock().await.todos.clone()
    }

    /// Replace the task list. Persists both the snapshot and the event log so
    /// resume restores the same list (and so per-item history is retained in
    /// the log).
    pub async fn set_todos(&self, todos: neenee_core::TodoList) -> Result<(), String> {
        let mut data = self.data.lock().await;
        data.todos = todos.clone();
        data.updated_at = unix_timestamp();
        ensure_event_log_started(&self.event_log, &data)?;
        self.event_log.append(SessionEvent::TodosSet { todos })?;
        self.persist(&data)
    }

    pub async fn compaction(&self) -> Option<CompactionCheckpoint> {
        self.data.lock().await.compaction.clone()
    }

    pub async fn archived_count(&self) -> usize {
        self.data.lock().await.archived_messages.len()
    }

    pub async fn parent_id(&self) -> Option<String> {
        self.data.lock().await.parent_id.clone()
    }

    pub async fn replace_messages(&self, messages: Vec<Message>) -> Result<(), String> {
        let mut data = self.data.lock().await;
        data.messages = messages;
        data.updated_at = unix_timestamp();
        ensure_event_log_started(&self.event_log, &data)?;
        self.event_log.append(SessionEvent::MessagesReplaced {
            messages: data.messages.clone(),
        })?;
        self.persist(&data)
    }

    pub async fn set_checkpoint(&self, checkpoint: Option<PursuitCheckpoint>) -> Result<(), String> {
        let mut data = self.data.lock().await;
        data.loop_checkpoint = checkpoint;
        data.updated_at = unix_timestamp();
        ensure_event_log_started(&self.event_log, &data)?;
        self.event_log.append(SessionEvent::CheckpointSet {
            checkpoint: data.loop_checkpoint.clone(),
        })?;
        self.persist(&data)
    }

    pub async fn commit_compaction(&self, result: CompactionResult) -> Result<(), String> {
        let mut data = self.data.lock().await;
        data.archived_messages.extend(result.archived.clone());
        data.messages = result.active.clone();
        data.compaction = Some(result.checkpoint.clone());
        data.updated_at = unix_timestamp();
        ensure_event_log_started(&self.event_log, &data)?;
        self.event_log.append(SessionEvent::CompactionCommitted {
            archived: result.archived,
            active: result.active,
            checkpoint: result.checkpoint,
        })?;
        self.persist(&data)
    }

    pub async fn reset(&self) -> Result<String, String> {
        let mut data = self.data.lock().await;
        if has_content(&data) {
            self.persist_archive(&data)?;
            if !data.messages.is_empty() {
                self.event_log.append(SessionEvent::Archived {
                    messages: data.messages.clone(),
                })?;
            }
        }
        let project_root = data.project_root.clone();
        let schema_version = data.schema_version;
        *data = SessionData::default();
        data.project_root = project_root;
        data.schema_version = schema_version;
        let id = data.id.clone();
        ensure_event_log_started(&self.event_log, &data)?;
        self.event_log
            .append(SessionEvent::Reset { id: id.clone() })?;
        self.persist(&data)?;
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
        Ok(self.data.lock().await.id.clone())
    }

    pub async fn fork(&self) -> Result<(String, String), String> {
        let mut data = self.data.lock().await;
        if data.messages.is_empty() && data.archived_messages.is_empty() {
            return Err("Cannot fork an empty session.".to_string());
        }
        self.persist_archive(&data)?;
        if !data.messages.is_empty() {
            self.event_log.append(SessionEvent::Archived {
                messages: data.messages.clone(),
            })?;
        }
        let parent_id = data.id.clone();
        let now = unix_timestamp();
        data.id = uuid::Uuid::new_v4().to_string();
        data.parent_id = Some(parent_id.clone());
        data.created_at = now;
        data.updated_at = now;
        data.loop_checkpoint = None;
        let fork_id = data.id.clone();
        ensure_event_log_started(&self.event_log, &data)?;
        self.event_log.append(SessionEvent::Forked {
            id: fork_id.clone(),
            parent_id: parent_id.clone(),
        })?;
        self.persist(&data)?;
        self.persist_archive(&data)?;
        Ok((fork_id, parent_id))
    }

    pub async fn open(&self, id: &str) -> Result<(), String> {
        let mut data = self.data.lock().await;
        let id = self.resolve_id(id, &data)?;
        if data.id != id {
            if has_content(&data) {
                self.persist_archive(&data)?;
                if !data.messages.is_empty() {
                    self.event_log.append(SessionEvent::Archived {
                        messages: data.messages.clone(),
                    })?;
                }
            }
            let path = self.archive_path(&id);
            let content = fs::read_to_string(&path)
                .map_err(|error| format!("Could not open session '{}': {}", id, error))?;
            let loaded: SessionData =
                serde_json::from_str(&content).map_err(|error| error.to_string())?;
            if !loaded.archived_messages.is_empty() {
                self.event_log.append(SessionEvent::Archived {
                    messages: loaded.archived_messages.clone(),
                })?;
            }
            data.clone_from(&loaded);
            data.updated_at = unix_timestamp();
            ensure_event_log_started(&self.event_log, &data)?;
            self.event_log.append(SessionEvent::MessagesReplaced {
                messages: data.messages.clone(),
            })?;
            self.persist(&data)?;
        }
        Ok(())
    }

    /// Delete a session by id or short id prefix. Deleting the active session
    /// removes its file and resets the store to a fresh empty session; archived
    /// sessions have their file removed from the sessions directory.
    pub async fn delete(&self, id: &str) -> Result<(), String> {
        let data = self.data.lock().await;
        let resolved = self.resolve_id(id, &data)?;
        let is_active = data.id == resolved;
        drop(data);

        if is_active {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_file(self.event_log.path());
            let mut data = self.data.lock().await;
            let project_root = data.project_root.clone();
            let schema_version = data.schema_version;
            *data = SessionData::default();
            data.project_root = project_root;
            data.schema_version = schema_version;
            self.event_log.append(SessionEvent::Reset {
                id: data.id.clone(),
            })?;
            self.persist(&data)
        } else {
            let path = self.archive_path(&resolved);
            fs::remove_file(&path)
                .map_err(|error| format!("Could not delete session '{}': {}", resolved, error))?;
            Ok(())
        }
    }

    pub async fn list(&self) -> Result<Vec<SessionSummary>, String> {
        let data = self.data.lock().await;
        let mut summaries = Vec::new();
        if self.archive_dir.exists() {
            for entry in fs::read_dir(&self.archive_dir).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let content =
                    fs::read_to_string(entry.path()).map_err(|error| error.to_string())?;
                let session: SessionData =
                    serde_json::from_str(&content).map_err(|error| error.to_string())?;
                if session.id != data.id {
                    summaries.push(summary(&session, false));
                }
            }
        }
        // Only surface the active session when it actually has content. A
        // fresh empty session is what the user is already in — listing it at
        // the top (where the freshly-touched `updated_at` sorts it) just
        // shows a permanent "(empty session)" row that cannot be usefully
        // resumed. Real archived sessions stay reachable.
        if has_content(&data) {
            summaries.push(summary(&data, true));
        }
        summaries.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
        Ok(summaries)
    }

    fn persist(&self, data: &SessionData) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        write_session_file(&self.path, data, &self.blob_store)
    }

    fn persist_archive(&self, data: &SessionData) -> Result<(), String> {
        fs::create_dir_all(&self.archive_dir).map_err(|error| error.to_string())?;
        let path = self.archive_path(&data.id);
        write_session_file(&path, data, &self.blob_store)
    }

    fn archive_path(&self, id: &str) -> PathBuf {
        self.archive_dir.join(format!("{}.json", id))
    }

    fn resolve_id(&self, input: &str, active: &SessionData) -> Result<String, String> {
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
        let mut matches = Vec::new();
        if active.id.starts_with(input) {
            matches.push(active.id.clone());
        }
        if self.archive_dir.exists() {
            for entry in fs::read_dir(&self.archive_dir).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                let path = entry.path();
                let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                if stem.starts_with(input) && !matches.iter().any(|id| id == stem) {
                    matches.push(stem.to_string());
                }
            }
        }
        match matches.as_slice() {
            [id] => Ok(id.clone()),
            [] => Err(format!("No session matches '{}'.", input)),
            _ => Err(format!(
                "Session prefix '{}' is ambiguous ({} matches).",
                input,
                matches.len()
            )),
        }
    }
}

fn has_content(data: &SessionData) -> bool {
    !data.messages.is_empty()
        || !data.archived_messages.is_empty()
        || data.loop_checkpoint.is_some()
        || data.compaction.is_some()
}

fn summary(data: &SessionData, active: bool) -> SessionSummary {
    SessionSummary {
        id: data.id.clone(),
        parent_id: data.parent_id.clone(),
        message_count: data.messages.len() + data.archived_messages.len(),
        updated_at: data.updated_at,
        created_at: data.created_at,
        overview: session_overview(data),
        active,
    }
}

/// Derive a short, human-readable description of a session: the first user
/// message, falling back to the active pursuit, then to a placeholder.
fn session_overview(data: &SessionData) -> String {
    const MAX: usize = 64;
    if let Some(message) = data
        .messages
        .iter()
        .chain(data.archived_messages.iter())
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

pub struct CompactionResult {
    pub active: Vec<Message>,
    pub archived: Vec<Message>,
    pub checkpoint: CompactionCheckpoint,
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

fn summary_budget(max_chars: usize) -> usize {
    (max_chars / 8).clamp(2_000, 16_000)
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
    Message::hidden(Role::User, format!("{CHECKPOINT_HEADER}{summary}"))
}

/// Assemble the final [`CompactionResult`] from a selection and a summary.
pub fn build_compaction_result(
    before_chars: usize,
    selection: CompactionSelection,
    summary: String,
) -> CompactionResult {
    let CompactionSelection { archived, tail, .. } = selection;
    let mut active = Vec::with_capacity(tail.len() + 1);
    active.push(checkpoint_message(&summary));
    active.extend(tail);
    let after_chars = estimate_chars(&active);
    CompactionResult {
        checkpoint: CompactionCheckpoint {
            archived_messages: archived.len(),
            active_messages: active.len(),
            before_chars,
            after_chars,
        },
        active,
        archived,
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
    max_chars: usize,
    preserve_turns: usize,
) -> Option<CompactionResult> {
    let before_chars = estimate_chars(messages);
    let selection = select_compaction(messages, preserve_turns)?;
    let budget = summary_budget(max_chars);
    let summary = build_excerpt_summary(
        &selection.archived,
        budget,
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
        // Sub-agent transcripts: render a bounded view of the nested work so
        // the summarizer can capture what each `task` call actually did
        // (otherwise the LLM only sees "[task result]:\n<final text>" and
        // cannot decide whether the sub-agent's tool usage is worth mentioning
        // in the anchored summary). The nested view is hard-capped to avoid
        // blowing the budget on a single sub-agent that ran for 30 tool rounds.
        if let Some(children) = &message.children {
            if !children.is_empty() {
                let nested =
                    serialize_subagent_transcript_for_summary(children, SUMMARY_SUBAGENT_CAP);
                if !nested.is_empty() {
                    body.push_str("\n[sub-agent transcript]\n");
                    body.push_str(&nested);
                }
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

/// Per-sub-agent character cap when rendering the nested transcript into the
/// summarizer prompt. Large enough to surface the sub-agent's task, its key
/// tool calls, and its conclusion; small enough that a turn with five
/// sub-agents cannot crowd out the rest of the conversation.
const SUMMARY_SUBAGENT_CAP: usize = 2_000;

/// Render a sub-agent's nested transcript as a compact summarizer-facing view.
/// Recursive: a sub-agent's own `task` results (sub-sub-agents) are rendered
/// one level deeper with an even smaller cap. Depth is bounded in practice by
/// the `TaskTool` excluding itself from the sub-toolset.
fn serialize_subagent_transcript_for_summary(children: &[Message], budget: usize) -> String {
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
        // than ~25% of the parent sub-agent's budget on a single sub-sub-agent.
        if let Some(nested) = &message.children {
            if !nested.is_empty() {
                let inner =
                    serialize_subagent_transcript_for_summary(nested, (budget / 4).max(500));
                if !inner.is_empty() {
                    body.push_str("\n[sub-sub-agent transcript]\n");
                    body.push_str(&inner);
                }
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
// Hooks + orchestrator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CompactionDecision {
    /// Set to `false` to veto the compaction. Defaults to `true`.
    pub proceed: bool,
    /// Extra context strings folded into the summarization prompt.
    pub extra_context: Vec<String>,
}

impl CompactionDecision {
    pub fn proceed() -> Self {
        Self {
            proceed: true,
            extra_context: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn veto() -> Self {
        Self {
            proceed: false,
            extra_context: Vec::new(),
        }
    }
}

/// Pre/post-compaction hooks. `pre_compact` can veto a compaction or inject
/// extra summarization context; `post_compact` is informational.
#[async_trait]
pub trait CompactionHooks: Send + Sync {
    async fn pre_compact(&self, _messages: &[Message]) -> CompactionDecision {
        CompactionDecision::proceed()
    }

    async fn post_compact(&self, _checkpoint: &CompactionCheckpoint) {}
}

/// No-op hooks used as the default.
#[allow(dead_code)]
pub struct NoopCompactionHooks;

#[async_trait]
impl CompactionHooks for NoopCompactionHooks {}

/// Run a compaction over `history` in place.
///
/// When `provider` is `Some`, an LLM produces an anchored structured summary
/// (with the previous summary carried forward for incremental updates); on any
/// failure it falls back to the deterministic excerpt summary. When `provider`
/// is `None`, the excerpt summary is used directly. `hooks.pre_compact` may
/// veto the run or supply extra context.
pub async fn run_compaction(
    history: &mut Vec<Message>,
    max_chars: usize,
    preserve_turns: usize,
    provider: Option<Arc<dyn Provider>>,
    hooks: &dyn CompactionHooks,
) -> Result<Option<CompactionResult>, String> {
    let decision = hooks.pre_compact(history).await;
    if !decision.proceed {
        return Ok(None);
    }

    let before_chars = estimate_chars(history);
    let before_tokens = estimate_tokens(history);
    let Some(selection) = select_compaction(history, preserve_turns) else {
        return Ok(None);
    };

    let budget = summary_budget(max_chars);
    let summary = match provider.as_ref() {
        Some(provider) => {
            match summarize_with_provider(
                provider,
                &selection.archived,
                selection.previous_summary.as_deref(),
                &decision.extra_context,
                budget,
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
                        budget,
                        selection.previous_summary.as_deref(),
                    )
                }
            }
        }
        None => build_excerpt_summary(
            &selection.archived,
            budget,
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
    hooks.post_compact(&result.checkpoint).await;
    let active = result.active.clone();
    *history = active;
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

/// Resolve the on-disk archive directory that sits alongside `path` (its
/// parent's `sessions/` sibling). Used by [`SessionStore::for_path`] so
/// callers stay isolated under their own temp directory regardless of the
/// global [`paths::Dirs`].
fn session_archive_dir(path: &std::path::Path) -> PathBuf {
    path.parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("sessions")
}

/// One-shot migration from the Phase 1 flat `data_dir/sessions/<uuid>.json`
/// layout to the Phase 2 per-project buckets `data_dir/projects/<hash>/...`.
///
/// Idempotent: a marker file (`data_dir/.migrated-v3`) is written on success
/// and prevents re-running. Each legacy archive is routed to the bucket of its
/// own `project_root` field (defaulting to the current cwd when missing, which
/// matches the `SessionData::default` semantic for legacy snapshots). Files
/// already present at the destination are not overwritten.
///
/// Errors are logged but non-fatal — the worst case is some legacy sessions
/// remaining in the flat directory, still readable on rollback to Phase 1.
pub fn migrate_flat_sessions_to_project_buckets(
    dirs: &paths::Dirs,
    blob_store: &BlobStore,
) -> Result<(), String> {
    let marker = dirs.data_dir.join(".migrated-v3");
    if marker.exists() {
        return Ok(());
    }
    let legacy_active = dirs.data_dir.join("session.json");
    let legacy_archive = dirs.legacy_sessions_dir();
    if !legacy_active.exists() && !legacy_archive.is_dir() {
        // Fresh install or already migrated. Stamp the marker.
        let _ = fsutil::atomic_write_bytes(&marker, b"nothing-to-migrate\n");
        return Ok(());
    }

    let route =
        |raw: &str, fallback_root: &std::path::Path| -> Option<(std::path::PathBuf, SessionData)> {
            let data: SessionData = serde_json::from_str(raw).ok()?;
            let root = if data.project_root.as_os_str().is_empty() {
                fallback_root.to_path_buf()
            } else {
                data.project_root.clone()
            };
            Some((dirs.project_dir(&root), data))
        };

    let mut migrated = 0usize;

    // 1. Legacy active session.json → its own bucket (becomes that bucket's active).
    if let Ok(raw) = fs::read_to_string(&legacy_active) {
        match route(&raw, &default_project_root()) {
            Some((dest_dir, data)) => {
                let dest = dest_dir.join("session.json");
                if !dest.exists() {
                    let _ = fs::create_dir_all(dest_dir.join("sessions"));
                    if write_session_file(&dest, &data, blob_store).is_ok() {
                        migrated += 1;
                    }
                }
            }
            None => {
                let parse_err = serde_json::from_str::<SessionData>(&raw)
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                tracing::warn!(error = %parse_err, "could not route legacy active session");
            }
        }
    }

    // 2. Legacy archives → their own bucket's sessions/ subdir.
    if let Ok(entries) = fs::read_dir(&legacy_archive) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Some((dest_dir, data)) = route(&raw, &default_project_root()) else {
                tracing::warn!(path = %path.display(), "could not route legacy archive");
                continue;
            };
            let dest = dest_dir.join("sessions").join(format!("{}.json", data.id));
            if dest.exists() {
                continue;
            }
            let _ = fs::create_dir_all(dest.parent().unwrap_or(&dest_dir));
            if write_session_file(&dest, &data, blob_store).is_ok() {
                migrated += 1;
                let _ = fs::remove_file(&path);
            }
        }
    }

    tracing::info!(migrated, "migrated legacy flat sessions to project buckets");
    let _ = fsutil::atomic_write_bytes(
        &marker,
        format!(
            "migrated-at-{}\n",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        )
        .as_bytes(),
    );
    Ok(())
}

/// Diagnostic scan of stored session files. When `project_root` is `None` every
/// project bucket and the legacy flat archive are inspected; when supplied only
/// that project's bucket is checked. Prints one line per file and a summary.
pub async fn run_doctor(project_root: Option<&std::path::Path>) -> Result<(), String> {
    struct Report {
        examined: usize,
        corrupt: usize,
        legacy: usize,
    }

    impl Report {
        fn record(&mut self, path: &std::path::Path, result: Result<&SessionData, String>) {
            self.examined += 1;
            match result {
                Ok(data) => {
                    let message_count = data.messages.len() + data.archived_messages.len();
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
        let active = path.join("session.json");
        if active.exists() {
            inspect(&active, report);
        }
        let archive_dir = path.join("sessions");
        if let Ok(entries) = fs::read_dir(&archive_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                inspect(&path, report);
            }
        }
    }

    let dirs = paths::get();
    let mut report = Report {
        examined: 0,
        corrupt: 0,
        legacy: 0,
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
        if dirs.legacy_sessions_dir().is_dir() {
            if let Ok(entries) = fs::read_dir(dirs.legacy_sessions_dir()) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("json") {
                        continue;
                    }
                    report.legacy += 1;
                    inspect(&path, &mut report);
                }
            }
        }
    }

    println!("---");
    println!(
        "examined: {}, corrupt: {}, legacy flat archives: {}",
        report.examined, report.corrupt, report.legacy
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let store = SessionStore {
            project_root: directory.clone(),
            path: path.clone(),
            archive_dir: directory.join("sessions"),
            event_log: EventLog::new(path.with_extension("jsonl")),
            blob_store: BlobStore::new(path.parent().unwrap().join("blobs")),
            data: Mutex::new(SessionData::default()),
        };
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
        assert_eq!(data.messages[0].content, messages[0].content);
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
        data.messages = vec![Message::new(neenee_core::Role::User, "hello")];
        data.checksum = Some(compute_checksum(&data));
        assert!(verify_checksum(&data).is_ok());

        // Tamper with a field: verification must fail.
        data.messages[0].content = "goodbye".to_string();
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
    async fn session_persists_subagent_children_round_trip() {
        // End-to-end persistence contract: a session that contains a `task`
        // tool call must round-trip the sub-agent's nested transcript through
        // session.json, so a subsequent `SessionStore::load_for_project` (the
        // production resume path) restores the children intact. Before Phase 3
        // children were silently dropped because `Message::children` did not
        // exist and the harness only persisted the textual summary.
        let directory =
            std::env::temp_dir().join(format!("neenee-subagent-persist-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore {
            project_root: directory.clone(),
            path: path.clone(),
            archive_dir: directory.join("sessions"),
            event_log: EventLog::new(path.with_extension("jsonl")),
            blob_store: BlobStore::new(path.parent().unwrap().join("blobs")),
            data: Mutex::new(SessionData::default()),
        };

        let call = neenee_core::ToolCall {
            id: "call_sub1".to_string(),
            name: "task".to_string(),
            arguments: r#"{"description":"d","prompt":"p"}"#.to_string(),
        };
        let assistant = Message::new(neenee_core::Role::Assistant, "")
            .with_attribution("kimi-code", "kimi-k2.7-code");
        let assistant = Message {
            tool_calls: Some(vec![call.clone()]),
            ..assistant
        };
        let subagent_transcript = vec![
            Message::new(neenee_core::Role::User, "find foo"),
            Message::new(neenee_core::Role::Assistant, "looking..."),
            Message::new(neenee_core::Role::Assistant, "foo is at src/foo.rs"),
        ];
        let tool = Message::tool_result(&call, "[task result]:\nfoo is at src/foo.rs")
            .with_children(subagent_transcript);
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
            .messages
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
            assert!(dirs
                .project_dir(&PathBuf::from("/projects/alpha"))
                .join("session.json")
                .exists());
            assert!(dirs
                .project_dir(&PathBuf::from("/projects/beta"))
                .join("session.json")
                .exists());

            // Reloading alpha does not see beta's messages.
            let reloaded_a = SessionStore::load_for_project(PathBuf::from("/projects/alpha"));
            assert_eq!(reloaded_a.messages().await[0].content, "alpha work");
            let reloaded_b = SessionStore::load_for_project(PathBuf::from("/projects/beta"));
            assert_eq!(reloaded_b.messages().await[0].content, "beta work");

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
    async fn migrate_flat_sessions_buckets_by_project_root() {
        locked!({
            let root =
                std::env::temp_dir().join(format!("neenee-flat-migrate-{}", uuid::Uuid::new_v4()));
            let dirs = paths::Dirs::resolve(&paths::PathsOverride {
                data_dir: Some(root.join("data")),
                state_dir: Some(root.join("state")),
                config_dir: Some(root.join("config")),
                cache_dir: Some(root.join("cache")),
            });
            dirs.ensure().unwrap();
            paths::set_test_default(Some(dirs.clone()));

            let legacy_dir = dirs.legacy_sessions_dir();
            std::fs::create_dir_all(&legacy_dir).unwrap();
            let alpha_active = SessionData {
                project_root: PathBuf::from("/projects/alpha"),
                ..SessionData::default()
            };
            fsutil::atomic_write_json(&dirs.data_dir.join("session.json"), &alpha_active).unwrap();
            let alpha_archive = SessionData {
                id: "aaaaaaaa-0000-0000-0000-000000000001".to_string(),
                project_root: PathBuf::from("/projects/alpha"),
                ..SessionData::default()
            };
            let beta_archive = SessionData {
                id: "bbbbbbbb-0000-0000-0000-000000000002".to_string(),
                project_root: PathBuf::from("/projects/beta"),
                ..SessionData::default()
            };
            fsutil::atomic_write_json(
                &legacy_dir.join(format!("{}.json", alpha_archive.id)),
                &alpha_archive,
            )
            .unwrap();
            fsutil::atomic_write_json(
                &legacy_dir.join(format!("{}.json", beta_archive.id)),
                &beta_archive,
            )
            .unwrap();

            let _ = SessionStore::load_for_project(PathBuf::from("/projects/alpha"));

            let alpha_dir = dirs.project_dir(&PathBuf::from("/projects/alpha"));
            assert!(alpha_dir.join("session.json").exists());
            assert!(alpha_dir
                .join("sessions")
                .join(format!("{}.json", alpha_archive.id))
                .exists());
            let beta_dir = dirs.project_dir(&PathBuf::from("/projects/beta"));
            assert!(!beta_dir.join("session.json").exists());
            assert!(beta_dir
                .join("sessions")
                .join(format!("{}.json", beta_archive.id))
                .exists());
            assert!(!legacy_dir
                .join(format!("{}.json", alpha_archive.id))
                .exists());
            assert!(!legacy_dir
                .join(format!("{}.json", beta_archive.id))
                .exists());
            assert!(dirs.data_dir.join(".migrated-v3").exists());

            paths::set_test_default(None);
            let _ = std::fs::remove_dir_all(root);
        });
    }

    #[tokio::test]
    async fn fork_preserves_both_durable_branches() {
        let directory =
            std::env::temp_dir().join(format!("neenee-session-fork-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore {
            project_root: directory.clone(),
            path: path.clone(),
            archive_dir: directory.join("sessions"),
            event_log: EventLog::new(path.with_extension("jsonl")),
            blob_store: BlobStore::new(path.parent().unwrap().join("blobs")),
            data: Mutex::new(SessionData::default()),
        };
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
        assert!(sessions
            .iter()
            .any(|item| item.id == fork_id && item.active));

        store.open(&parent_id[..8]).await.unwrap();
        assert_eq!(store.messages().await[0].content, "parent");
        store.open(&fork_id[..8]).await.unwrap();
        assert_eq!(store.messages().await[0].content, "fork");
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
        let store = SessionStore {
            project_root: directory.clone(),
            path: path.clone(),
            archive_dir: directory.join("sessions"),
            event_log: EventLog::new(path.with_extension("jsonl")),
            blob_store: BlobStore::new(path.parent().unwrap().join("blobs")),
            data: Mutex::new(SessionData::default()),
        };

        // Seed one archived session so the picker has something to show,
        // then keep the active session empty (the default state).
        let archived = SessionData {
            project_root: directory.clone(),
            messages: vec![Message::new(neenee_core::Role::User, "archived branch")],
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
    async fn plan_state_round_trips_through_disk() {
        let directory =
            std::env::temp_dir().join(format!("neenee-plan-state-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore {
            project_root: directory.clone(),
            path: path.clone(),
            archive_dir: directory.join("sessions"),
            event_log: EventLog::new(path.with_extension("jsonl")),
            blob_store: BlobStore::new(path.parent().unwrap().join("blobs")),
            data: Mutex::new(SessionData::default()),
        };
        assert_eq!(store.active_plan_path().await, None);
        assert_eq!(store.plan_progress().await, None);

        let plan = PathBuf::from(".neenee/plans/feature-x.md");
        store
            .set_active_plan_path(Some(plan.clone()))
            .await
            .unwrap();

        let progress =
            neenee_core::PlanProgress::from_markdown(plan.clone(), "## Summary\n## Key Changes\n");
        store
            .set_plan_progress(Some(progress.clone()))
            .await
            .unwrap();

        // Reload from disk and confirm both values round-trip.
        let reloaded = SessionStore::for_path(path.clone());
        assert_eq!(
            reloaded.active_plan_path().await.as_deref(),
            Some(plan.as_path()),
            "active plan path should round-trip through disk"
        );
        let loaded_progress = reloaded.plan_progress().await.expect("progress persisted");
        assert_eq!(loaded_progress.path, plan);
        assert_eq!(loaded_progress.sections.len(), 2);

        // Clearing also persists.
        reloaded.set_active_plan_path(None).await.unwrap();
        reloaded.set_plan_progress(None).await.unwrap();
        assert_eq!(reloaded.active_plan_path().await, None);
        assert_eq!(reloaded.plan_progress().await, None);

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn todos_round_trip_through_disk() {
        let directory =
            std::env::temp_dir().join(format!("neenee-todos-state-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore {
            project_root: directory.clone(),
            path: path.clone(),
            archive_dir: directory.join("sessions"),
            event_log: EventLog::new(path.with_extension("jsonl")),
            blob_store: BlobStore::new(path.parent().unwrap().join("blobs")),
            data: Mutex::new(SessionData::default()),
        };
        assert!(store.todos().await.is_empty());

        // Seed via the domain helper (same path plan_exit will use) and persist.
        let mut list = neenee_core::TodoList::from_plan_markdown(
            "## Summary\n## Key Changes\n## Test Plan\n",
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
        reloaded.set_todos(neenee_core::TodoList::default()).await.unwrap();
        assert!(reloaded.todos().await.is_empty());

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn startup_new_session_can_resume_most_recent_cache() {
        let directory =
            std::env::temp_dir().join(format!("neenee-session-resume-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore {
            project_root: directory.clone(),
            path: path.clone(),
            archive_dir: directory.join("sessions"),
            event_log: EventLog::new(path.with_extension("jsonl")),
            blob_store: BlobStore::new(path.parent().unwrap().join("blobs")),
            data: Mutex::new(SessionData::default()),
        };
        store
            .replace_messages(vec![Message::new(neenee_core::Role::User, "previous")])
            .await
            .unwrap();
        let previous_id = store.id().await;

        let new_id = store.reset().await.unwrap();
        assert_ne!(new_id, previous_id);
        assert!(store.messages().await.is_empty());

        let resumed_id = store.resume(None).await.unwrap();
        assert_eq!(resumed_id, previous_id);
        assert_eq!(store.messages().await[0].content, "previous");
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
        corrupted.messages[0].content = "tampered".to_string();
        let test_blobs = BlobStore::new(directory.join("blobs"));
        write_session_file(&path, &corrupted, &test_blobs).unwrap();

        // Re-open: for_path replays the event log, not the snapshot.
        let reloaded = SessionStore::for_path(path.clone());
        assert_eq!(reloaded.id().await, first_id);
        assert_eq!(reloaded.messages().await[0].content, "first");

        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn snapshot_without_event_log_gets_imported() {
        locked!({
            let root = std::env::temp_dir()
                .join(format!("neenee-snapshot-import-{}", uuid::Uuid::new_v4()));
            let dirs = paths::Dirs::resolve(&paths::PathsOverride {
                data_dir: Some(root.join("data")),
                state_dir: Some(root.join("state")),
                config_dir: Some(root.join("config")),
                cache_dir: Some(root.join("cache")),
            });
            dirs.ensure().unwrap();
            paths::set_test_default(Some(dirs.clone()));

            let project_root = PathBuf::from("/projects/event-import-test");
            let project_dir = dirs.project_dir(&project_root);
            let path = project_dir.join("session.json");
            let event_path = project_dir.join("events.jsonl");
            std::fs::create_dir_all(&project_dir).unwrap();

            let snapshot = SessionData {
                id: "00000000-0000-0000-0000-000000000001".to_string(),
                messages: vec![Message::new(neenee_core::Role::User, "from snapshot")],
                ..Default::default()
            };
            let blob_store = BlobStore::new(dirs.blobs_dir());
            write_session_file(&path, &snapshot, &blob_store).unwrap();

            let store = SessionStore::load_for_project(project_root);
            assert_eq!(store.id().await, snapshot.id);
            assert_eq!(store.messages().await[0].content, "from snapshot");
            assert!(
                event_path.exists(),
                "event log should be seeded from snapshot"
            );

            paths::set_test_default(None);
            let _ = std::fs::remove_dir_all(root);
        });
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
        let messages = reloaded.messages().await;
        assert_eq!(messages[0].content, big);
        assert!(
            messages[0].content_blob.is_none(),
            "memory uses inline content"
        );

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

        assert_eq!(result.active[0].role, neenee_core::Role::User);
        assert!(result.active[0].hidden);
        assert_eq!(result.active[1].content, "recent question");
        assert_eq!(result.active[2].content, "recent answer");
        assert!(result
            .archived
            .iter()
            .any(|message| message.content == "old tool result"));
        assert!(!result
            .archived
            .iter()
            .any(|message| message.role == neenee_core::Role::System));
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
        assert!(selection
            .archived
            .iter()
            .any(|message| message.content.starts_with("[Conversation checkpoint]")));
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

        let result = run_compaction(
            &mut history,
            10_000,
            1,
            Some(provider),
            &NoopCompactionHooks,
        )
        .await
        .unwrap()
        .unwrap();

        // The mock provider's canned reply becomes the checkpoint summary.
        assert!(result.active[0].content.contains("mock AI"));
        assert_eq!(result.active[1].content, "recent question");
        assert!(result.active[0].hidden);
    }

    #[tokio::test]
    async fn run_compaction_vetoed_by_hook_leaves_history_untouched() {
        struct VetoHooks;
        #[async_trait]
        impl CompactionHooks for VetoHooks {
            async fn pre_compact(&self, _messages: &[Message]) -> CompactionDecision {
                CompactionDecision::veto()
            }
        }

        let original = vec![
            Message::new(Role::System, "system"),
            Message::new(Role::User, "old question"),
            Message::new(Role::Assistant, "old answer"),
            Message::new(Role::User, "recent question"),
            Message::new(Role::Assistant, "recent answer"),
        ];
        let mut history = original.clone();

        let outcome = run_compaction(&mut history, 10_000, 1, None, &VetoHooks)
            .await
            .unwrap();

        assert!(outcome.is_none());
        assert_eq!(history.len(), original.len());
        assert_eq!(
            history.last().unwrap().content,
            original.last().unwrap().content
        );
        assert!(history.iter().all(|message| !message.hidden
            || message.role != Role::User
            || !message.content.starts_with("[Conversation checkpoint]")));
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

        let result = run_compaction(
            &mut history,
            10_000,
            1,
            Some(provider),
            &NoopCompactionHooks,
        )
        .await
        .unwrap()
        .unwrap();

        // Fallback excerpt summary references the old question.
        assert!(result.active[0].content.contains("old question"));
        // Silence the unused MockProvider import warning while keeping the path
        // documented for the success-case test above.
        let _: &dyn Provider = &MockProvider;
    }
}

use directories::ProjectDirs;
use neenee_core::Message;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoopCheckpoint {
    pub goal: String,
    pub iteration: usize,
    pub max_iterations: usize,
    pub status: String,
}

impl LoopCheckpoint {
    pub fn resume_iteration(&self) -> Result<usize, String> {
        if !(1..=50).contains(&self.max_iterations) {
            return Err(format!(
                "Checkpoint has invalid iteration budget {}.",
                self.max_iterations
            ));
        }
        if matches!(self.status.as_str(), "completed" | "exhausted") {
            return Err(format!(
                "Checkpoint is {} and has no unfinished iteration to resume.",
                self.status
            ));
        }
        if !matches!(self.status.as_str(), "running" | "interrupted" | "error") {
            return Err(format!("Checkpoint has unknown status '{}'.", self.status));
        }
        let iteration = self.iteration.max(1);
        if iteration > self.max_iterations {
            return Err(format!(
                "Checkpoint iteration {} exceeds its budget {}.",
                iteration, self.max_iterations
            ));
        }
        Ok(iteration)
    }
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
    loop_checkpoint: Option<LoopCheckpoint>,
    compaction: Option<CompactionCheckpoint>,
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub parent_id: Option<String>,
    pub message_count: usize,
    pub updated_at: u64,
    pub active: bool,
}

pub struct SessionStore {
    path: PathBuf,
    archive_dir: PathBuf,
    data: Mutex<SessionData>,
}

impl SessionStore {
    pub fn load() -> Self {
        let path = session_file_path();
        let archive_dir = session_archive_dir(&path);
        let data = fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default();
        Self {
            path,
            archive_dir,
            data: Mutex::new(data),
        }
    }

    #[cfg(test)]
    pub fn for_path(path: PathBuf) -> Self {
        Self {
            archive_dir: session_archive_dir(&path),
            path,
            data: Mutex::new(SessionData::default()),
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

    pub async fn checkpoint(&self) -> Option<LoopCheckpoint> {
        self.data.lock().await.loop_checkpoint.clone()
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
        self.persist(&data)
    }

    pub async fn set_checkpoint(&self, checkpoint: Option<LoopCheckpoint>) -> Result<(), String> {
        let mut data = self.data.lock().await;
        data.loop_checkpoint = checkpoint;
        data.updated_at = unix_timestamp();
        self.persist(&data)
    }

    pub async fn commit_compaction(&self, result: CompactionResult) -> Result<(), String> {
        let mut data = self.data.lock().await;
        data.archived_messages.extend(result.archived);
        data.messages = result.active;
        data.compaction = Some(result.checkpoint);
        data.updated_at = unix_timestamp();
        self.persist(&data)
    }

    pub async fn reset(&self) -> Result<String, String> {
        let mut data = self.data.lock().await;
        if has_content(&data) {
            self.persist_archive(&data)?;
        }
        *data = SessionData::default();
        let id = data.id.clone();
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
        let parent_id = data.id.clone();
        let now = unix_timestamp();
        data.id = uuid::Uuid::new_v4().to_string();
        data.parent_id = Some(parent_id.clone());
        data.created_at = now;
        data.updated_at = now;
        data.loop_checkpoint = None;
        let fork_id = data.id.clone();
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
            }
            let path = self.archive_path(&id);
            let content = fs::read_to_string(&path)
                .map_err(|error| format!("Could not open session '{}': {}", id, error))?;
            let loaded: SessionData =
                serde_json::from_str(&content).map_err(|error| error.to_string())?;
            data.clone_from(&loaded);
            data.updated_at = unix_timestamp();
            self.persist(&data)?;
        }
        Ok(())
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
        summaries.push(summary(&data, true));
        summaries.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
        Ok(summaries)
    }

    fn persist(&self, data: &SessionData) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let content = serde_json::to_string_pretty(data).map_err(|error| error.to_string())?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, content).map_err(|error| error.to_string())?;
        fs::rename(temporary, &self.path).map_err(|error| error.to_string())
    }

    fn persist_archive(&self, data: &SessionData) -> Result<(), String> {
        fs::create_dir_all(&self.archive_dir).map_err(|error| error.to_string())?;
        let path = self.archive_path(&data.id);
        let temporary = path.with_extension("json.tmp");
        let content = serde_json::to_string_pretty(data).map_err(|error| error.to_string())?;
        fs::write(&temporary, content).map_err(|error| error.to_string())?;
        fs::rename(temporary, path).map_err(|error| error.to_string())
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
        active,
    }
}

fn unix_timestamp() -> u64 {
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

pub fn estimate_chars(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            message.content.len()
                + message
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|call| call.name.len() + call.arguments.len())
                            .sum::<usize>()
                    })
                    .unwrap_or(0)
        })
        .sum()
}

pub fn discard_trailing_loop_prompts(messages: &mut Vec<Message>) -> usize {
    let original_len = messages.len();
    while messages.last().is_some_and(|message| {
        message.hidden
            && message.role == neenee_core::Role::User
            && message
                .content
                .starts_with("Autonomous goal loop iteration ")
    }) {
        messages.pop();
    }
    original_len - messages.len()
}

pub fn compact_messages(
    messages: &[Message],
    max_chars: usize,
    preserve_turns: usize,
) -> Option<CompactionResult> {
    let before_chars = estimate_chars(messages);
    let user_indices = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == neenee_core::Role::User
                && !message.content.starts_with("[Conversation checkpoint]")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if user_indices.len() <= preserve_turns {
        return None;
    }

    let keep_from = user_indices[user_indices.len() - preserve_turns];
    let archived = messages[..keep_from]
        .iter()
        .filter(|message| message.role != neenee_core::Role::System)
        .cloned()
        .collect::<Vec<_>>();
    if archived.is_empty() {
        return None;
    }

    let summary_budget = (max_chars / 8).clamp(2_000, 16_000);
    let summary = build_summary(&archived, summary_budget);
    let mut active = vec![Message::hidden(
        neenee_core::Role::User,
        format!(
            "[Conversation checkpoint]\nEarlier complete turns were compacted. \
             Treat this as durable context, not a new user request.\n\n{}",
            summary
        ),
    )];
    active.extend_from_slice(&messages[keep_from..]);
    let after_chars = estimate_chars(&active);

    Some(CompactionResult {
        checkpoint: CompactionCheckpoint {
            archived_messages: archived.len(),
            active_messages: active.len(),
            before_chars,
            after_chars,
        },
        active,
        archived,
    })
}

fn build_summary(messages: &[Message], max_chars: usize) -> String {
    let mut output = String::new();
    for message in messages {
        let label = match message.role {
            neenee_core::Role::User => "User",
            neenee_core::Role::Assistant => "Assistant",
            neenee_core::Role::Tool => "Tool",
            neenee_core::Role::System => continue,
        };
        let content = message.content.trim();
        if content.is_empty() {
            continue;
        }
        let remaining = max_chars.saturating_sub(output.len());
        if remaining < 64 {
            break;
        }
        let excerpt = truncate_utf8(content, remaining.min(1_500));
        output.push_str(label);
        output.push_str(": ");
        output.push_str(excerpt);
        output.push_str("\n\n");
    }
    output.trim_end().to_string()
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

fn session_file_path() -> PathBuf {
    let project =
        ProjectDirs::from("ai", "neenee", "neenee").expect("Could not determine config directory");
    project.config_dir().join("session.json")
}

fn session_archive_dir(path: &std::path::Path) -> PathBuf {
    path.parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_data_round_trips() {
        let directory =
            std::env::temp_dir().join(format!("neenee-session-test-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore {
            path: path.clone(),
            archive_dir: directory.join("sessions"),
            data: Mutex::new(SessionData::default()),
        };
        let messages = vec![Message::new(neenee_core::Role::User, "hello")];
        store.replace_messages(messages.clone()).await.unwrap();
        store
            .set_checkpoint(Some(LoopCheckpoint {
                goal: "test".to_string(),
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
    }

    #[tokio::test]
    async fn fork_preserves_both_durable_branches() {
        let directory =
            std::env::temp_dir().join(format!("neenee-session-fork-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore {
            path,
            archive_dir: directory.join("sessions"),
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
    async fn startup_new_session_can_resume_most_recent_cache() {
        let directory =
            std::env::temp_dir().join(format!("neenee-session-resume-{}", uuid::Uuid::new_v4()));
        let path = directory.join("session.json");
        let store = SessionStore {
            path,
            archive_dir: directory.join("sessions"),
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
    fn resume_discards_only_trailing_loop_control_prompts() {
        let mut messages = vec![
            Message::new(neenee_core::Role::User, "real request"),
            Message::hidden(
                neenee_core::Role::User,
                "Autonomous goal loop iteration 2/8.\nGoal: ship",
            ),
        ];

        assert_eq!(discard_trailing_loop_prompts(&mut messages), 1);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "real request");
    }

    #[test]
    fn resume_iteration_accepts_only_unfinished_valid_checkpoints() {
        let unfinished = LoopCheckpoint {
            goal: "ship".to_string(),
            iteration: 3,
            max_iterations: 8,
            status: "interrupted".to_string(),
        };
        assert_eq!(unfinished.resume_iteration().unwrap(), 3);

        let initial = LoopCheckpoint {
            iteration: 0,
            status: "running".to_string(),
            ..unfinished.clone()
        };
        assert_eq!(initial.resume_iteration().unwrap(), 1);

        for status in ["completed", "exhausted", "unknown"] {
            let checkpoint = LoopCheckpoint {
                status: status.to_string(),
                ..unfinished.clone()
            };
            assert!(checkpoint.resume_iteration().is_err());
        }
    }
}

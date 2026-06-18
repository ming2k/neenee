use directories::ProjectDirs;
use neenee_core::async_trait;
use neenee_core::{Message, Provider, Role};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

// Re-export the cheap context estimators so callers keep using
// `session::estimate_chars` / `session::estimate_tokens`.
pub use neenee_core::{estimate_chars, estimate_tokens};

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
    pub created_at: u64,
    /// Short description of what the session is about (first user message or
    /// the active goal), already truncated for display.
    pub overview: String,
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
            let mut data = self.data.lock().await;
            *data = SessionData::default();
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
        created_at: data.created_at,
        overview: session_overview(data),
        active,
    }
}

/// Derive a short, human-readable description of a session: the first user
/// message, falling back to the active goal, then to a placeholder.
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
        return truncate_preview(&checkpoint.goal, MAX);
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
        let label = label_for(message.role).unwrap();
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
    let summary = build_excerpt_summary(&selection.archived, budget, selection.previous_summary.as_deref());
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
## Goal\n\
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
        None => parts.push(
            "Create a new anchored summary from the conversation history below."
                .to_string(),
        ),
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
    let response = provider.chat(messages).await?;
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

pub fn goals_db_path() -> PathBuf {
    let project =
        ProjectDirs::from("ai", "neenee", "neenee").expect("Could not determine config directory");
    project.config_dir().join("goals.db")
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
        assert_eq!(selection.previous_summary.as_deref(), Some("prev summary body"));
        // The prior checkpoint lands in the archived head, not the tail.
        assert!(selection
            .archived
            .iter()
            .any(|message| message
                .content
                .starts_with("[Conversation checkpoint]")));
        assert_eq!(selection.tail.last().unwrap().content, "a2");
    }

    #[tokio::test]
    async fn run_compaction_uses_provider_summary() {
        use neenee_core::providers::MockProvider;

        let mut history = vec![
            Message::new(Role::System, "system"),
            Message::new(Role::User, "old question"),
            Message::new(Role::Assistant, "old answer"),
            Message::new(Role::User, "recent question"),
            Message::new(Role::Assistant, "recent answer"),
        ];
        let provider: Arc<dyn Provider> = Arc::new(MockProvider);

        let result = run_compaction(&mut history, 10_000, 1, Some(provider), &NoopCompactionHooks)
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
        use neenee_core::providers::MockProvider;

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
            ) -> Result<futures::stream::BoxStream<'static, Result<String, String>>, String> {
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

        let result = run_compaction(&mut history, 10_000, 1, Some(provider), &NoopCompactionHooks)
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

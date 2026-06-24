//! Unified task list (todos) — the single source of truth for "what is the
//! agent working on and what is left to do."
//!
//! One list, one sticky panel, one persisted field. There is no longer a
//! parallel "plan progress" tracker: the [`crate::plan`] module owns only the
//! Plan *Mode* workflow (`plan_enter` / `plan_exit` / the plan document), and
//! on `plan_exit` it seeds a [`TodoList`] (see [`TodoList::from_plan_markdown`])
//! rather than maintaining its own progress type. Entering Plan mode clears
//! the list.
//!
//! Architecture: domain types live here, a shared [`TodoToolContext`] holds
//! the `Arc<Mutex<TodoList>>` that the host `Agent` (in `neenee-agent`) also
//! owns, and the tools mutate that shared cell so a call takes effect
//! immediately for the next system prompt and the TUI. The same context is
//! embedded in [`crate::plan::PlanToolContext`] so the plan workflow can
//! reseed/clear the list. The harness mirrors the cell back into the session
//! each turn (event-sourced) and replays it on resume.
//!
//! ## Identity vs. display
//! [`TodoItem::id`] is a stable, monotonic identifier used for persistence
//! and reconciliation (so an item keeps its `created_at` when the model
//! re-sends the same content). It is *not* a display number: the model's
//! `todo` tool takes the whole desired list each call (robust against
//! identity-tracking failures), and `todo_update` / the `/todos` command
//! refer to items by 1-based position or by content substring — never by
//! internal id — so the display can reorder or shrink without breaking
//! references.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Tool, ToolAccess};

/// Hard cap on the number of items in a [`TodoList`]. Matches the historical
/// `todo` tool contract so existing prompts and behavior stay valid.
pub const MAX_TODOS: usize = 50;

/// Number of harness turns the todos panel may go without any change before
/// the TUI renders it dimmed with a "not updated for N turns" hint: a model
/// that abandons the list after the first item should not display
/// trustworthy-looking checks.
pub const TODO_STALE_TURN_THRESHOLD: u64 = 5;

/// Stable, monotonic identifier for a single todo item. Opaque to callers —
/// display and references use position/content, not this value. Serialized
/// transparently so persisted lists stay compact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TodoId(pub u64);

/// Lifecycle of a single todo item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    /// Single-character glyph used by the TUI panel.
    pub fn glyph(self) -> &'static str {
        match self {
            TodoStatus::Completed => "✓",
            TodoStatus::InProgress => "●",
            TodoStatus::Pending => "○",
            TodoStatus::Cancelled => "✕",
        }
    }

    /// Wire string used by the `todo` / `todo_update` tools' `status` field.
    pub fn as_str(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
            TodoStatus::Cancelled => "cancelled",
        }
    }

    /// Bracket glyph used in plain-text rendering (tool results, `/todos`
    /// text output). Distinct from [`glyph`](Self::glyph) (panel style) to
    /// match the historical `todo` tool output the model is accustomed to.
    pub fn bracket(self) -> &'static str {
        match self {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Completed => "[x]",
            TodoStatus::Cancelled => "[-]",
        }
    }

    /// Parse a status from its wire string. Accepts the legacy plan-progress
    /// spellings (`done`, `skipped`) so a model trained on the old vocabulary
    /// still works, mapping them onto the unified statuses.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "pending" => Self::Pending,
            "in_progress" => Self::InProgress,
            "completed" | "done" => Self::Completed,
            "cancelled" | "skipped" => Self::Cancelled,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: TodoId,
    pub content: String,
    pub status: TodoStatus,
    /// Unix-epoch seconds when the item was first created. Preserved across
    /// reconciliations (the model re-sending the same content does not reset
    /// the clock).
    #[serde(default)]
    pub created_at: u64,
    /// Unix-epoch seconds of the last status change. Bumped only when the
    /// status actually differs, not on every no-op reconcile.
    #[serde(default)]
    pub updated_at: u64,
}

/// The whole task list. Display order is the [`Vec`] order; identity is
/// [`TodoItem::id`]. Invariants (unique ids, ≤ [`MAX_TODOS`] items, ≤ one
/// `InProgress`) are enforced by the constructors and mutators, never by
/// callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoList {
    #[serde(default)]
    pub items: Vec<TodoItem>,
    /// Next id to hand out. Monotonic for the life of the list so ids are
    /// never reused after a removal, even across save/resume.
    #[serde(default = "default_next_id")]
    next_id: u64,
    /// Harness turn counter value the last time the list changed. The TUI
    /// compares this against the current turn counter to flag a stale panel.
    #[serde(default)]
    pub updated_at_turn: u64,
}

fn default_next_id() -> u64 {
    1
}

impl Default for TodoList {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
            updated_at_turn: 0,
        }
    }
}

impl TodoList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Number of items in a given status (used for the panel summary
    /// `done_count/total`).
    pub fn count(&self, status: TodoStatus) -> usize {
        self.items.iter().filter(|i| i.status == status).count()
    }

    fn allocate_id(&mut self) -> TodoId {
        let id = TodoId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Replace the whole list with `desired`, preserving identity for items
    /// whose content is unchanged (first unused match wins). New content gets
    /// a fresh id; dropped items vanish from the live list (their history is
    /// retained in the session event log). Returns `true` if anything
    /// changed.
    ///
    /// `desired` is assumed already validated (non-empty content, ≤
    /// [`MAX_TODOS`], ≤ one `InProgress`) — validation lives in the callers
    /// so the user-facing `/todos` command and the model-facing `todo` tool
    /// can surface violations with their own appropriate error shape.
    pub fn reconcile(&mut self, desired: &[(String, TodoStatus)], now: u64, turn: u64) -> bool {
        // Snapshot current items so id allocation (which mutates self) does
        // not fight the immutable lookups used to match identity.
        let current: Vec<TodoItem> = self.items.clone();
        let mut used: HashSet<u64> = HashSet::new();
        let mut next_id = self.next_id;

        let new_items: Vec<TodoItem> = desired
            .iter()
            .map(|(content, status)| {
                if let Some(mut item) = current
                    .iter()
                    .find(|it| it.content == *content && !used.contains(&it.id.0))
                    .map(|m| {
                        used.insert(m.id.0);
                        m.clone()
                    })
                {
                    if item.status != *status {
                        item.status = *status;
                        item.updated_at = now;
                    }
                    item
                } else {
                    let id = TodoId(next_id);
                    next_id += 1;
                    TodoItem {
                        id,
                        content: content.clone(),
                        status: *status,
                        created_at: now,
                        updated_at: now,
                    }
                }
            })
            .collect();

        let changed = new_items != self.items;
        if changed {
            self.items = new_items;
            self.next_id = next_id;
            self.updated_at_turn = turn;
        }
        changed
    }

    /// Update the status of items matched by `key`. `key` is either a 1-based
    /// display position (`"1"`, `"3"`) or, when not a valid position, a
    /// case-insensitive substring of the content (all matches updated).
    /// Returns the number of items changed. Stamps `updated_at_turn` only
    /// when at least one item moved.
    pub fn update(&mut self, key: &str, status: TodoStatus, now: u64, turn: u64) -> usize {
        let mut changed = 0;
        let trimmed = key.trim();
        // Position match takes priority when it is a valid 1-based index.
        let pos = trimmed
            .trim_start_matches('#')
            .parse::<usize>()
            .ok()
            .filter(|p| *p >= 1 && *p <= self.items.len());
        if let Some(p) = pos {
            let item = &mut self.items[p - 1];
            if item.status != status {
                item.status = status;
                item.updated_at = now;
                changed += 1;
            }
            if changed > 0 {
                self.updated_at_turn = turn;
            }
            return changed;
        }
        // Fall back to case-insensitive content substring.
        let needle = trimmed.to_lowercase();
        if !needle.is_empty() {
            for item in &mut self.items {
                if item.content.to_lowercase().contains(&needle) && item.status != status {
                    item.status = status;
                    item.updated_at = now;
                    changed += 1;
                }
            }
        }
        if changed > 0 {
            self.updated_at_turn = turn;
        }
        changed
    }

    /// Append a new item as `Pending`. Returns `Err` with a user-facing
    /// message on validation failure (empty content, list full). Used by the
    /// `/todos add` command.
    pub fn add(&mut self, content: String, now: u64, turn: u64) -> Result<(), String> {
        let content = content.trim().to_string();
        if content.is_empty() {
            return Err("Todo item content cannot be empty.".to_string());
        }
        if self.items.len() >= MAX_TODOS {
            return Err(format!("Todo list is limited to {MAX_TODOS} items."));
        }
        let id = self.allocate_id();
        self.items.push(TodoItem {
            id,
            content,
            status: TodoStatus::Pending,
            created_at: now,
            updated_at: now,
        });
        self.updated_at_turn = turn;
        Ok(())
    }

    /// Remove the item matched by `key` (position or content substring, same
    /// rules as [`update`]). Returns the number removed.
    pub fn remove(&mut self, key: &str, turn: u64) -> usize {
        let trimmed = key.trim();
        let pos = trimmed
            .trim_start_matches('#')
            .parse::<usize>()
            .ok()
            .filter(|p| *p >= 1 && *p <= self.items.len());
        if let Some(p) = pos {
            self.items.remove(p - 1);
            self.updated_at_turn = turn;
            return 1;
        }
        let needle = trimmed.to_lowercase();
        if needle.is_empty() {
            return 0;
        }
        let before = self.items.len();
        self.items
            .retain(|it| !it.content.to_lowercase().contains(&needle));
        let removed = before - self.items.len();
        if removed > 0 {
            self.updated_at_turn = turn;
        }
        removed
    }

    pub fn clear(&mut self, turn: u64) {
        if !self.items.is_empty() {
            self.items.clear();
            self.updated_at_turn = turn;
        }
    }

    /// Seed a list from a plan document's `##` headings. Each heading becomes a
    /// `Pending` item, in document order. If the plan has no headings a
    /// single synthetic "Plan" item is used so the panel still renders.
    pub fn from_plan_markdown(content: &str, now: u64, turn: u64) -> Self {
        let headings = parse_plan_headings(content);
        let mut list = TodoList::new();
        list.updated_at_turn = turn;
        let names = if headings.is_empty() {
            vec!["Plan".to_string()]
        } else {
            headings
        };
        for name in names {
            let id = list.allocate_id();
            list.items.push(TodoItem {
                id,
                content: name,
                status: TodoStatus::Pending,
                created_at: now,
                updated_at: now,
            });
        }
        list
    }

    /// Plain-text rendering with 1-based positions and bracket glyphs, the
    /// format the model is trained on for `todo` tool results.
    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return "(empty)".to_string();
        }
        self.items
            .iter()
            .enumerate()
            .map(|(idx, item)| format!("{}. {} {}", idx + 1, item.status.bracket(), item.content))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Extract `## ` (level-2) heading text from a markdown plan. Deeper
/// headings (`### `+) are sub-bullets inside a section and do not become
/// items. Used by [`TodoList::from_plan_markdown`] to seed the list when a
/// plan is approved via `plan_exit`.
fn parse_plan_headings(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("## ") else {
            continue;
        };
        if rest.starts_with('#') {
            continue;
        }
        let name = rest.trim().trim_end_matches('#').trim();
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    out
}

/// Current wall-clock time in Unix-epoch seconds. Shared with [`crate::plan`]
/// so `plan_exit` can stamp `created_at` when seeding a [`TodoList`] from the
/// approved plan markdown.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Shared handle injected into the todo tools so they can read and mutate
/// the live [`TodoList`]. The same `Arc`s are held by the host `Agent` (in
/// `neenee-agent`), so a tool call is visible to the agent, the harness, and
/// the TUI immediately. Mirrors [`crate::plan::PlanToolContext`].
#[derive(Clone)]
pub struct TodoToolContext {
    todos: Arc<Mutex<TodoList>>,
    turn_counter: Arc<Mutex<u64>>,
}

impl TodoToolContext {
    pub fn new(todos: Arc<Mutex<TodoList>>, turn_counter: Arc<Mutex<u64>>) -> Self {
        Self {
            todos,
            turn_counter,
        }
    }

    /// Build a context that shares state with cells owned by the `Agent`.
    pub fn shared(todos: Arc<Mutex<TodoList>>, turn_counter: Arc<Mutex<u64>>) -> Self {
        Self {
            todos,
            turn_counter,
        }
    }

    pub fn todos(&self) -> TodoList {
        self.todos.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_todos(&self, list: TodoList) {
        let mut guard = self.todos.lock().unwrap_or_else(|e| e.into_inner());
        *guard = list;
    }

    pub fn current_turn(&self) -> u64 {
        self.turn_counter.lock().map(|g| *g).unwrap_or(0)
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tools
// ─────────────────────────────────────────────────────────────────────

const TODO_DESCRIPTION: &str =
    "Maintain the task list for the current work. Replace the whole list each call with the current \
     set of concrete steps, in the order you intend to tackle them. At most one item may be \
     in_progress. This list is the single source of truth shown in the sticky panel above the input \
     box and persisted across restarts, so keep it honest: add an item when you commit to a step, \
     move it to in_progress when you start, and to completed the moment it is done. The returned \
     list reflects the reconciled state (items keep their identity when you resend the same content).";

/// Full-replace todo tool. The model sends the desired list each call; the
/// tool reconciles it against the current list preserving identity (see
/// [`TodoList::reconcile`]). This is the robust interface: the model never
/// has to track ids.
pub struct TodoWriteTool {
    context: TodoToolContext,
}

impl TodoWriteTool {
    pub fn new(context: TodoToolContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        TODO_DESCRIPTION
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "maxItems": MAX_TODOS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"]
                            }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["items"],
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct Arguments {
            items: Vec<TodoArgs>,
        }
        #[derive(serde::Deserialize)]
        struct TodoArgs {
            content: String,
            status: String,
        }

        let parsed: Arguments =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {e}"))?;
        if parsed.items.len() > MAX_TODOS {
            return Err(format!("Todo list is limited to {MAX_TODOS} items."));
        }

        let mut desired: Vec<(String, TodoStatus)> = Vec::with_capacity(parsed.items.len());
        let mut in_progress = 0;
        for entry in parsed.items {
            if entry.content.trim().is_empty() {
                return Err("Todo item content cannot be empty.".to_string());
            }
            let status = TodoStatus::parse(&entry.status).ok_or_else(|| {
                format!(
                    "Unknown todo status '{}'. Use pending / in_progress / completed / cancelled.",
                    entry.status
                )
            })?;
            if status == TodoStatus::InProgress {
                in_progress += 1;
            }
            desired.push((entry.content, status));
        }
        if in_progress > 1 {
            return Err("At most one todo item may be in_progress.".to_string());
        }

        let now = unix_now();
        let turn = self.context.current_turn();
        let mut list = self.context.todos();
        list.reconcile(&desired, now, turn);
        self.context.set_todos(list);
        let rendered = self.context.todos().render();
        Ok(format!("Todo list updated:\n{rendered}"))
    }
}

const TODO_UPDATE_DESCRIPTION: &str =
    "Surgically update the status of one or more existing todo items without re-sending the whole \
     list. `key` is either a 1-based position as shown by the `todo` tool (\"1\", \"3\") or, when \
     not a valid position, a case-insensitive substring of the item content (all matches update). \
     Prefer this over `todo` when you only want to mark progress on a single step.";

/// Surgical update tool: change the status of items matched by position or
/// content substring, leaving everything else untouched. Complements
/// [`TodoWriteTool`] so the model can mark a step done without re-emitting the
/// entire list.
pub struct TodoUpdateTool {
    context: TodoToolContext,
}

impl TodoUpdateTool {
    pub fn new(context: TodoToolContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl Tool for TodoUpdateTool {
    fn name(&self) -> &str {
        "todo_update"
    }

    fn description(&self) -> &str {
        TODO_UPDATE_DESCRIPTION
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "1-based position or case-insensitive content substring"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed", "cancelled"],
                    "description": "New status for the matched item(s)"
                }
            },
            "required": ["key", "status"],
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let value = serde_json::from_str::<serde_json::Value>(arguments)
            .map_err(|e| format!("Invalid JSON: {e}"))?;
        let key = value
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or("Missing 'key'")?;
        let status_str = value
            .get("status")
            .and_then(|v| v.as_str())
            .ok_or("Missing 'status'")?;
        let status = TodoStatus::parse(status_str).ok_or_else(|| {
            format!(
                "Unknown status '{status_str}'. Use pending / in_progress / completed / cancelled."
            )
        })?;

        let now = unix_now();
        let turn = self.context.current_turn();
        let mut list = self.context.todos();
        if list.is_empty() {
            return Ok(
                "No todos to update. Use the `todo` tool to create the list first.".to_string(),
            );
        }
        let changed = list.update(key, status, now, turn);
        if changed == 0 {
            return Ok(format!(
                "No todo matched '{key}'. Current todos:\n{}",
                list.render()
            ));
        }
        self.context.set_todos(list);
        let rendered = self.context.todos().render();
        Ok(format!(
            "Updated {changed} item(s) to {status_str}.\n{rendered}",
            status_str = status_str
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(items: &[(&str, TodoStatus)]) -> Vec<(String, TodoStatus)> {
        items.iter().map(|(c, s)| (c.to_string(), *s)).collect()
    }

    #[test]
    fn reconcile_preserves_identity_for_unchanged_content() {
        let mut list = TodoList::new();
        list.reconcile(&desired(&[("design", TodoStatus::Pending)]), 100, 1);
        let first_id = list.items[0].id;

        // Same content, new status: id and created_at preserved, status/updated move.
        list.reconcile(&desired(&[("design", TodoStatus::Completed)]), 200, 2);
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].id, first_id);
        assert_eq!(list.items[0].created_at, 100);
        assert_eq!(list.items[0].status, TodoStatus::Completed);
        assert_eq!(list.items[0].updated_at, 200);
        assert_eq!(list.updated_at_turn, 2);
    }

    #[test]
    fn reconcile_assigns_fresh_ids_to_new_content() {
        let mut list = TodoList::new();
        list.reconcile(&desired(&[("a", TodoStatus::Pending)]), 100, 1);
        list.reconcile(
            &desired(&[("a", TodoStatus::Pending), ("b", TodoStatus::Pending)]),
            100,
            1,
        );
        assert_eq!(list.items.len(), 2);
        assert_ne!(list.items[0].id, list.items[1].id);
    }

    #[test]
    fn reconcile_drops_absent_items() {
        let mut list = TodoList::new();
        list.reconcile(
            &desired(&[("a", TodoStatus::Pending), ("b", TodoStatus::Pending)]),
            100,
            1,
        );
        list.reconcile(&desired(&[("a", TodoStatus::Completed)]), 200, 2);
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].content, "a");
    }

    #[test]
    fn reconcile_is_noop_when_unchanged() {
        let mut list = TodoList::new();
        list.reconcile(&desired(&[("a", TodoStatus::Pending)]), 100, 1);
        assert!(!list.reconcile(&desired(&[("a", TodoStatus::Pending)]), 100, 1));
    }

    #[test]
    fn update_by_position_changes_one_item() {
        let mut list = TodoList::new();
        list.reconcile(
            &desired(&[("a", TodoStatus::Pending), ("b", TodoStatus::Pending)]),
            100,
            1,
        );
        assert_eq!(list.update("1", TodoStatus::InProgress, 200, 3), 1);
        assert_eq!(list.items[0].status, TodoStatus::InProgress);
        assert_eq!(list.items[1].status, TodoStatus::Pending);
        assert_eq!(list.updated_at_turn, 3);
    }

    #[test]
    fn update_by_content_substring_matches_case_insensitive() {
        let mut list = TodoList::new();
        list.reconcile(
            &desired(&[
                ("Design UI", TodoStatus::Pending),
                ("design db", TodoStatus::Pending),
            ]),
            100,
            1,
        );
        // Substring "design" matches both.
        assert_eq!(list.update("design", TodoStatus::Completed, 200, 3), 2);
    }

    #[test]
    fn update_position_takes_priority_over_content() {
        let mut list = TodoList::new();
        list.reconcile(
            &desired(&[
                ("1 thing", TodoStatus::Pending),
                ("other", TodoStatus::Pending),
            ]),
            100,
            1,
        );
        // "1" is a valid position → only the first item.
        assert_eq!(list.update("1", TodoStatus::Completed, 200, 3), 1);
    }

    #[test]
    fn remove_by_position_and_content() {
        let mut list = TodoList::new();
        list.reconcile(
            &desired(&[("a", TodoStatus::Pending), ("ab", TodoStatus::Pending)]),
            100,
            1,
        );
        assert_eq!(list.remove("a", 2), 2);
        assert!(list.is_empty());
    }

    #[test]
    fn from_plan_markdown_seeds_pending_items_from_headings() {
        let list = TodoList::from_plan_markdown(
            "# Title\n\n## Summary\nbody\n\n## Key Changes\n- x\n",
            100,
            1,
        );
        let names: Vec<_> = list.items.iter().map(|i| i.content.as_str()).collect();
        assert_eq!(names, vec!["Summary", "Key Changes"]);
        assert!(list.items.iter().all(|i| i.status == TodoStatus::Pending));
        assert_eq!(list.updated_at_turn, 1);
    }

    #[test]
    fn from_plan_markdown_synthesizes_plan_item_without_headings() {
        let list = TodoList::from_plan_markdown("just prose", 100, 1);
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].content, "Plan");
    }

    #[test]
    fn status_parse_accepts_legacy_plan_spellings() {
        assert_eq!(TodoStatus::parse("done"), Some(TodoStatus::Completed));
        assert_eq!(TodoStatus::parse("skipped"), Some(TodoStatus::Cancelled));
        assert_eq!(
            TodoStatus::parse("in_progress"),
            Some(TodoStatus::InProgress)
        );
        assert_eq!(TodoStatus::parse("nope"), None);
    }

    #[test]
    fn render_uses_familiar_bracket_format() {
        let mut list = TodoList::new();
        list.reconcile(
            &desired(&[
                ("design", TodoStatus::Completed),
                ("implement", TodoStatus::InProgress),
                ("verify", TodoStatus::Pending),
                ("docs", TodoStatus::Cancelled),
            ]),
            100,
            1,
        );
        let out = list.render();
        assert!(out.contains("1. [x] design"));
        assert!(out.contains("2. [~] implement"));
        assert!(out.contains("3. [ ] verify"));
        assert!(out.contains("4. [-] docs"));
    }

    fn ctx() -> (TodoToolContext, Arc<Mutex<TodoList>>) {
        let list = Arc::new(Mutex::new(TodoList::new()));
        let turn = Arc::new(Mutex::new(5u64));
        (TodoToolContext::shared(Arc::clone(&list), turn), list)
    }

    #[tokio::test]
    async fn todo_write_tool_reconciles_and_preserves_identity() {
        let (context, list) = ctx();
        let tool = TodoWriteTool::new(context.clone());
        tool.call(r#"{"items":[{"content":"design","status":"pending"}]}"#)
            .await
            .unwrap();
        let first_id = list.lock().unwrap().items[0].id;

        tool.call(
            r#"{"items":[{"content":"design","status":"completed"},
                         {"content":"implement","status":"in_progress"}]}"#,
        )
        .await
        .unwrap();
        let guard = list.lock().unwrap();
        assert_eq!(guard.items[0].id, first_id); // identity preserved
        assert_eq!(guard.items[0].status, TodoStatus::Completed);
        assert_eq!(guard.items[1].content, "implement");
        assert_eq!(guard.updated_at_turn, 5);
    }

    #[tokio::test]
    async fn todo_write_tool_rejects_two_in_progress() {
        let (context, _) = ctx();
        let tool = TodoWriteTool::new(context);
        let err = tool
            .call(
                r#"{"items":[
                    {"content":"a","status":"in_progress"},
                    {"content":"b","status":"in_progress"}
                ]}"#,
            )
            .await
            .unwrap_err();
        assert!(err.contains("in_progress"));
    }

    #[tokio::test]
    async fn todo_update_tool_matches_by_position() {
        let (context, list) = ctx();
        let write = TodoWriteTool::new(context.clone());
        write
            .call(
                r#"{"items":[
                {"content":"a","status":"pending"},
                {"content":"b","status":"pending"}
            ]}"#,
            )
            .await
            .unwrap();
        let update = TodoUpdateTool::new(context);
        update
            .call(r#"{"key":"2","status":"completed"}"#)
            .await
            .unwrap();
        let guard = list.lock().unwrap();
        assert_eq!(guard.items[1].status, TodoStatus::Completed);
        assert_eq!(guard.items[0].status, TodoStatus::Pending);
    }

    #[tokio::test]
    async fn todo_update_tool_matches_by_content() {
        let (context, list) = ctx();
        let write = TodoWriteTool::new(context.clone());
        write
            .call(r#"{"items":[{"content":"Write tests","status":"pending"}]}"#)
            .await
            .unwrap();
        let update = TodoUpdateTool::new(context);
        let body = update
            .call(r#"{"key":"tests","status":"completed"}"#)
            .await
            .unwrap();
        assert!(body.contains("Updated 1"));
        assert_eq!(list.lock().unwrap().items[0].status, TodoStatus::Completed);
    }

    #[tokio::test]
    async fn todo_update_tool_empty_list_returns_hint() {
        let (context, _) = ctx();
        let tool = TodoUpdateTool::new(context);
        let body = tool.call(r#"{"key":"1","status":"done"}"#).await.unwrap();
        assert!(body.contains("No todos"));
    }
}

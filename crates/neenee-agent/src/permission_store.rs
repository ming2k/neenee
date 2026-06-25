//! Permission allowlist + pending-request registry, extracted from the
//! `Agent` god-object.
//!
//! Owns the "always allow" rule set (optionally persisted to disk per
//! project), the map of pending permission requests awaiting a user reply,
//! and the project root used for persistence. The [`crate::Agent`] owns a
//! single `PermissionStore` and delegates its permission-related public
//! methods here.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use neenee_core::PermissionDecision;
use tokio::sync::oneshot;

/// Internal lock-guard helper: poison-immune (recovers via `into_inner`).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PermissionRule {
    pub tool: String,
    pub scope: String,
}

/// On-disk shape of the persisted "always allow" allowlist, versioned for
/// future schema evolution. Readers reject unknown future versions rather
/// than guessing, so a downgrade silently ignores the file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedPermissions {
    version: u32,
    rules: Vec<PermissionRule>,
}

impl PersistedPermissions {
    const CURRENT_VERSION: u32 = 1;
}

#[derive(Default)]
struct PermissionState {
    always: HashSet<PermissionRule>,
    pending: HashMap<String, oneshot::Sender<PermissionDecision>>,
}

/// In-memory permission state: the "always allow" allowlist, the pending
/// request channels, and the optional project root for on-disk persistence.
pub struct PermissionStore {
    state: Mutex<PermissionState>,
    project_root: Mutex<Option<std::path::PathBuf>>,
    /// When true, write tools execute without a permission prompt. Bypasses
    /// the allowlist entirely (the prompt block is skipped wholesale).
    auto_approve: Mutex<bool>,
}

impl PermissionStore {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(PermissionState::default()),
            project_root: Mutex::new(None),
            auto_approve: Mutex::new(false),
        }
    }

    // ── auto-approve ────────────────────────────────────────────────────

    pub fn auto_approve(&self) -> bool {
        *lock(&self.auto_approve)
    }

    pub fn set_auto_approve(&self, value: bool) {
        *lock(&self.auto_approve) = value;
    }

    // ── pending requests ────────────────────────────────────────────────

    /// Register a pending permission request and return the receiver the
    /// caller should `await` for the user's decision.
    pub fn park_request(&self, request_id: String) -> oneshot::Receiver<PermissionDecision> {
        let (sender, receiver) = oneshot::channel();
        lock(&self.state).pending.insert(request_id, sender);
        receiver
    }

    /// Resolve a pending permission request. Rejecting one aborts the turn,
    /// so every other pending request in the same batch is also resolved with
    /// `Reject` to avoid deadlocking the `join_all`. Returns whether a sender
    /// was found.
    pub fn reply(&self, request_id: &str, decision: PermissionDecision) -> bool {
        let mut perms = lock(&self.state);
        let sender = perms.pending.remove(request_id);
        let sent = sender.is_some_and(|sender| sender.send(decision).is_ok());
        if sent && decision == PermissionDecision::Reject {
            for (_, pending_sender) in perms.pending.drain() {
                let _ = pending_sender.send(PermissionDecision::Reject);
            }
        }
        sent
    }

    /// Reject every pending permission request (e.g. on turn abort).
    pub fn reject_pending(&self) {
        let pending = std::mem::take(&mut lock(&self.state).pending);
        for (_, sender) in pending {
            let _ = sender.send(PermissionDecision::Reject);
        }
    }

    // ── allowlist ───────────────────────────────────────────────────────

    /// Check whether a rule is in the "always allow" set.
    pub fn is_always_allowed(&self, rule: &PermissionRule) -> bool {
        lock(&self.state).always.contains(rule)
    }

    /// Add a rule to the "always allow" set and persist.
    pub fn add_always(&self, rule: PermissionRule) {
        lock(&self.state).always.insert(rule);
        self.persist();
    }

    pub fn allowed_tools(&self) -> Vec<String> {
        let mut tools = lock(&self.state)
            .always
            .iter()
            .map(|rule| format!("{} {}", rule.tool, rule.scope))
            .collect::<Vec<_>>();
        tools.sort();
        tools
    }

    pub fn allowed_tools_structured(&self) -> Vec<neenee_core::PermissionRuleInfo> {
        let mut rules: Vec<neenee_core::PermissionRuleInfo> = lock(&self.state)
            .always
            .iter()
            .map(|rule| neenee_core::PermissionRuleInfo {
                tool: rule.tool.clone(),
                scope: rule.scope.clone(),
            })
            .collect();
        rules.sort_by(|a, b| a.tool.cmp(&b.tool).then_with(|| a.scope.cmp(&b.scope)));
        rules
    }

    pub fn clear_allowed(&self) {
        lock(&self.state).always.clear();
        self.persist();
    }

    pub fn revoke_allowed(&self, tool: &str, scope: &str) -> bool {
        let rule = PermissionRule {
            tool: tool.to_string(),
            scope: scope.to_string(),
        };
        let removed = lock(&self.state).always.remove(&rule);
        if removed {
            self.persist();
        }
        removed
    }

    // ── persistence ─────────────────────────────────────────────────────

    /// The persisted project root, if any.
    pub fn project_root(&self) -> Option<std::path::PathBuf> {
        lock(&self.project_root).clone()
    }

    /// Designate the project whose bucket backs the persistent "always"
    /// allowlist, and load any rules already on disk into the in-memory set.
    /// Pass `None` to disable persistence (sub-agents and most tests do this).
    pub fn set_project_root(&self, root: Option<std::path::PathBuf>) {
        {
            *lock(&self.project_root) = root.clone();
        }
        if let Some(root) = root {
            self.load_persistent(&root);
        }
    }

    fn load_persistent(&self, root: &std::path::Path) {
        let path = neenee_store::paths::get().project_permissions(root);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        match serde_json::from_str::<PersistedPermissions>(&text) {
            Ok(persisted) if persisted.version == PersistedPermissions::CURRENT_VERSION => {
                let mut perms = lock(&self.state);
                let count = persisted.rules.len();
                for rule in persisted.rules {
                    perms.always.insert(rule);
                }
                tracing::info!(count, path = %path.display(), "loaded persistent permission rules");
            }
            Ok(other) => {
                tracing::warn!(
                    version = other.version,
                    path = %path.display(),
                    "unsupported persisted permissions version; ignoring file",
                );
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "could not parse persistent permissions file; ignoring",
                );
            }
        }
    }

    /// Atomically mirror the current `always` allowlist into the project
    /// bucket. Best-effort: logs on failure and never propagates the error.
    fn persist(&self) {
        let root = lock(&self.project_root).clone();
        let Some(root) = root else {
            return;
        };
        let path = neenee_store::paths::get().project_permissions(&root);
        let snapshot = {
            let perms = lock(&self.state);
            let mut rules: Vec<PermissionRule> = perms.always.iter().cloned().collect();
            rules.sort_by(|a, b| a.tool.cmp(&b.tool).then_with(|| a.scope.cmp(&b.scope)));
            PersistedPermissions {
                version: PersistedPermissions::CURRENT_VERSION,
                rules,
            }
        };
        if let Err(e) = neenee_store::fsutil::atomic_write_json(&path, &snapshot) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not persist permission rules",
            );
        }
    }
}

impl Default for PermissionStore {
    fn default() -> Self {
        Self::new()
    }
}

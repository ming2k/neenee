//! Sub-agent profiles: declarative tool-permission roles for autonomous
//! sub-agents spawned by the `task` tool (and wrappers like
//! `verify_plan_execution`).
//!
//! ## Why this exists
//!
//! Before ADR-0011 the sub-agent's toolset was a hardcoded filter inside
//! `TaskTool` (`access() == Read && name != "task"`). That had two problems:
//!
//! 1. **It was name-driven, not semantic.** `ask_user` is `Read`, so it
//!    passed the filter and reached the sub-agent. But a sub-agent is
//!    autonomous and non-interactive — its `UserQuestionRequest` events are
//!    dropped by the task tool's event forwarder, so the request deadlocks
//!    until the parent turn is cancelled. The user could see the call but
//!    could not answer it.
//! 2. **The policy was buried in orchestration code.** Adding a second
//!    sub-agent role (or tightening the existing one) meant editing the
//!    dispatch tool rather than declaring intent.
//!
//! The fix is a profile primitive that expresses the tool policy in terms of
//! [`Tool`] capability axes — [`Tool::access`], [`Tool::requires_user`],
//! [`Tool::spawns_subagent`] — so admission is data-driven and generalizes to
//! future tools without touching the dispatch path.
//!
//! ## The capability axes
//!
//! - [`Tool::access`] — filesystem mutation (`Read` vs `Write`). Existing.
//! - [`Tool::requires_user`] — may block on a live human (e.g. `ask_user`).
//! - [`Tool::spawns_subagent`] — dispatches a nested agent (e.g. `task`).
//!
//! Recursion is unconditionally forbidden in any sub-agent: a tool that
//! `spawns_subagent` is never admitted, regardless of profile. User
//! interaction is a per-profile knob ([`ToolPolicy::allow_user_interaction`])
//! so a future interactive role could opt in once the plumbing surfaces the
//! request; the built-in [`EXPLORE`] profile leaves it off.

use std::sync::Arc;

use crate::{Tool, ToolAccess};

/// Ceiling on what a sub-agent may do, expressed as capability-axis rules.
///
/// `access` is a *ceiling*, not a set: a `Read` policy admits only `Read`
/// tools; a `Write` policy admits both. [`Tool::spawns_subagent`] tools are
/// always excluded (recursion is absolute, not a per-profile toggle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPolicy {
    /// Maximum filesystem access a sub-agent under this policy may have.
    pub access: ToolAccess,
    /// Whether tools that block on a human ([`Tool::requires_user`]) may run.
    pub allow_user_interaction: bool,
}

impl ToolPolicy {
    /// Returns `true` if a tool may be handed to a sub-agent under this policy.
    pub fn admits(&self, tool: &dyn Tool) -> bool {
        // Recursion is unconditionally forbidden in sub-agents.
        if tool.spawns_subagent() {
            return false;
        }
        // Respect the filesystem-access ceiling (`access` is ordered
        // `Read < Execute < Write`, so this admits the tier and everything
        // below it).
        if tool.access() > self.access {
            return false;
        }
        // Tools that block on a human are gated by the profile.
        if tool.requires_user() && !self.allow_user_interaction {
            return false;
        }
        true
    }
}

/// A declarative sub-agent role: a name, the system-prompt fragment that
/// frames the role, and the [`ToolPolicy`] that scopes what it may touch.
///
/// Profiles live in `neenee-core` (domain vocabulary) so dispatch tools in
/// `neenee-agent` resolve them without re-implementing admission logic. The
/// built-in [`EXPLORE`] profile is what `task` binds to today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentProfile {
    pub name: &'static str,
    pub system_prompt: &'static str,
    pub tool_policy: ToolPolicy,
}

impl SubagentProfile {
    /// Filter a parent toolset down to what this profile admits.
    pub fn select_tools(&self, tools: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
        tools
            .iter()
            .filter(|tool| self.tool_policy.admits(tool.as_ref()))
            .cloned()
            .collect()
    }
}

/// The built-in read-only research role used by `task`.
///
/// Read-only, non-interactive, non-recursive. This is the profile the `task`
/// tool binds to; declaring additional profiles (and exposing a role selector
/// on `task`) is a future extension that needs no changes here.
pub const EXPLORE: SubagentProfile = SubagentProfile {
    name: "explore",
    system_prompt: "\
You are a focused research sub-agent. Your single job is to answer the assigned \
task accurately and concisely using read-only tools. Explore the workspace or \
the web as needed, then write a clear, complete final answer with the key \
findings (file paths, signatures, relevant snippets, conclusions). \
Do not modify any files. You are non-interactive: never ask the user any \
question — if information is missing, make a reasonable assumption, note it \
explicitly in your answer, or report that you could not find it. Run at most a \
handful of tool rounds, then answer.",
    tool_policy: ToolPolicy {
        access: ToolAccess::Read,
        allow_user_interaction: false,
    },
};

/// The built-in independent-verifier role used by `verify_plan_execution`.
///
/// Distinct from [`EXPLORE`] in one dimension: its access ceiling is
/// `Execute`, so it additionally admits command-execution tools (`bash`) for
/// running tests, builds, and type-checks as concrete verification evidence.
/// It still excludes `Write` tools (a verifier must not edit the
/// implementation it is auditing), `requires_user` tools (no user reachable),
/// and `spawns_subagent` tools (no recursion). See ADR-0012.
pub const VERIFY: SubagentProfile = SubagentProfile {
    name: "verify",
    system_prompt: "\
You are an independent verification sub-agent. Your job is to audit whether an \
implementation actually matches a plan, with no bias from whoever wrote it. \
Read the plan and the current workspace state; run read-only inspection and \
command-line checks (tests, builds, type-checks) as needed to gather concrete \
evidence. You may run commands but must not modify any files. You are \
non-interactive: never ask the user any question — if you cannot determine \
something, report it as inconclusive. Report only what you directly observed, \
never the implementer's claims.",
    tool_policy: ToolPolicy {
        access: ToolAccess::Execute,
        allow_user_interaction: false,
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// A configurable test tool used to exercise every admission branch.
    struct Stub {
        name: &'static str,
        access: ToolAccess,
        requires_user: bool,
        spawns_subagent: bool,
    }

    #[async_trait]
    impl Tool for Stub {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn access(&self) -> ToolAccess {
            self.access
        }
        fn requires_user(&self) -> bool {
            self.requires_user
        }
        fn spawns_subagent(&self) -> bool {
            self.spawns_subagent
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("stub".to_string())
        }
    }

    fn make(access: ToolAccess, requires_user: bool, spawns_subagent: bool) -> Stub {
        Stub {
            name: "stub",
            access,
            requires_user,
            spawns_subagent,
        }
    }

    #[test]
    fn explore_policy_admits_plain_read_tool() {
        assert!(EXPLORE
            .tool_policy
            .admits(&make(ToolAccess::Read, false, false)));
    }

    #[test]
    fn explore_policy_rejects_write_tool() {
        assert!(!EXPLORE
            .tool_policy
            .admits(&make(ToolAccess::Write, false, false)));
    }

    #[test]
    fn explore_policy_rejects_execute_tool() {
        // bash shape: Execute. EXPLORE's Read ceiling excludes it — a research
        // explorer must not run commands.
        assert!(!EXPLORE
            .tool_policy
            .admits(&make(ToolAccess::Execute, false, false)));
    }

    #[test]
    fn verify_policy_admits_read_and_execute_but_not_write() {
        // VERIFY is the verifier shape: read-only inspection + command
        // execution for tests/builds, but no file-write.
        assert!(VERIFY
            .tool_policy
            .admits(&make(ToolAccess::Read, false, false)));
        assert!(VERIFY
            .tool_policy
            .admits(&make(ToolAccess::Execute, false, false)));
        assert!(!VERIFY
            .tool_policy
            .admits(&make(ToolAccess::Write, false, false)));
    }

    #[test]
    fn verify_policy_still_rejects_user_and_recursion() {
        assert!(!VERIFY
            .tool_policy
            .admits(&make(ToolAccess::Read, true, false)));
        assert!(!VERIFY
            .tool_policy
            .admits(&make(ToolAccess::Execute, false, true)));
    }

    #[test]
    fn explore_policy_rejects_user_interaction_tool() {
        // ask_user shape: Read + requires_user.
        assert!(!EXPLORE
            .tool_policy
            .admits(&make(ToolAccess::Read, true, false)));
    }

    #[test]
    fn explore_policy_rejects_dispatch_tool() {
        // task shape: Read + spawns_subagent. Even though it is Read, the
        // recursion guard excludes it — this is the case the old name-based
        // `name != "task"` filter hardcoded.
        assert!(!EXPLORE
            .tool_policy
            .admits(&make(ToolAccess::Read, false, true)));
    }

    #[test]
    fn recursion_is_rejected_even_by_a_permissive_write_policy() {
        let permissive = ToolPolicy {
            access: ToolAccess::Write,
            allow_user_interaction: true,
        };
        // A Write+interactive policy still never admits recursion.
        assert!(!permissive.admits(&make(ToolAccess::Read, false, true)));
        assert!(permissive.admits(&make(ToolAccess::Write, true, false)));
    }

    #[test]
    fn select_tools_filters_a_mixed_set() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(make(ToolAccess::Read, false, false)), // admit
            Arc::new(make(ToolAccess::Execute, false, false)), // reject (execute > Read)
            Arc::new(make(ToolAccess::Write, false, false)), // reject (write)
            Arc::new(make(ToolAccess::Read, true, false)),  // reject (user)
            Arc::new(make(ToolAccess::Read, false, true)),  // reject (recurse)
        ];
        let selected = EXPLORE.select_tools(&tools);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name(), "stub");
    }

    #[test]
    fn verify_select_tools_admits_read_and_execute_only() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(Stub {
                name: "read_file",
                access: ToolAccess::Read,
                requires_user: false,
                spawns_subagent: false,
            }),
            Arc::new(Stub {
                name: "bash",
                access: ToolAccess::Execute,
                requires_user: false,
                spawns_subagent: false,
            }),
            Arc::new(Stub {
                name: "write_file",
                access: ToolAccess::Write,
                requires_user: false,
                spawns_subagent: false,
            }),
            Arc::new(Stub {
                name: "ask_user",
                access: ToolAccess::Read,
                requires_user: true,
                spawns_subagent: false,
            }),
            Arc::new(Stub {
                name: "task",
                access: ToolAccess::Read,
                requires_user: false,
                spawns_subagent: true,
            }),
        ];
        let selected = VERIFY.select_tools(&tools);
        let names: Vec<&str> = selected.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["read_file", "bash"]);
    }
}

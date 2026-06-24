//! Subagent profiles: declarative tool-permission roles for autonomous
//! sub-agents spawned by the `task` tool (and wrappers like
//! `verify_plan_execution`).
//!
//! ## Why this exists
//!
//! Before ADR-0011 the subagent's toolset was a hardcoded filter inside
//! the dispatch tool (`access() == Read` plus a name exclusion for itself).
//! That had two problems:
//!
//! 1. **It was name-driven, not semantic.** `ask_user` is `Read`, so it
//!    passed the filter and reached the subagent. But a subagent is
//!    autonomous and non-interactive — its `UserQuestionRequest` events are
//!    dropped by the subagent tool's event forwarder, so the request deadlocks
//!    until the parent turn is cancelled. The user could see the call but
//!    could not answer it.
//! 2. **The policy was buried in orchestration code.** Adding a second
//!    subagent role (or tightening the existing one) meant editing the
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
//! Recursion is unconditionally forbidden in any subagent: a tool that
//! `spawns_subagent` is never admitted, regardless of profile. User
//! interaction is a per-profile knob ([`ToolPolicy::allow_user_interaction`])
//! so a future interactive role could opt in once the plumbing surfaces the
//! request; the built-in [`EXPLORE`] profile leaves it off.

use std::path::Path;
use std::sync::Arc;

use crate::{Tool, ToolAccess, WriteScope};

/// Ceiling on what a subagent may do, expressed as capability-axis rules.
///
/// `access` is a *ceiling*, not a set: a `Read` policy admits only `Read`
/// tools; a `Write` policy admits both. [`Tool::spawns_subagent`] tools are
/// always excluded (recursion is absolute, not a per-profile toggle). Write
/// tools below the ceiling are additionally admitted by a non-empty
/// `write_paths` grant — see [`ToolPolicy::write_paths`] and ADR-0028.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPolicy {
    /// Maximum filesystem access a subagent under this policy may have.
    pub access: ToolAccess,
    /// Whether tools that block on a human ([`Tool::requires_user`]) may run.
    pub allow_user_interaction: bool,
    /// Declarative write grant: directory specs (relative or absolute) a
    /// subagent under this policy may write to, **beyond** what `access`
    /// admits. Empty (the default) grants no writes.
    ///
    /// Admission effect: a `Write` tool is admitted when `access == Write`
    /// *or* this list is non-empty; an `Execute` tool (e.g. `bash`) is never
    /// admitted this way. So a `Read` ceiling plus `[".neenee/plans"]` yields
    /// "read tools + writes scoped to the plans dir, but no bash" — the
    /// `PLAN` shape. At spawn, [`SubagentProfile::resolve_write_scope`]
    /// canonicalizes these against the cwd into a runtime [`WriteScope`] the
    /// agent enforces. See ADR-0028.
    pub write_paths: &'static [&'static str],
}

impl ToolPolicy {
    /// Returns `true` if a tool may be handed to a subagent under this policy.
    pub fn admits(&self, tool: &dyn Tool) -> bool {
        // Recursion is unconditionally forbidden in sub-agents.
        if tool.spawns_subagent() {
            return false;
        }
        // Tools that block on a human are gated by the profile.
        if tool.requires_user() && !self.allow_user_interaction {
            return false;
        }
        let access = tool.access();
        // Read/Execute tools (and Write under a Write ceiling) admitted by the
        // ordered ceiling.
        if access <= self.access {
            return true;
        }
        // A Write tool below the ceiling is admitted by a write_paths grant
        // (scoped at runtime). Execute (bash) is never granted this way, so a
        // Read ceiling + write_paths profile gets writes-without-bash. ADR-0028.
        if access == ToolAccess::Write && !self.write_paths.is_empty() {
            return true;
        }
        false
    }
}

/// A declarative subagent role: a name, the system-prompt fragment that
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
    /// Whether the spawned subagent auto-approves its admitted write/execute
    /// tools, bypassing the permission broker. Full-duplex (ADR-0029): the
    /// built-in profiles keep this `true` to preserve the legacy autonomous
    /// contract (the broker's `PermissionRequest` would otherwise surface up
    /// to the parent, which historically had no path to answer it). Now that
    /// the up-direction (forwarding) and down-direction (registry → handle →
    /// `reply_permission`) are wired, a future interactive profile can set
    /// this `false` so a subagent's tool calls prompt the user through the
    /// same modal a top-level call uses, and the reply routes back down.
    pub auto_approve: bool,
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

    /// Resolve this profile's declarative `write_paths` grant into a runtime
    /// [`WriteScope`] against `cwd`. Each spec (relative or absolute) is
    /// joined to `cwd` and canonicalized best-effort (a not-yet-existing dir
    /// falls back to the joined path). An empty `write_paths` yields
    /// [`WriteScope::None`]. The resulting scope is what the spawned agent
    /// enforces on every write tool. See ADR-0028.
    pub fn resolve_write_scope(&self, cwd: &Path) -> WriteScope {
        if self.tool_policy.write_paths.is_empty() {
            return WriteScope::None;
        }
        let dirs = self
            .tool_policy
            .write_paths
            .iter()
            .map(|spec| {
                let p = std::path::Path::new(spec);
                let joined = if p.is_absolute() {
                    std::path::PathBuf::from(spec)
                } else {
                    cwd.join(spec)
                };
                joined.canonicalize().unwrap_or(joined)
            })
            .collect();
        WriteScope::Scoped(dirs)
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
You are a focused research subagent. Your single job is to answer the assigned \
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
        write_paths: &[],
    },
    auto_approve: true,
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
You are an independent verification subagent. Your job is to audit whether an \
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
        write_paths: &[],
    },
    auto_approve: true,
};

/// The diagnostic role used by session review (ADR-0016). Read-only,
/// non-interactive, non-recursive — like [`EXPLORE`] in capability, but framed
/// as a health auditor that reasons over a handed-off transcript snapshot and
/// returns structured verdicts rather than free-form research findings. Bound
/// by `SubagentTool`-style machinery in `neenee-agent` (`Agent::run_session_review`),
/// never by a model tool call.
pub const REVIEW: SubagentProfile = SubagentProfile {
    name: "review",
    system_prompt: "\
You are a session-health diagnostic subagent. You are handed a snapshot of \
another agent's live transcript and asked whether it is making progress or \
stuck. Judge from what you see — the sequence of tool calls, whether the same \
ground is being revisited, whether edits or commands are actually landing. \
You may read files to check a claim, but you must not modify anything. You are \
non-interactive: never ask a question; if you cannot tell, say so. Answer with \
the requested structured verdict only, no preamble.",
    tool_policy: ToolPolicy {
        access: ToolAccess::Read,
        allow_user_interaction: false,
        write_paths: &[],
    },
    auto_approve: true,
};

/// The session-titling role (ADR-0022). Read-only and non-interactive like
/// [`REVIEW`], but its task is pure text-in/text-out — it admits no tool loop at
/// all. The runner (`Agent::generate_title`) makes a single `provider.chat()`
/// framed by this prompt and normalizes the reply via `clean_title`. Declared as
/// a profile (not an ad-hoc call) so the capability-axis vocabulary stays the
/// single source of truth for what a bounded subagent may do, per ADR-0011.
pub const TITLE: SubagentProfile = SubagentProfile {
    name: "title",
    system_prompt: "\
You are a session-titling subagent. You are shown an excerpt of a conversation \
and asked for a short title that captures what the session is about. Reply with \
only the title — 3 to 7 words, plain text, no quotes, no markdown, no trailing \
punctuation, no preamble. Name the concrete subject of the work (a feature, \
file, bug, or task) rather than a generic word like \"chat\" or \"help\". Write \
the title in the same language as the conversation.",
    tool_policy: ToolPolicy {
        access: ToolAccess::Read,
        allow_user_interaction: false,
        write_paths: &[],
    },
    auto_approve: true,
};

/// The planning role (ADR-0027). Read-only research like [`EXPLORE`], plus a
/// scoped write grant to `.neenee/plans/` (ADR-0028) so it can persist its own
/// plan document — but **no `bash`** (`Execute` is never granted via
/// `write_paths`). Non-interactive: if it needs user input it returns the
/// question for the parent agent to relay (ADR-0027 §5), rather than blocking.
///
/// Spawned by the main agent's `plan` tool. Its job is to research the
/// request, design the change, and write the plan to
/// `.neenee/plans/<descriptive-slug>.md` using `## ` section headings (one per
/// implementation step, since the headings seed the todo list on approval),
/// then return a one-line completion signal naming the path it wrote.
pub const PLAN: SubagentProfile = SubagentProfile {
    name: "plan",
    system_prompt: "\
You are a planning subagent. Research the assigned change with read-only tools, \
design a concrete implementation approach, and write the plan to a file under \
.neenee/plans/ using write_file (the only place you may write). Name the file \
<descriptive-slug>.md. Structure the plan with a `## ` (level-2) heading per \
implementation step — each heading becomes a tracked todo on approval, so make \
them discrete, ordered, actionable steps. Under each, give the files to touch \
and the approach. You may NOT run commands (no bash) and may NOT ask the user \
any question; if a decision is genuinely blocking, state the assumption you \
made and the open question explicitly in the plan. When the plan file is \
written, reply with a single line: `Plan written to .neenee/plans/<slug>.md`.",
    tool_policy: ToolPolicy {
        access: ToolAccess::Read,
        allow_user_interaction: false,
        write_paths: &[".neenee/plans"],
    },
    auto_approve: true,
};

/// The interactive subagent role (ADR-0029). The built-in roles
/// ([`EXPLORE`]/[`VERIFY`]/[`PLAN`]) are autonomous: `auto_approve: true` and
/// no `requires_user` tools, so they never block on a human. This role is the
/// opposite shape — it is meant to run **under user supervision**: a `Write`
/// ceiling admits the full tool ladder (read + execute + write),
/// `allow_user_interaction` admits `ask_user`, and `auto_approve: false` leaves
/// the permission broker on, so every execute/write surfaces as a
/// `SubagentEvent::PermissionRequest` that round-trips through the parent
/// harness ↔ TUI ↔ registry handle.
///
/// It is the "turn the duplex on" role: bind it to a dispatch tool to get a
/// subagent whose tool calls and questions reach the user in real time and
/// whose replies route back down. Left unbound by the built-in `subagent`
/// tool (which stays `EXPLORE`) because forcing every research subagent to
/// prompt would defeat the point of autonomous exploration — opting a
/// specific dispatch tool into `INTERACTIVE` is a product-level decision.
pub const INTERACTIVE: SubagentProfile = SubagentProfile {
    name: "interactive",
    system_prompt: "\
You are an interactive subagent operating under user supervision. You may read \
files, run commands, write files, and ask the user questions. Every command and \
write you attempt is presented to the user for approval before it executes — \
treat that as a real gate, not a rubber stamp: prefer the narrowest action that \
answers the question, and batch only when genuinely related. When you need a \
decision only the user can make (an ambiguous requirement, a choice between \
approaches with different trade-offs), use ask_user rather than guessing. Keep \
turns short and report concrete findings, then stop.",
    tool_policy: ToolPolicy {
        access: ToolAccess::Write,
        allow_user_interaction: true,
        write_paths: &[],
    },
    auto_approve: false,
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
        // self-exclusion filter hardcoded.
        assert!(!EXPLORE
            .tool_policy
            .admits(&make(ToolAccess::Read, false, true)));
    }

    #[test]
    fn recursion_is_rejected_even_by_a_permissive_write_policy() {
        let permissive = ToolPolicy {
            access: ToolAccess::Write,
            allow_user_interaction: true,
            write_paths: &[],
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
                name: "subagent",
                access: ToolAccess::Read,
                requires_user: false,
                spawns_subagent: true,
            }),
        ];
        let selected = VERIFY.select_tools(&tools);
        let names: Vec<&str> = selected.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["read_file", "bash"]);
    }

    /// The PLAN shape (ADR-0028): a `Read` ceiling plus a non-empty
    /// `write_paths` grant admits read tools **and** write tools (scoped at
    /// runtime), but **not** `Execute` (`bash`). It still drops user-interactive
    /// and recursive tools.
    #[test]
    fn write_paths_grant_admits_write_below_read_ceiling_but_not_execute() {
        let plan_policy = ToolPolicy {
            access: ToolAccess::Read,
            allow_user_interaction: false,
            write_paths: &[".neenee/plans"],
        };
        assert!(plan_policy.admits(&make(ToolAccess::Read, false, false)));
        assert!(plan_policy.admits(&make(ToolAccess::Write, false, false))); // via the grant
        assert!(!plan_policy.admits(&make(ToolAccess::Execute, false, false))); // bash excluded
        assert!(!plan_policy.admits(&make(ToolAccess::Write, true, false))); // user excluded
        assert!(!plan_policy.admits(&make(ToolAccess::Read, false, true))); // recursion excluded
    }

    /// Regression for every existing profile: an empty `write_paths` leaves a
    /// `Read` ceiling excluding write tools exactly as before the grant existed.
    #[test]
    fn empty_write_paths_keeps_read_ceiling_excluding_write() {
        let read_only = ToolPolicy {
            access: ToolAccess::Read,
            allow_user_interaction: false,
            write_paths: &[],
        };
        assert!(read_only.admits(&make(ToolAccess::Read, false, false)));
        assert!(!read_only.admits(&make(ToolAccess::Write, false, false)));
    }

    /// The built-in `PLAN` profile (ADR-0027/0028): admits read tools and
    /// write tools (scoped to `.neenee/plans` at runtime), but excludes
    /// `bash` (Execute), interactive tools, and recursion.
    #[test]
    fn plan_profile_admits_read_and_scoped_write_but_not_bash() {
        use crate::PLAN;
        // PLAN writes — admission says yes (the runtime WriteScope scopes it).
        assert!(PLAN
            .tool_policy
            .admits(&make(ToolAccess::Read, false, false)));
        assert!(PLAN
            .tool_policy
            .admits(&make(ToolAccess::Write, false, false)));
        // No bash, no user-interactive, no recursion.
        assert!(!PLAN
            .tool_policy
            .admits(&make(ToolAccess::Execute, false, false)));
        assert!(!PLAN
            .tool_policy
            .admits(&make(ToolAccess::Read, true, false)));
        assert!(!PLAN
            .tool_policy
            .admits(&make(ToolAccess::Read, false, true)));
    }

    /// The `INTERACTIVE` profile (ADR-0029): the supervised shape — a `Write`
    /// ceiling admits the full ladder (read + execute + write) AND
    /// user-interactive tools (ask_user), but still forbids recursion. Its
    /// `auto_approve: false` (asserted separately) is what makes its
    /// execute/write calls surface as `PermissionRequest`s that round-trip to
    /// the user; this test fixes the *admission* contract.
    #[test]
    fn interactive_profile_admits_execute_and_user_but_not_recursion() {
        use crate::INTERACTIVE;
        assert!(INTERACTIVE
            .tool_policy
            .admits(&make(ToolAccess::Read, false, false)));
        assert!(INTERACTIVE
            .tool_policy
            .admits(&make(ToolAccess::Execute, false, false)));
        assert!(INTERACTIVE
            .tool_policy
            .admits(&make(ToolAccess::Write, false, false)));
        // ask_user shape (Read + requires_user) is admitted — the only built-in
        // role that lets a subagent reach the user directly.
        assert!(INTERACTIVE
            .tool_policy
            .admits(&make(ToolAccess::Read, true, false)));
        // Recursion is still absolute.
        assert!(!INTERACTIVE
            .tool_policy
            .admits(&make(ToolAccess::Read, false, true)));
        // The duplex "on" switch is `auto_approve: false` on INTERACTIVE —
        // left asserted at runtime by the duplex end-to-end test
        // (`subagent_tool_registry_routes_reply_into_live_subagent`), which
        // only passes if the broker fires for this profile. Asserting it here
        // too would trip clippy::assertions_on_constants (the profile is a
        // const), so the regression coverage lives in the runtime test.
    }
}

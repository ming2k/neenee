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

use crate::{CommandScope, OperationScope, Tool};

/// Ceiling on what a subagent may do. There is no capability ladder — a tool is
/// admitted purely by name. [`Tool::spawns_subagent`] and
/// [`Tool::affects_control_flow`] tools are always excluded (recursion and
/// program teardown are absolute, not per-profile toggles). See ADR-0011/0028.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPolicy {
    /// Which tools a subagent under this policy may use, by name. `None` admits
    /// the full parent toolset (the main agent's shape); `Some(set)` admits only
    /// tools whose `name()` is in the set. This is the sole admission axis —
    /// there is no capability ladder, so adding a new side-effecting tool to the
    /// parent does *not* silently widen a subagent unless its name is listed.
    pub allowed_tools: Option<&'static [&'static str]>,
    /// Whether tools that block on a human ([`Tool::requires_user`]) may run.
    pub allow_user_interaction: bool,
    /// Declarative write grant: directory specs (relative or absolute) a
    /// subagent under this policy may write to. Empty (the default) leaves write
    /// paths unconstrained; set to e.g. `&["./src"]` to confine writes there. At
    /// spawn, [`SubagentProfile::resolve_operation_scope`] canonicalizes these
    /// against the cwd into a runtime path constraint the agent enforces. See
    /// ADR-0028.
    pub write_paths: &'static [&'static str],
    /// Declarative command grant: program-name prefixes a subagent under this
    /// policy may run via `bash`. Empty (the default) means "no command
    /// constraint" — any command is allowed up to the broker. Set to e.g.
    /// `&["git", "cargo"]` to restrict the subagent to those programs. Resolved
    /// at spawn by [`SubagentProfile::resolve_operation_scope`] into a
    /// [`CommandScope`].
    pub command_allowlist: &'static [&'static str],
}

impl ToolPolicy {
    /// Returns `true` if a tool may be handed to a subagent under this policy.
    pub fn admits(&self, tool: &dyn Tool) -> bool {
        // Recursion is unconditionally forbidden in sub-agents.
        if tool.spawns_subagent() {
            return false;
        }
        // Control-flow tools (e.g. the abort/exit escape hatch) are
        // unconditionally forbidden in sub-agents — a spawned agent must never
        // be able to tear down the whole program.
        if tool.affects_control_flow() {
            return false;
        }
        // Tools that block on a human are gated by the profile.
        if tool.requires_user() && !self.allow_user_interaction {
            return false;
        }
        // Name whitelist: `None` admits everything; `Some(set)` admits only
        // listed names. This is the sole admission axis.
        match self.allowed_tools {
            None => true,
            Some(set) => set.iter().any(|name| *name == tool.name()),
        }
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

    /// Resolve this profile's declarative `write_paths` and `command_allowlist`
    /// grants into a runtime [`OperationScope`] against `cwd`.
    ///
    /// - Each `write_paths` spec (relative or absolute) is joined to `cwd` and
    ///   canonicalized best-effort (a not-yet-existing dir falls back to the
    ///   joined path). An empty `write_paths` leaves the path dimension
    ///   unconstrained (`None`), not "no paths".
    /// - `command_allowlist` becomes a [`CommandScope`]. An empty allowlist
    ///   leaves the command dimension unconstrained (`None`) — distinct from an
    ///   allowlist of `["*"]`, which means "any command".
    ///
    /// The resulting scope is what the spawned agent enforces on every admitted
    /// tool via [`OperationScope::allows`]. See ADR-0028.
    pub fn resolve_operation_scope(&self, cwd: &Path) -> OperationScope {
        let paths = if self.tool_policy.write_paths.is_empty() {
            None
        } else {
            Some(
                self.tool_policy
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
                    .collect(),
            )
        };
        let commands = if self.tool_policy.command_allowlist.is_empty() {
            None
        } else {
            Some(CommandScope::new(
                self.tool_policy
                    .command_allowlist
                    .iter()
                    .map(|s| s.to_string()),
            ))
        };
        OperationScope { paths, commands }
    }
}

/// Tools a read-only subagent (EXPLORE / REVIEW / TITLE) may use: pure
/// inspection with no side effects. Listed by name so adding a new
/// side-effecting tool to the parent never silently widens these profiles.
const READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "read_image",
    "grep",
    "glob",
    "list_dir",
    "webfetch",
    "websearch",
];

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
        allowed_tools: Some(READ_ONLY_TOOLS),
        allow_user_interaction: false,
        write_paths: &[],
        command_allowlist: &[],
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
        allowed_tools: Some(READ_ONLY_TOOLS),
        allow_user_interaction: false,
        write_paths: &[],
        command_allowlist: &[],
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
        allowed_tools: Some(READ_ONLY_TOOLS),
        allow_user_interaction: false,
        write_paths: &[],
        command_allowlist: &[],
    },
    auto_approve: true,
};

/// The interactive subagent role (ADR-0029). The built-in roles
/// ([`EXPLORE`]) are autonomous: `auto_approve: true` and
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
        allowed_tools: None,
        allow_user_interaction: true,
        write_paths: &[],
        command_allowlist: &[],
    },
    auto_approve: false,
};

/// Tools a quant-analysis subagent may use: the read-only quant domain tools
/// (market data, backtest, position review) plus the generic read-only
/// inspection tools (so it can read strategy code, configs, logs). Crucially,
/// this list excludes live-trading tools (`place_order`) and all coding
/// write/edit tools — a quant *analyst* role observes and reasons, it does not
/// trade or mutate the repo. Listed by name so adding a new tool to the parent
/// never silently widens this profile.
const QUANT_ANALYSIS_TOOLS: &[&str] = &[
    // Generic read-only inspection (shared with EXPLORE).
    "read_file",
    "read_image",
    "grep",
    "glob",
    "list_dir",
    "webfetch",
    "websearch",
    // Quant domain — read-only.
    "market_data",
    "backtest",
    "list_positions",
];

/// The quant-analysis subagent role. Read-only and non-interactive like
/// [`EXPLORE`], but scoped to a quantitative-trading domain: it may pull
/// market data, run backtests, and review open positions, but it cannot place
/// live orders or modify any file. A quant agent that *should* trade binds a
/// different profile (one that admits `place_order` under user supervision —
/// the analogue of [`INTERACTIVE`] for the quant domain); this one is the
/// "research before you risk capital" shape.
///
/// This profile exists precisely so the per-role tools-分配 requirement holds:
/// a quant analyst never sees coding tools (`write_file`, `edit_file`, `bash`),
/// and a coding agent never sees quant tools (`market_data`, `place_order`).
/// The two domains are isolated by name at the profile layer, which is the
/// single source of truth for what a bounded subagent may touch (ADR-0011).
pub const QUANT: SubagentProfile = SubagentProfile {
    name: "quant",
    system_prompt: "\
You are a quantitative-trading analysis subagent. Your job is to research and \
evaluate trading strategies using read-only tools: pull market data, run \
backtests, and review open positions. Report findings concisely with concrete \
numbers (returns, Sharpe, drawdown, exposure). You must not place any live \
order, cancel any order, or modify any file — if a trade is warranted, state \
the recommendation and stop; the user or a trading-authorized agent will act \
on it. You are non-interactive: never ask a question; if data is missing, say \
so. Run at most a handful of tool rounds, then answer.",
    tool_policy: ToolPolicy {
        allowed_tools: Some(QUANT_ANALYSIS_TOOLS),
        allow_user_interaction: false,
        write_paths: &[],
        command_allowlist: &[],
    },
    auto_approve: true,
};

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// A configurable test tool used to exercise every admission branch. The
    /// admission axis is now *name* (no capability ladder), so each Stub is
    /// parameterized by the tool name it claims.
    struct Stub {
        name: &'static str,
        requires_user: bool,
        spawns_subagent: bool,
        affects_control_flow: bool,
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
        fn requires_user(&self) -> bool {
            self.requires_user
        }
        fn spawns_subagent(&self) -> bool {
            self.spawns_subagent
        }
        fn affects_control_flow(&self) -> bool {
            self.affects_control_flow
        }
        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("stub".to_string())
        }
    }

    /// Build a plain tool named `name`. The flags default to "harmless" — the
    /// admission axis is the name itself.
    fn make(name: &'static str) -> Stub {
        Stub {
            name,
            requires_user: false,
            spawns_subagent: false,
            affects_control_flow: false,
        }
    }

    fn with_user(mut t: Stub) -> Stub {
        t.requires_user = true;
        t
    }

    fn with_spawn(mut t: Stub) -> Stub {
        t.spawns_subagent = true;
        t
    }

    /// A control-flow tool shape (the `abort` tool's shape). Used to prove
    /// profiles exclude control tools by the control-flow flag, regardless of
    /// name.
    fn make_control() -> Stub {
        Stub {
            name: "abort",
            requires_user: false,
            spawns_subagent: false,
            affects_control_flow: true,
        }
    }

    #[test]
    fn explore_admits_a_whitelisted_read_tool() {
        assert!(EXPLORE.tool_policy.admits(&make("read_file")));
        assert!(EXPLORE.tool_policy.admits(&make("grep")));
    }

    #[test]
    fn explore_rejects_a_non_whitelisted_tool() {
        // write_file is not in READ_ONLY_TOOLS — a research explorer must not
        // mutate files.
        assert!(!EXPLORE.tool_policy.admits(&make("write_file")));
        // bash is also not whitelisted.
        assert!(!EXPLORE.tool_policy.admits(&make("bash")));
    }

    #[test]
    fn explore_rejects_a_whitelisted_tool_that_requires_user() {
        // ask_user is not whitelisted, but even a whitelisted name is rejected
        // when requires_user is set and the profile disallows interaction.
        assert!(!EXPLORE.tool_policy.admits(&with_user(make("read_file"))));
    }

    #[test]
    fn explore_rejects_dispatch_tool_even_if_named_like_a_read() {
        // Recursion is absolute: even a whitelisted name is excluded when it
        // spawns a subagent.
        assert!(!EXPLORE.tool_policy.admits(&with_spawn(make("read_file"))));
    }

    #[test]
    fn explore_rejects_control_flow_tool() {
        assert!(!EXPLORE.tool_policy.admits(&make_control()));
    }

    #[test]
    fn recursion_is_rejected_even_by_a_permissive_policy() {
        let permissive = ToolPolicy {
            allowed_tools: None,
            allow_user_interaction: true,
            write_paths: &[],
            command_allowlist: &[],
        };
        assert!(!permissive.admits(&with_spawn(make("read_file"))));
        assert!(permissive.admits(&make("bash")));
    }

    #[test]
    fn control_flow_is_rejected_even_by_a_permissive_policy() {
        let permissive = ToolPolicy {
            allowed_tools: None,
            allow_user_interaction: true,
            write_paths: &[],
            command_allowlist: &[],
        };
        assert!(!permissive.admits(&make_control()));
    }

    #[test]
    fn select_tools_filters_by_name() {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(make("read_file")),  // admit (whitelisted)
            Arc::new(make("bash")),       // reject (not whitelisted)
            Arc::new(make("write_file")), // reject (not whitelisted)
            Arc::new(make("grep")),       // admit (whitelisted)
            Arc::new(with_spawn(make("read_file"))), // reject (recursion)
        ];
        let selected = EXPLORE.select_tools(&tools);
        assert_eq!(selected.len(), 2);
        let names: Vec<&str> = selected.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"grep"));
    }

    #[test]
    fn none_allowed_tools_admits_everything_named() {
        let open = ToolPolicy {
            allowed_tools: None,
            allow_user_interaction: false,
            write_paths: &[],
            command_allowlist: &[],
        };
        assert!(open.admits(&make("read_file")));
        assert!(open.admits(&make("bash")));
        assert!(open.admits(&make("write_file")));
    }

    /// INTERACTIVE (ADR-0029): `allowed_tools: None` admits every named tool,
    /// `allow_user_interaction` admits ask_user, but recursion and control-flow
    /// are still absolute. Its `auto_approve: false` (asserted at runtime by the
    /// duplex test) is what surfaces its calls as PermissionRequests.
    #[test]
    fn interactive_admits_all_named_tools_but_not_recursion_or_control() {
        use crate::INTERACTIVE;
        assert!(INTERACTIVE.tool_policy.admits(&make("read_file")));
        assert!(INTERACTIVE.tool_policy.admits(&make("bash")));
        assert!(INTERACTIVE.tool_policy.admits(&make("write_file")));
        assert!(INTERACTIVE.tool_policy.admits(&with_user(make("ask_user"))));
        // Recursion and control-flow are still absolute.
        assert!(!INTERACTIVE.tool_policy.admits(&with_spawn(make("read_file"))));
        assert!(!INTERACTIVE.tool_policy.admits(&make_control()));
    }

    /// QUANT is the per-role isolation contract between the coding and
    /// quant domains (the "tools 分配应该不同,不要搞混" requirement). It
    /// admits the read-only quant tools and shared read-only inspection tools,
    /// but excludes: live trading (place_order), every coding write/edit tool,
    /// bash, and recursion/control. This test pins the domain boundary so a
    /// future tool added to either domain cannot leak across it without an
    /// explicit profile edit.
    #[test]
    fn quant_profile_isolates_domain_and_excludes_trading_and_coding() {
        use crate::QUANT;
        // Quant read-only domain tools: admitted.
        assert!(QUANT.tool_policy.admits(&make("market_data")));
        assert!(QUANT.tool_policy.admits(&make("backtest")));
        assert!(QUANT.tool_policy.admits(&make("list_positions")));
        // Shared read-only inspection: admitted.
        assert!(QUANT.tool_policy.admits(&make("read_file")));
        assert!(QUANT.tool_policy.admits(&make("grep")));
        // Live trading is NOT admitted — a quant analyst recommends, never
        // trades. Trading needs a separate, user-supervised profile.
        assert!(!QUANT.tool_policy.admits(&make("place_order")));
        // Coding write/edit tools are NOT admitted — domain isolation.
        assert!(!QUANT.tool_policy.admits(&make("write_file")));
        assert!(!QUANT.tool_policy.admits(&make("edit_file")));
        assert!(!QUANT.tool_policy.admits(&make("bash")));
        // Recursion and control-flow remain absolute.
        assert!(!QUANT.tool_policy.admits(&with_spawn(make("read_file"))));
        assert!(!QUANT.tool_policy.admits(&make_control()));
    }

    /// The reciprocal of the domain boundary: EXPLORE (the coding agent's
    /// research role) must not see quant tools either. Isolation is symmetric —
    /// a coding agent's context never carries quant schemas, and vice versa.
    #[test]
    fn explore_profile_excludes_quant_tools() {
        assert!(!EXPLORE.tool_policy.admits(&make("market_data")));
        assert!(!EXPLORE.tool_policy.admits(&make("backtest")));
        assert!(!EXPLORE.tool_policy.admits(&make("place_order")));
        assert!(!EXPLORE.tool_policy.admits(&make("list_positions")));
    }
}

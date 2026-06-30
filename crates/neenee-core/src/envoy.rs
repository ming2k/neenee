//! Envoy profiles: declarative tool-permission roles for autonomous
//! envoys spawned by the `task` tool (and wrappers like
//! `verify_plan_execution`).
//!
//! ## Why this exists
//!
//! Before ADR-0011 the envoy's toolset was a hardcoded filter inside
//! the dispatch tool (`access() == Read` plus a name exclusion for itself).
//! That had two problems:
//!
//! 1. **It was name-driven, not semantic.** `ask_user` is `Read`, so it
//!    passed the filter and reached the envoy. But an envoy is
//!    autonomous and non-interactive — its `UserQuestionRequest` events are
//!    dropped by the envoy tool's event forwarder, so the request deadlocks
//!    until the parent turn is cancelled. The user could see the call but
//!    could not answer it.
//! 2. **The policy was buried in orchestration code.** Adding a second
//!    envoy role (or tightening the existing one) meant editing the
//!    dispatch tool rather than declaring intent.
//!
//! The fix is a profile primitive that expresses the tool policy in terms of
//! [`Tool`] capability axes — [`Tool::scope_target`], [`Tool::requires_user`],
//! [`Tool::spawns_envoy`] — so admission is data-driven and generalizes to
//! future tools without touching the dispatch path.
//!
//! ## The capability axes
//!
//! - [`Tool::scope_target`] — what the call touches (`Read` vs `Write` path). Existing.
//! - [`Tool::requires_user`] — may block on a live human (e.g. `ask_user`).
//! - [`Tool::spawns_envoy`] — dispatches a nested agent (e.g. `task`).
//!
//! Recursion is unconditionally forbidden in any envoy: a tool that
//! `spawns_envoy` is never admitted, regardless of profile. User
//! interaction is a per-profile knob ([`ToolPolicy::allow_user_interaction`])
//! so a future interactive role could opt in once the plumbing surfaces the
//! request; the built-in [`EXPLORE`] profile leaves it off.

use std::path::Path;
use std::sync::Arc;

use crate::model::Model;
use crate::{CommandScope, OperationScope, Tool, ToolScope, ToolSelection, ToolSet};

/// Ceiling on what an envoy may do. There is no capability ladder — a tool is
/// admitted purely by name. [`Tool::spawns_envoy`] and
/// [`Tool::affects_control_flow`] tools are always excluded (recursion and
/// program teardown are absolute, not per-profile toggles). See ADR-0011/0028.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPolicy {
    /// Which tools an envoy under this policy may use, by name. `None` admits
    /// the full parent toolset (the main agent's shape); `Some(set)` admits only
    /// tools whose `name()` is in the set. This is the sole admission axis —
    /// there is no capability ladder, so adding a new side-effecting tool to the
    /// parent does *not* silently widen an envoy unless its name is listed.
    pub allowed_tools: Option<&'static [&'static str]>,
    /// Whether tools that block on a human ([`Tool::requires_user`]) may run.
    pub allow_user_interaction: bool,
    /// Declarative write grant: directory specs (relative or absolute) a
    /// envoy under this policy may write to. Empty (the default) leaves write
    /// paths unconstrained; set to e.g. `&["./src"]` to confine writes there. At
    /// spawn, [`EnvoyProfile::resolve_operation_scope`] canonicalizes these
    /// against the cwd into a runtime path constraint the agent enforces. See
    /// ADR-0028.
    pub write_paths: &'static [&'static str],
    /// Declarative command grant: program-name prefixes an envoy under this
    /// policy may run via `bash`. Empty (the default) means "no command
    /// constraint" — any command is allowed up to the broker. Set to e.g.
    /// `&["git", "cargo"]` to restrict the envoy to those programs. Resolved
    /// at spawn by [`EnvoyProfile::resolve_operation_scope`] into a
    /// [`CommandScope`].
    pub command_allowlist: &'static [&'static str],
}

impl ToolPolicy {
    /// Returns `true` if a tool may be handed to an envoy under this policy.
    /// Combines the **name scope** ([`allowed_tools`](Self::allowed_tools)) with
    /// the **runtime hard rules** ([`admits_runtime`](Self::admits_runtime)).
    pub fn admits(&self, tool: &dyn Tool) -> bool {
        self.admits_runtime(tool) && self.scope().admits(tool.name())
    }

    /// The envoy hard rules that are independent of the name whitelist:
    /// recursion ([`Tool::spawns_envoy`]) and program teardown
    /// ([`Tool::affects_control_flow`]) are absolute, and human-blocking tools
    /// ([`Tool::requires_user`]) are gated by
    /// [`allow_user_interaction`](Self::allow_user_interaction). These are not
    /// expressible as a capability *name* scope, so the pool resolver (which
    /// handles name scope + the model-capability filter) cannot apply them — the
    /// envoy resolution applies this as a post-filter. See
    /// [`EnvoyProfile::resolve_tools`].
    pub fn admits_runtime(&self, tool: &dyn Tool) -> bool {
        // Recursion is unconditionally forbidden in envoys.
        if tool.spawns_envoy() {
            return false;
        }
        // Control-flow tools (e.g. the abort/exit escape hatch) are
        // unconditionally forbidden in envoys — a spawned agent must never
        // be able to tear down the whole program.
        if tool.affects_control_flow() {
            return false;
        }
        // Tools that block on a human are gated by the profile.
        if tool.requires_user() && !self.allow_user_interaction {
            return false;
        }
        true
    }

    /// This policy's capability **name scope** for the pool resolver: `None`
    /// [`allowed_tools`](Self::allowed_tools) → [`ToolScope::All`]; `Some(set)`
    /// → [`ToolScope::Only`] the listed names. The runtime hard rules
    /// ([`admits_runtime`](Self::admits_runtime)) are layered on separately.
    pub fn scope(&self) -> ToolScope {
        match self.allowed_tools {
            None => ToolScope::All,
            Some(names) => ToolScope::only(names.iter().copied()),
        }
    }
}

/// A declarative envoy role: a name, the system-prompt fragment that
/// frames the role, and the [`ToolPolicy`] that scopes what it may touch.
///
/// Profiles live in `neenee-core` (domain vocabulary) so dispatch tools in
/// `neenee-agent` resolve them without re-implementing admission logic. The
/// built-in [`EXPLORE`] profile is what `task` binds to today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvoyProfile {
    pub name: &'static str,
    pub system_prompt: &'static str,
    pub tool_policy: ToolPolicy,
    /// The profile's variant pins (the agent-side **override** axis): a list of
    /// `(capability, variant_id)` this role forces, regardless of the model's
    /// own preference. Empty for every built-in role — they accept whatever
    /// variant the model resolves. A non-empty pin wins over the model's choice
    /// for that capability (agent-over-model), but can still be overridden
    /// *down* by the model's hard capability limit if the pinned variant is
    /// unusable. See [`ToolSet::resolve_for`].
    pub variant_pins: &'static [(&'static str, &'static str)],
    /// Whether the spawned envoy runs its admitted write/execute tools
    /// unattended, bypassing the permission broker. Full-duplex (ADR-0029): the
    /// built-in profiles keep this `true` to preserve the legacy autonomous
    /// contract (the broker's `PermissionRequest` would otherwise surface up
    /// to the parent, which historically had no path to answer it). Now that
    /// the up-direction (forwarding) and down-direction (registry → handle →
    /// `reply_permission`) are wired, a future interactive profile can set
    /// this `false` so an envoy's tool calls prompt the user through the
    /// same modal a top-level call uses, and the reply routes back down.
    pub unattended: bool,
    /// Whether an envoy spawned under this profile may have the **model**
    /// supply stdin bytes for a `bash` call it emits (the opt-in automatic-
    /// flow path). Default `false` for every built-in profile: autonomous
    /// envoys run non-interactively (the L1 hard floor + L2 idle watchdog
    /// keep them from hanging); a profile aimed at unattended CI/batch flows
    /// where no human is reachable can set this `true` so the model can feed
    /// a command's stdin directly. Without it, stdin is structurally
    /// unreachable from the model's arguments even inside an envoy.
    pub allow_model_stdin: bool,
}

impl EnvoyProfile {
    /// This profile's [`ToolSelection`] — the agent-identity selector it hands
    /// the pool: the capability **name scope** from its [`ToolPolicy`], plus its
    /// own variant pins (the **override** axis, agent side). Built-in profiles
    /// pin nothing, so they accept the model's variant for every capability;
    /// a profile that pins a variant takes precedence over the model's choice
    /// (agent-over-model) — see [`ToolSet::resolve_for`].
    pub fn selection(&self) -> ToolSelection {
        ToolSelection {
            scope: self.tool_policy.scope(),
            variants: self
                .variant_pins
                .iter()
                .map(|(cap, var)| (cap.to_string(), var.to_string()))
                .collect(),
        }
    }

    /// Resolve the pool down to the toolset a spawned envoy on `model` actually
    /// gets. This is the envoy's whole admission story in one call:
    ///
    /// 1. [`ToolSet::resolve_for`] composes this profile's [`selection`](Self::selection)
    ///    with the model's selection (`model_sel`) — scope by intersection,
    ///    variants by agent-over-model precedence, the model's capability limits
    ///    applied hard.
    /// 2. The envoy **runtime hard rules** ([`ToolPolicy::admits_runtime`]) are
    ///    applied as a post-filter: recursion, control-flow, and (unless the
    ///    profile opts in) human-blocking tools are stripped regardless of name.
    ///
    /// The result is the variant-resolved, model-legal, role-scoped tool list to
    /// hand the child agent.
    pub fn resolve_tools(
        &self,
        toolset: &ToolSet,
        model: &Model,
        model_sel: &ToolSelection,
    ) -> Vec<Arc<dyn Tool>> {
        toolset
            .resolve_for(model, &self.selection(), model_sel)
            .into_iter()
            .filter(|tool| self.tool_policy.admits_runtime(tool.as_ref()))
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

/// Tools a read-only envoy (EXPLORE / REVIEW / TITLE) may use: pure
/// inspection with no side effects. Listed by name so adding a new
/// side-effecting tool to the parent never silently widens these profiles.
const READ_ONLY_TOOLS: &[&str] = &[
    "read_text",
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
pub const EXPLORE: EnvoyProfile = EnvoyProfile {
    name: "explore",
    system_prompt: "\
You are a focused research envoy. Your single job is to answer the assigned \
task accurately and concisely using read-only tools. Explore the workspace or \
the web as needed, then write a clear, complete final answer with the key \
findings (file paths, signatures, relevant snippets, conclusions). \
You are non-interactive: never ask the user any \
question — if information is missing, make a reasonable assumption, note it \
explicitly in your answer, or report that you could not find it. Run at most a \
handful of tool rounds, then answer.",
    tool_policy: ToolPolicy {
        allowed_tools: Some(READ_ONLY_TOOLS),
        allow_user_interaction: false,
        write_paths: &[],
        command_allowlist: &[],
    },
    variant_pins: &[],
    unattended: true,
    allow_model_stdin: false,
};

/// The diagnostic role used by session review (ADR-0016). Read-only,
/// non-interactive, non-recursive — like [`EXPLORE`] in capability, but framed
/// as a health auditor that reasons over a handed-off transcript snapshot and
/// returns structured verdicts rather than free-form research findings. Bound
/// by `EnvoyTool`-style machinery in `neenee-agent` (`Agent::run_session_review`),
/// never by a model tool call.
pub const REVIEW: EnvoyProfile = EnvoyProfile {
    name: "review",
    system_prompt: "\
You are a session-health diagnostic envoy. You are handed a snapshot of \
another agent's live transcript and asked whether it is making progress or \
stuck. Judge from what you see — the sequence of tool calls, whether the same \
ground is being revisited, whether edits or commands are actually landing. \
You may read files to check a claim. You are \
non-interactive: never ask a question; if you cannot tell, say so. Answer with \
the requested structured verdict only, no preamble.",
    tool_policy: ToolPolicy {
        allowed_tools: Some(READ_ONLY_TOOLS),
        allow_user_interaction: false,
        write_paths: &[],
        command_allowlist: &[],
    },
    variant_pins: &[],
    unattended: true,
    allow_model_stdin: false,
};

/// The session-titling role (ADR-0022). Read-only and non-interactive like
/// [`REVIEW`], but its task is pure text-in/text-out — it admits no tool loop at
/// all. The runner (`Agent::generate_title`) makes a single `provider.chat()`
/// framed by this prompt and normalizes the reply via `clean_title`. Declared as
/// a profile (not an ad-hoc call) so the capability-axis vocabulary stays the
/// single source of truth for what a bounded envoy may do, per ADR-0011.
pub const TITLE: EnvoyProfile = EnvoyProfile {
    name: "title",
    system_prompt: "\
You are a session-titling envoy. You are shown an excerpt of a conversation \
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
    variant_pins: &[],
    unattended: true,
    allow_model_stdin: false,
};

/// The interactive envoy role (ADR-0029). The built-in roles
/// ([`EXPLORE`]) are autonomous: `unattended: true` and
/// no `requires_user` tools, so they never block on a human. This role is the
/// opposite shape — it is meant to run **under user supervision**: a `Write`
/// ceiling admits the full tool ladder (read + execute + write),
/// `allow_user_interaction` admits `ask_user`, and `unattended: false` leaves
/// the permission broker on, so every execute/write surfaces as a
/// `EnvoyEvent::PermissionRequest` that round-trips through the parent
/// harness ↔ TUI ↔ registry handle.
///
/// It is the "turn the duplex on" role: bind it to a dispatch tool to get a
/// envoy whose tool calls and questions reach the user in real time and
/// whose replies route back down. Left unbound by the built-in `envoy`
/// tool (which stays `EXPLORE`) because forcing every research envoy to
/// prompt would defeat the point of autonomous exploration — opting a
/// specific dispatch tool into `INTERACTIVE` is a product-level decision.
pub const INTERACTIVE: EnvoyProfile = EnvoyProfile {
    name: "interactive",
    system_prompt: "\
You are an interactive envoy operating under user supervision. You may read \
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
    variant_pins: &[],
    unattended: false,
    allow_model_stdin: false,
};

/// Tools a quant-analysis envoy may use: the read-only quant domain tools
/// (market data, backtest, position review) plus the generic read-only
/// inspection tools (so it can read strategy code, configs, logs). Crucially,
/// this list excludes live-trading tools (`place_order`, `cancel_order`) and all coding
/// write/edit tools — a quant *analyst* role observes and reasons, it does not
/// trade or mutate the repo. Listed by name so adding a new tool to the parent
/// never silently widens this profile.
const QUANT_ANALYSIS_TOOLS: &[&str] = &[
    // Generic read-only inspection (shared with EXPLORE).
    "read_text",
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

/// The quant-analysis envoy role. Read-only and non-interactive like
/// [`EXPLORE`], but scoped to a quantitative-trading domain: it may pull
/// market data, run backtests, and review open positions, but it cannot place
/// live orders or modify any file. A quant agent that *should* trade binds a
/// different profile (one that admits `place_order` / `cancel_order` under user
/// supervision — the analogue of [`INTERACTIVE`] for the quant domain); this one is the
/// "research before you risk capital" shape.
///
/// This profile exists precisely so the per-role tool-allocation requirement holds:
/// a quant analyst never sees coding tools (`write_file`, `edit_file`, `bash`),
/// and a coding agent never sees quant tools (`market_data`, `place_order`, `cancel_order`).
/// The two domains are isolated by name at the profile layer, which is the
/// single source of truth for what a bounded envoy may touch (ADR-0011).
pub const QUANT: EnvoyProfile = EnvoyProfile {
    name: "quant",
    system_prompt: "\
You are a quantitative-trading analysis envoy. Your job is to research and \
evaluate trading strategies using read-only tools: pull market data, run \
backtests, and review open positions. Report findings concisely with concrete \
numbers (returns, Sharpe, drawdown, exposure). You are non-interactive: never ask a question; if data is missing, say \
so. Run at most a handful of tool rounds, then answer.",
    tool_policy: ToolPolicy {
        allowed_tools: Some(QUANT_ANALYSIS_TOOLS),
        allow_user_interaction: false,
        write_paths: &[],
        command_allowlist: &[],
    },
    variant_pins: &[],
    unattended: true,
    allow_model_stdin: false,
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
        spawns_envoy: bool,
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
        fn spawns_envoy(&self) -> bool {
            self.spawns_envoy
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
            spawns_envoy: false,
            affects_control_flow: false,
        }
    }

    fn with_user(mut t: Stub) -> Stub {
        t.requires_user = true;
        t
    }

    fn with_spawn(mut t: Stub) -> Stub {
        t.spawns_envoy = true;
        t
    }

    /// A control-flow tool shape (the `abort` tool's shape). Used to prove
    /// profiles exclude control tools by the control-flow flag, regardless of
    /// name.
    fn make_control() -> Stub {
        Stub {
            name: "abort",
            requires_user: false,
            spawns_envoy: false,
            affects_control_flow: true,
        }
    }

    #[test]
    fn explore_admits_a_whitelisted_read_tool() {
        assert!(EXPLORE.tool_policy.admits(&make("read_text")));
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
        assert!(!EXPLORE.tool_policy.admits(&with_user(make("read_text"))));
    }

    #[test]
    fn explore_rejects_dispatch_tool_even_if_named_like_a_read() {
        // Recursion is absolute: even a whitelisted name is excluded when it
        // spawns an envoy.
        assert!(!EXPLORE.tool_policy.admits(&with_spawn(make("read_text"))));
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
        assert!(!permissive.admits(&with_spawn(make("read_text"))));
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

    /// A test model (vision-capable; the Stub tools require no vision, so the
    /// model-capability filter is a no-op here — this test isolates the scope +
    /// runtime-rule composition).
    fn test_model() -> Model {
        Model {
            id: "test",
            name: "Test",
            family: "test",
            context_window: 100_000,
            thinking: crate::thinking::ThinkingSupport::None,
            tool_call: true,
            vision: true,
            format: crate::WireFormat::OpenAiCompat,
            model_guidance: "",
            effort_levels: &[],
        }
    }

    #[test]
    fn resolve_tools_applies_scope_and_runtime_rules() {
        // `grep` is whitelisted → admitted. `bash` is not whitelisted → dropped
        // by scope. `read_text` is whitelisted *but spawns an envoy* → dropped
        // by the runtime recursion rule despite passing the name scope.
        let toolset = ToolSet::from_tools(vec![
            Arc::new(make("grep")) as Arc<dyn Tool>,
            Arc::new(make("bash")) as Arc<dyn Tool>,
            Arc::new(with_spawn(make("read_text"))) as Arc<dyn Tool>,
        ]);
        let selected =
            EXPLORE.resolve_tools(&toolset, &test_model(), &ToolSelection::unrestricted());
        let names: Vec<&str> = selected.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["grep"]);
    }

    #[test]
    fn none_allowed_tools_admits_everything_named() {
        let open = ToolPolicy {
            allowed_tools: None,
            allow_user_interaction: false,
            write_paths: &[],
            command_allowlist: &[],
        };
        assert!(open.admits(&make("read_text")));
        assert!(open.admits(&make("bash")));
        assert!(open.admits(&make("write_file")));
    }

    /// INTERACTIVE (ADR-0029): `allowed_tools: None` admits every named tool,
    /// `allow_user_interaction` admits ask_user, but recursion and control-flow
    /// are still absolute. Its `unattended: false` (asserted at runtime by the
    /// duplex test) is what surfaces its calls as PermissionRequests.
    #[test]
    fn interactive_admits_all_named_tools_but_not_recursion_or_control() {
        use crate::INTERACTIVE;
        assert!(INTERACTIVE.tool_policy.admits(&make("read_text")));
        assert!(INTERACTIVE.tool_policy.admits(&make("bash")));
        assert!(INTERACTIVE.tool_policy.admits(&make("write_file")));
        assert!(INTERACTIVE.tool_policy.admits(&with_user(make("ask_user"))));
        // Recursion and control-flow are still absolute.
        assert!(
            !INTERACTIVE
                .tool_policy
                .admits(&with_spawn(make("read_text")))
        );
        assert!(!INTERACTIVE.tool_policy.admits(&make_control()));
    }

    /// QUANT is the per-role isolation contract between the coding and
    /// quant domains (the "separate tool allocation per role" requirement). It
    /// admits the read-only quant tools and shared read-only inspection tools,
    /// but excludes: live trading (place_order/cancel_order), every coding
    /// write/edit tool, bash, and recursion/control. This test pins the domain boundary so a
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
        assert!(QUANT.tool_policy.admits(&make("read_text")));
        assert!(QUANT.tool_policy.admits(&make("grep")));
        // Live trading is NOT admitted — a quant analyst recommends, never
        // trades. Trading needs a separate, user-supervised profile.
        assert!(!QUANT.tool_policy.admits(&make("place_order")));
        assert!(!QUANT.tool_policy.admits(&make("cancel_order")));
        // Coding write/edit tools are NOT admitted — domain isolation.
        assert!(!QUANT.tool_policy.admits(&make("write_file")));
        assert!(!QUANT.tool_policy.admits(&make("edit_file")));
        assert!(!QUANT.tool_policy.admits(&make("bash")));
        // Recursion and control-flow remain absolute.
        assert!(!QUANT.tool_policy.admits(&with_spawn(make("read_text"))));
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
        assert!(!EXPLORE.tool_policy.admits(&make("cancel_order")));
        assert!(!EXPLORE.tool_policy.admits(&make("list_positions")));
    }
}

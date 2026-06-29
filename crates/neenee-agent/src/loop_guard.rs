//! Deterministic, non-terminating defence against read loops.
//!
//! A model occasionally gets stuck issuing the *same* read over and over —
//! re-reading one file (or thrashing between two pages of it) without making
//! progress. The agentic loop is uncapped by design (ADR-0009) and the harness
//! deliberately keeps no hard equality guard (the old `guard_repeated_call` and
//! the ADR-0030 review nudge were removed in favour of the model-driven `abort`
//! tool), so nothing automatically breaks the trajectory — and a model that is
//! looping is, by definition, not noticing it should `abort`. This module fills
//! that gap without resurrecting a hard cap.
//!
//! ## Why this is deterministic (not a semantic judgement)
//!
//! Identical read arguments return byte-for-byte identical content. So "the
//! model keeps re-issuing the same read" is a *provable* waste — it needs no LLM
//! to adjudicate, unlike the fuzzier "lots of reads but each different" case
//! that [`crate::session_review`] handles. Detection is pure string bookkeeping,
//! which is why it is free, instant, and has no false positives on legitimate
//! work: real research reads *different* things, so its signatures never repeat.
//!
//! ## Why a frequency window (not a consecutive counter)
//!
//! A naive "same signature N rounds in a row" counter misses the oscillation
//! pattern `A B A B A B` — the exact shape a two-page thrash produces — because
//! no signature is ever consecutive. Instead we keep a sliding window of the
//! last [`WINDOW`] read-round signatures and fire when any signature occurs
//! [`THRESHOLD`] times *within* the window. That catches `A A A`, `A B A B A`,
//! and anything in between, while leaving genuine forward paging (`A B C D E`,
//! all distinct) untouched.
//!
//! ## What firing does
//!
//! It returns a [`GuardAction`]. The escalation ladder is:
//! 1. **[`GuardAction::Inject`]** — the first time a signature reaches
//!    [`THRESHOLD`] (3) we inject a hidden user message ([`neenee_core::InjectionKind::LoopReviewNudge`])
//!    before the next model request: *you repeated this read, it is unchanged,
//!    change course.* This is a soft nudge; the model is free to ignore it.
//! 2. **[`GuardAction::Block`]** — if the model ignores the nudge and keeps
//!    repeating until [`ESCALATE_AT`] (6), we escalate from *asking* to
//!    *blocking*: the exact read signature is masked for the rest of the turn,
//!    so dispatch short-circuits any matching read and returns an explanatory
//!    error instead of executing it. The model physically cannot re-issue the
//!    looped read — it can only read something *different*. This is the rung
//!    that breaks a loop a nudge failed to. Surgical: it leaves all other
//!    reads, writes, and tools untouched.
//! 3. **`Esc` / `abort` / `hard_stop_rounds`** — the hard backstops remain for
//!    a turn that loops even past a block.
//!
//! A one-shot-per-signature latch keeps each rung from spamming. Block state is
//! per-turn (dropped with the `TurnState`), so it never leaks across turns.

use std::collections::{HashMap, HashSet, VecDeque};

// ===========================================================================
// Round-guard abstraction: a pluggable registry of per-round guards.
//
// `ReadLoopGuard` below is one implementation. A guard observes each completed
// tool round and may return a `GuardAction` — `Inject` a steering nudge, or
// `Abort` the turn. The `GuardRegistry` dispatches to all registered guards and
// merges their actions (Abort > Inject > Continue). This replaces the bespoke
// `loop_guard_enabled`/`pending_round`/`maybe_inject_loop_nudge` triplet with a
// uniform seam, so future guards (e.g. a write-rewrite guard) slot in without
// touching the loop body.
// ===========================================================================

/// The outcome a [`RoundGuard`] returns after observing a round.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum GuardAction {
    /// No action — the round is fine, keep going.
    #[default]
    Continue,
    /// Inject `message` as a hidden user message before the next model request.
    /// Non-terminating: the turn keeps running.
    Inject(String),
    /// Hard-block one or more read-call `signatures` for the remainder of this
    /// turn. The agent records them in a per-turn mask and short-circuits any
    /// subsequent read whose canonical signature matches, returning an
    /// explanatory `ToolOutput` instead of executing the read — so the model
    /// *cannot* re-issue the looped read, only read something *different*.
    /// Non-terminating and surgical: it leaves all other reads, writes, and
    /// tools untouched.
    ///
    /// `message` is a steering note injected alongside the block (same vehicle
    /// as [`Inject`](Self::Inject)) so the model learns *why* the read is now
    /// refused and what to do instead. Severity sits between `Inject` (a soft
    /// nudge the model is free to ignore — it often does, which is the whole
    /// problem this variant exists to solve) and `Abort` (which kills the whole
    /// turn). Escalation ladder: `Inject` first, `Block` if the loop persists,
    /// `Abort` as a last resort.
    Block {
        signatures: Vec<String>,
        message: String,
    },
    /// Abort the turn with `reason` as a terminal error. Hard-terminating.
    Abort(String),
}

impl GuardAction {
    /// Severity rank for merging: Abort > Block > Inject > Continue.
    fn severity(&self) -> u8 {
        match self {
            GuardAction::Continue => 0,
            GuardAction::Inject(_) => 1,
            GuardAction::Block { .. } => 2,
            GuardAction::Abort(_) => 3,
        }
    }

    /// Merge two actions, keeping the more severe. Two `Inject`s of equal
    /// severity concatenate so both nudges reach the model; two `Block`s merge
    /// their signature sets; a `Block` absorbs a weaker `Inject` by folding the
    /// inject's message into the block's. A more severe action always wins
    /// outright.
    pub fn merge(self, other: GuardAction) -> GuardAction {
        match (self.severity(), other.severity()) {
            // Equal severity: combine payloads.
            (1, 1) => {
                let GuardAction::Inject(mut s) = self else {
                    unreachable!("severity 1 is Inject")
                };
                let GuardAction::Inject(t) = other else {
                    unreachable!("severity 1 is Inject")
                };
                s.push_str("\n\n");
                s.push_str(&t);
                GuardAction::Inject(s)
            }
            (2, 2) => {
                let GuardAction::Block {
                    mut signatures,
                    mut message,
                } = self
                else {
                    unreachable!("severity 2 is Block")
                };
                let GuardAction::Block {
                    signatures: other_sigs,
                    message: other_msg,
                } = other
                else {
                    unreachable!("severity 2 is Block")
                };
                for sig in other_sigs {
                    if !signatures.contains(&sig) {
                        signatures.push(sig);
                    }
                }
                if !other_msg.is_empty() {
                    message.push_str("\n\n");
                    message.push_str(&other_msg);
                }
                GuardAction::Block {
                    signatures,
                    message,
                }
            }
            // Block absorbs a weaker Inject's message.
            (2, 1) => {
                let GuardAction::Block {
                    signatures,
                    mut message,
                } = self
                else {
                    unreachable!("severity 2 is Block")
                };
                if let GuardAction::Inject(t) = other
                    && !t.is_empty()
                {
                    message.push_str("\n\n");
                    message.push_str(&t);
                }
                GuardAction::Block {
                    signatures,
                    message,
                }
            }
            // Winner is whichever side is more severe.
            (a, b) if a >= b => self,
            _ => other,
        }
    }
}

/// Read-only view of one completed tool round, fed to each [`RoundGuard`].
/// Borrows from the agent's `ToolCall` list so no allocation is needed.
#[derive(Debug, Clone, Copy)]
pub struct GuardRound<'a> {
    /// The tool calls issued this round: `(name, arguments)`.
    pub calls: &'a [(&'a str, &'a str)],
    /// Whether *every* call in `calls` targets an `Unspecified` scope (i.e.
    /// pure read/search, no Path/Command). The caller (which owns the
    /// tool-access classifier) computes this once and passes it in.
    pub all_read: bool,
}

/// A deterministic round-level guard. Observes each completed tool round and may
/// return a [`GuardAction`] — inject a steering nudge, or abort the turn.
///
/// A guard is stateful and scoped to one turn (lives in `TurnState`): it
/// observes each round, carrying state across rounds within that turn. Dropped
/// when the turn ends, so state never crosses turns. Implementations must be
/// cheap (no model calls) — they run on the hot round-boundary path. and is dropped
/// when the turn ends, so state never crosses turns. Implementations must be
/// cheap (no model calls) — they run on the hot round-boundary path.
pub trait RoundGuard: Send + Sync {
    /// Observe one completed round. Return an action to inject or abort.
    fn observe(&mut self, round: GuardRound<'_>) -> GuardAction;

    /// Reset all internal state (called when a progress round clears the slate,
    /// and at turn start). Default does nothing.
    fn reset(&mut self) {}
}

/// An ordered collection of [`RoundGuard`]s. Dispatches each round to all
/// registered guards and merges their actions. Lives in `TurnState`; per-turn.
#[derive(Default)]
pub struct GuardRegistry {
    guards: Vec<Box<dyn RoundGuard>>,
}

impl GuardRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a guard. Guards fire in registration order.
    pub fn register(&mut self, guard: Box<dyn RoundGuard>) {
        self.guards.push(guard);
    }

    /// Whether any guards are registered.
    pub fn is_empty(&self) -> bool {
        self.guards.is_empty()
    }

    /// Dispatch `round` to every guard, merging their actions. The merged
    /// action is applied by the caller (inject nudge, abort turn).
    pub fn observe(&mut self, round: GuardRound<'_>) -> GuardAction {
        self.guards
            .iter_mut()
            .map(|g| g.observe(round))
            .fold(GuardAction::Continue, GuardAction::merge)
    }

    /// Reset every guard's internal state (e.g. on a progress round).
    pub fn reset(&mut self) {
        for g in &mut self.guards {
            g.reset();
        }
    }
}

/// Per-turn state bundling a [`GuardRegistry`] with the tool-call data for the
/// round just dispatched. The agent loop sets `pending_calls`
/// and `pending_all_read` in `dispatch_tool_calls`,
/// then consumes them once at the round boundary via [`Self::take_action`].
///
/// This replaces the old `pending_round: RoundClass` + `loop_guard: ReadLoopGuard`
/// pair with a single uniform seam that dispatches to any number of guards.
#[derive(Default)]
pub struct RoundGuardState {
    /// The registry of guards. Dispatches to all of them.
    registry: GuardRegistry,
    /// The tool calls from the round just dispatched, stored as owned `(name,
    /// args)` pairs so they outlive the borrowed `GuardRound`. Consumed once.
    pending_calls: Vec<(String, String)>,
    /// Whether `pending_calls` was classified as all-read. `true` by default so
    /// an empty round (no calls) doesn't spuriously reset guards.
    pending_all_read: bool,
    /// Read-call signatures hard-blocked by a guard this turn
    /// ([`GuardAction::Block`]). The dispatch layer consults
    /// [`Self::is_blocked`] before executing a read and short-circuits any
    /// match. Per-turn: cleared when the turn ends (the `TurnState` owning it
    /// is dropped), so a block never leaks across turns. A progress round does
    /// *not* clear it — blocking a proven-looping read for the remainder of the
    /// turn is the point, even if the model makes other progress in between.
    blocked_signatures: HashSet<String>,
}

impl RoundGuardState {
    /// Build guard state with a pre-populated registry.
    pub fn new(registry: GuardRegistry) -> Self {
        Self {
            registry,
            pending_calls: Vec::new(),
            pending_all_read: true,
            blocked_signatures: HashSet::new(),
        }
    }

    /// Record the classification of the round just dispatched. Called once per
    /// round in `dispatch_tool_calls`. When `all_read` is false, also resets the
    /// guards immediately (a progress round clears the slate).
    pub fn set_round(&mut self, calls: Vec<(String, String)>, all_read: bool) {
        self.pending_calls = calls;
        self.pending_all_read = all_read;
        if !all_read {
            self.registry.reset();
        }
    }

    /// Consume the pending round: dispatch it to every guard and return the
    /// merged [`GuardAction`]. Called once at the round boundary. After this,
    /// the pending state is cleared until the next `set_round`. A
    /// [`GuardAction::Block`] returned by a guard is applied here as a side
    /// effect — its signatures are added to [`blocked_signatures`](Self::blocked_signatures)
    /// — *before* the action is returned, so the caller only needs to handle
    /// the message/inject surface.
    pub fn take_action(&mut self) -> GuardAction {
        if !self.pending_all_read {
            self.pending_calls.clear();
            return GuardAction::Continue;
        }
        // Build borrowed views from the owned pairs.
        let borrows: Vec<(&str, &str)> = self
            .pending_calls
            .iter()
            .map(|(n, a)| (n.as_str(), a.as_str()))
            .collect();
        let round = GuardRound {
            calls: &borrows,
            all_read: true,
        };
        let action = self.registry.observe(round);
        self.pending_calls.clear();
        // Apply any hard block: record the signatures so subsequent reads of
        // exactly this shape are short-circuited at dispatch.
        if let GuardAction::Block { ref signatures, .. } = action {
            for sig in signatures {
                self.blocked_signatures.insert(sig.clone());
            }
        }
        action
    }

    /// Whether a single read call (name + raw args) is hard-blocked this turn.
    /// Checks **both** axes the guard keys on: the exact signature
    /// (`name|path|offset|limit`) and the path bucket (`name|path`). A call is
    /// blocked if *either* matches a masked entry — this mirrors how the guard
    /// detects, so blocking an exact read blocks that exact call, and blocking
    /// a path bucket blocks every read of that file regardless of offset.
    pub fn is_blocked(&self, name: &str, args: &str) -> bool {
        if self.blocked_signatures.is_empty() {
            return false;
        }
        let exact = read_signature([(name, args)]);
        if self.blocked_signatures.contains(&exact) {
            return true;
        }
        // A block keyed on the path bucket (`name|path`) must catch reads of
        // that file at any offset. The exact signature of such a read is
        // `name|path|offset|limit`, which won't string-equal `name|path`, so we
        // check the path bucket too.
        let path = path_signature([(name, args)]);
        self.blocked_signatures.contains(&path)
    }

    /// A compact, log-friendly summary of what is currently blocked. Returns
    /// `None` when nothing is masked so callers can cheaply skip logging.
    pub fn blocked_summary(&self) -> Option<Vec<String>> {
        if self.blocked_signatures.is_empty() {
            None
        } else {
            let mut v: Vec<String> = self.blocked_signatures.iter().cloned().collect();
            v.sort();
            Some(v)
        }
    }
}

// Re-export the canonical default thresholds (defined in `crate::nudge`) so
// external callers that referenced the old `WINDOW` / `THRESHOLD` /
// `ESCALATE_AT` / `PATH_THRESHOLD` constants by name keep compiling. The
// detector itself reads from `NudgeConfig` fields, not these constants —
// they remain only as the documented baseline a fresh config seeds.
pub use crate::nudge::{
    DEFAULT_ESCALATE_AT as ESCALATE_AT, DEFAULT_PATH_THRESHOLD as PATH_THRESHOLD,
    DEFAULT_THRESHOLD as THRESHOLD, DEFAULT_WINDOW as WINDOW,
};
// Re-export the signature helpers and nudge text builders so the detector
// and its callers reach them through `loop_guard::` unchanged. Their
// implementations now live in `crate::nudge`.
pub use crate::nudge::{
    build_block_nudge, build_nudge, build_path_block_nudge, build_path_nudge, humanize,
    path_signature, read_signature,
};

use neenee_core::NudgeConfig;

/// How a completed tool round looks to a guard. Computed by the caller (which
/// owns tool-access classification) and fed to `ReadLoopGuard::observe_round`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RoundClass {
    /// An all-read round, reduced to its canonical signature (see
    /// [`read_signature`]). A repeating signature is what the guard watches for.
    Read(String),
    /// Anything that is not a pure read round — a write, an execute, a mixed
    /// round, or no tool call at all. Real progress, so it clears the window: a
    /// loop that resumes afterwards is a *fresh* loop, re-armed from scratch.
    #[default]
    Progress,
}

/// Per-turn read-loop detector. Cheap value type; one lives in `TurnState` and is
/// dropped when the turn ends, so loop state never leaks across turns.
///
/// Tracks two axes of repetition:
/// - **Exact signature** (`name|path|offset|limit`): catches `A A A` and the
///   two-page thrash `A B A B A`. Genuine forward paging (`A B C D E`) never
///   trips because each page is a distinct signature.
/// - **Path bucket** (`name|path`): catches the similar-parameter escape where
///   a model reads the *same file* at many different offsets (`1`, `50`, `100`,
///   `150`, …) without ever leaving that file. Each exact signature differs, so
///   the exact-signature axis misses it; the path-bucket axis catches it.
///
/// Tunable thresholds (`window`, `threshold`, `escalate_at`, `path_threshold`)
/// come from [`NudgeConfig`]; the canonical defaults live in [`crate::nudge`].
/// Whether the guard is *armed* (allowed to inject) is a separate
/// `Agent`-level switch outside this struct.
pub struct ReadLoopGuard {
    /// Tunable thresholds and the master enable switch. The detector reads
    /// `window` / `threshold` / `escalate_at` / `path_threshold` from here on
    /// every observation; `enabled` is **not** consulted here (the
    /// `Agent::apply_guard_actions` site gates the whole registry on the
    /// live config's `enabled` flag, so a disabled guard is a no-op without
    /// each guard having to re-check).
    config: NudgeConfig,
    /// Exact signatures of the last `config.window` read rounds, oldest at the
    /// front.
    window: VecDeque<String>,
    /// Per-signature latch: how many nudges this signature has already drawn in
    /// its current streak. Cleared for a signature once it ages out of the
    /// window (its streak is over), so a later recurrence can nudge again.
    nudges_sent: HashMap<String, u8>,
    /// Path-bucket signatures (`name|path`) of the last `config.window` read
    /// rounds. Catches the similar-parameter escape: re-reading the same file
    /// at different offsets. A distinct file each time never trips.
    path_window: VecDeque<String>,
    /// Per-path-bucket latch, same semantics as [`nudges_sent`](Self::nudges_sent).
    path_nudges_sent: HashMap<String, u8>,
}

impl ReadLoopGuard {
    /// Construct a guard tuned by `config`. The thresholds are read live from
    /// the config on every observation, so a runtime update via
    /// `Agent::set_nudge_config` takes effect on the next turn (per-turn
    /// state is rebuilt fresh each turn).
    pub fn new(config: NudgeConfig) -> Self {
        Self {
            config,
            window: VecDeque::new(),
            nudges_sent: HashMap::new(),
            path_window: VecDeque::new(),
            path_nudges_sent: HashMap::new(),
        }
    }

    /// The configured thresholds. Exposed so the `/config` modal can render
    /// the live values and tests can assert against them.
    pub fn config(&self) -> NudgeConfig {
        self.config
    }

    /// Record one completed tool round and return a nudge prompt iff this round
    /// pushed a read signature to (or past) the loop threshold and the latch
    /// admits firing. `None` the rest of the time — including every non-read
    /// round, which also resets the window.
    ///
    /// This is the legacy entry point (kept for tests). It reports only the
    /// *inject* level: an escalation to [`GuardAction::Block`] is represented
    /// here as `None` (the level-2 nudge text is not a plain string the legacy
    /// caller consumes). The production path is the [`RoundGuard`] trait via
    /// `observe_round`, which returns the full [`GuardAction`].
    pub fn observe(&mut self, round: RoundClass) -> Option<String> {
        match round {
            RoundClass::Progress => {
                self.reset();
                None
            }
            RoundClass::Read(signature) => match self.observe_exact(signature)? {
                GuardAction::Inject(nudge) => Some(nudge),
                // Block/Abort carry structured payloads the legacy string API
                // cannot represent; tests that need them use the trait path.
                _ => None,
            },
        }
    }

    /// Observe a round via the trait's [`GuardRound`] context. This is the
    /// primary entry point from the agent loop — it computes both the exact
    /// signature and the path bucket, then checks both axes.
    ///
    /// The two axes are **mutually exclusive per round** to avoid double-nudging
    /// an identical read, and this exclusion extends to window bookkeeping: a
    /// round that the exact-signature axis handles (fires or simply counts
    /// toward its window) is *not* also recorded against the path-bucket axis.
    /// This is deliberate — without it, the path axis would build up a parallel
    /// count for a file the exact axis already covers and fire a redundant,
    /// late nudge. The path axis therefore only ever sees rounds whose exact
    /// signature was inert, which is precisely the "same file, many different
    /// offsets" escape it exists to catch (every exact signature is distinct
    /// there, so the exact axis never claims the round).
    fn observe_round(&mut self, round: GuardRound<'_>) -> GuardAction {
        if !round.all_read {
            self.reset();
            return GuardAction::Continue;
        }
        let exact = read_signature(round.calls.iter().copied());

        // Exact axis claims the round: advance its window and, if it fires,
        // return its action without touching the path axis.
        let exact_action = self.observe_exact(exact.clone());
        if let Some(action) = exact_action {
            return action;
        }
        // The exact axis did not *fire* this round, but it may still *own* this
        // signature: once a nudge (or block) has been latched for it, the exact
        // axis is tracking the loop and the path axis must not also record
        // these rounds — otherwise the path bucket builds a parallel count and
        // fires a redundant, late nudge while the exact axis sits in its
        // post-nudge wait window (between THRESHOLD and ESCALATE_AT). So skip
        // the path axis whenever the exact signature is already latched.
        if self.nudges_sent.contains_key(&exact) {
            return GuardAction::Continue;
        }
        // Exact axis was inert and unowned this round. Offer it to the
        // path-bucket axis (which advances the path window and may fire). This
        // is the "same file, many different offsets" escape: every exact
        // signature is distinct so the exact axis never latches one, and the
        // collapsed path signature is what repeats. Guarded by `exact != path`
        // so a path-less/query-shaped read isn't double-counted against itself.
        let path = path_signature(round.calls.iter().copied());
        if exact != path
            && let Some(action) = self.observe_path(path)
        {
            return action;
        }
        GuardAction::Continue
    }

    /// Exact-signature axis: push the signature to the window and return an
    /// action if it reaches threshold. Called by [`observe_round`](Self::observe_round)
    /// (which owns the round→axis routing) and by the legacy `observe()`.
    fn observe_exact(&mut self, signature: String) -> Option<GuardAction> {
        self.push(signature.clone());
        self.check_exact(&signature)
    }

    /// Path-bucket axis: push the path signature to the path window and return
    /// an action if the same file is read at many different offsets. Called by
    /// [`observe_round`](Self::observe_round) and the legacy `observe()`.
    fn observe_path(&mut self, path_sig: String) -> Option<GuardAction> {
        self.push_path(path_sig.clone());
        self.check_path(&path_sig)
    }

    /// Judge the exact signature (already in the window) for threshold. Returns
    /// the escalation action: a level-1 [`GuardAction::Inject`] nudge at
    /// `config.threshold`, escalating to a level-2 [`GuardAction::Block`] at
    /// `config.escalate_at` when the loop persists past the nudge — masking
    /// the read so it physically cannot recur.
    fn check_exact(&mut self, signature: &str) -> Option<GuardAction> {
        let count = self.count(signature);
        if count < self.config.threshold {
            return None;
        }

        let already_sent = self.nudges_sent.get(signature).copied().unwrap_or(0);
        let level = match already_sent {
            0 => 1,
            1 if count >= self.config.escalate_at => 2,
            _ => return None,
        };
        self.nudges_sent.insert(signature.to_string(), level);
        if level == 1 {
            Some(GuardAction::Inject(build_nudge(signature, count, level)))
        } else {
            let message = build_block_nudge(&humanize(signature), count);
            Some(GuardAction::Block {
                signatures: vec![signature.to_string()],
                message,
            })
        }
    }

    /// Judge the path signature (already in the path window) for threshold.
    /// Same escalation ladder as [`check_exact`](Self::check_exact): nudge at
    /// `config.path_threshold`, hard-block the path bucket at
    /// `config.escalate_at`.
    fn check_path(&mut self, path_sig: &str) -> Option<GuardAction> {
        let count = self.count_path(path_sig);
        if count < self.config.path_threshold {
            return None;
        }

        let already_sent = self.path_nudges_sent.get(path_sig).copied().unwrap_or(0);
        let level = match already_sent {
            0 => 1,
            1 if count >= self.config.escalate_at => 2,
            _ => return None,
        };
        self.path_nudges_sent.insert(path_sig.to_string(), level);
        if level == 1 {
            Some(GuardAction::Inject(build_path_nudge(
                path_sig, count, level,
            )))
        } else {
            let message = build_path_block_nudge(&humanize(path_sig), count);
            Some(GuardAction::Block {
                signatures: vec![path_sig.to_string()],
                message,
            })
        }
    }

    fn push(&mut self, signature: String) {
        self.window.push_back(signature);
        while self.window.len() > self.config.window {
            #[allow(clippy::expect_used)]
            // only popped while len > window, so non-empty by construction
            let evicted = self.window.pop_front().expect("non-empty");
            if !self.window.contains(&evicted) {
                self.nudges_sent.remove(&evicted);
            }
        }
    }

    fn push_path(&mut self, signature: String) {
        self.path_window.push_back(signature);
        while self.path_window.len() > self.config.window {
            #[allow(clippy::expect_used)]
            // only popped while len > window, so non-empty by construction
            let evicted = self.path_window.pop_front().expect("non-empty");
            if !self.path_window.contains(&evicted) {
                self.path_nudges_sent.remove(&evicted);
            }
        }
    }

    fn count(&self, signature: &str) -> u32 {
        self.window.iter().filter(|s| *s == signature).count() as u32
    }

    fn count_path(&self, signature: &str) -> u32 {
        self.path_window.iter().filter(|s| *s == signature).count() as u32
    }

    fn reset(&mut self) {
        self.window.clear();
        self.nudges_sent.clear();
        self.path_window.clear();
        self.path_nudges_sent.clear();
    }
}

impl RoundGuard for ReadLoopGuard {
    fn observe(&mut self, round: GuardRound<'_>) -> GuardAction {
        self.observe_round(round)
    }

    fn reset(&mut self) {
        ReadLoopGuard::reset(self);
    }
}

// The signature helpers (`read_signature`, `path_signature`,
// `call_signature`, `path_call_signature`, `has_query_arg`) and the nudge
// text builders (`build_nudge`, `build_path_nudge`, `build_block_nudge`,
// `build_path_block_nudge`, `humanize`) now live in [`crate::nudge`]. They
// are re-exported at the top of this module so callers reaching them through
// `loop_guard::` keep compiling.

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &str, offset: u64, limit: u64) -> RoundClass {
        let args = format!(r#"{{"path":"{path}","offset":{offset},"limit":{limit}}}"#);
        RoundClass::Read(read_signature([("read_text", args.as_str())]))
    }

    #[test]
    fn identical_reads_trip_at_threshold_and_only_once() {
        let mut guard = ReadLoopGuard::new(NudgeConfig::default());
        assert!(guard.observe(read("a.rs", 1, 50)).is_none()); // 1st
        assert!(guard.observe(read("a.rs", 1, 50)).is_none()); // 2nd
        let nudge = guard.observe(read("a.rs", 1, 50)).expect("3rd fires");
        assert!(nudge.contains("read_text a.rs"));
        // Same signature again: latched, no repeat nudge until escalation.
        assert!(guard.observe(read("a.rs", 1, 50)).is_none());
        assert!(guard.observe(read("a.rs", 1, 50)).is_none());
    }

    #[test]
    fn oscillation_between_two_pages_is_caught() {
        // A B A B A — no signature is ever consecutive, but A reaches 3 in the
        // window. A consecutive counter would miss this; the frequency window
        // does not.
        let mut guard = ReadLoopGuard::new(NudgeConfig::default());
        assert!(guard.observe(read("big.rs", 1, 100)).is_none()); // A
        assert!(guard.observe(read("big.rs", 900, 100)).is_none()); // B
        assert!(guard.observe(read("big.rs", 1, 100)).is_none()); // A (2)
        assert!(guard.observe(read("big.rs", 900, 100)).is_none()); // B (2)
        let nudge = guard.observe(read("big.rs", 1, 100)).expect("A hits 3");
        assert!(nudge.contains("big.rs"));
    }

    #[test]
    fn legitimate_paging_never_fires() {
        // Forward paging reads the same file but distinct ranges every time, so
        // no signature repeats.
        let mut guard = ReadLoopGuard::new(NudgeConfig::default());
        for page in 0..6 {
            let offset = 1 + page * 100;
            assert!(
                guard.observe(read("big.rs", offset, 100)).is_none(),
                "page at offset {offset} must not nudge"
            );
        }
    }

    #[test]
    fn a_progress_round_resets_the_window() {
        let mut guard = ReadLoopGuard::new(NudgeConfig::default());
        guard.observe(read("a.rs", 1, 50));
        guard.observe(read("a.rs", 1, 50));
        // A write/execute round breaks the loop: the count restarts.
        assert!(guard.observe(RoundClass::Progress).is_none());
        assert!(guard.observe(read("a.rs", 1, 50)).is_none()); // 1st of a fresh streak
        assert!(guard.observe(read("a.rs", 1, 50)).is_none()); // 2nd
        assert!(guard.observe(read("a.rs", 1, 50)).is_some()); // 3rd fires again
    }

    #[test]
    fn nudge_then_hard_block_on_persistent_loop() {
        // The escalation ladder the production path exercises: a level-1
        // `Inject` nudge at count THRESHOLD (3), then a level-2 `Block` at
        // ESCALATE_AT (6) when the loop persists, then silence. We drive it
        // through the `RoundGuard` trait — the real dispatch path — because the
        // legacy `observe()` string API cannot represent a `Block`.
        let mut guard = ReadLoopGuard::new(NudgeConfig::default());
        let args =
            |offset, limit| format!(r#"{{"path":"a.rs","offset":{offset},"limit":{limit}}}"#);
        let mut actions = Vec::new();
        for _ in 0..10 {
            let args = args(1, 50);
            let call = ("read_text", args.as_str());
            actions.push(trait_observe(&mut guard, guard_round(&[call])));
        }
        // Exactly two non-Continue actions across ten identical reads.
        let fired: Vec<_> = actions
            .iter()
            .filter(|a| !matches!(a, GuardAction::Continue))
            .collect();
        assert_eq!(fired.len(), 2, "fires exactly twice (nudge + block)");
        // First is the soft nudge.
        match &fired[0] {
            GuardAction::Inject(msg) => assert!(
                msg.contains("read_text a.rs"),
                "level-1 nudge names the read"
            ),
            other => panic!("first action should be Inject, got {other:?}"),
        }
        // Second is the hard block, carrying the exact signature.
        match &fired[1] {
            GuardAction::Block {
                signatures,
                message,
            } => {
                assert!(
                    signatures.iter().any(|s| s.contains("a.rs")),
                    "block carries the a.rs signature: {signatures:?}"
                );
                assert!(
                    message.contains("blocked"),
                    "block message announces the hard block"
                );
            }
            other => panic!("second action should be Block, got {other:?}"),
        }
    }

    #[test]
    fn distinct_grep_queries_on_one_file_are_not_collapsed() {
        // Same path, different patterns -> different signatures (query-shaped
        // args fall back to raw), so a real search is never mistaken for a loop.
        let mut guard = ReadLoopGuard::new(NudgeConfig::default());
        let sig = |pat: &str| {
            RoundClass::Read(read_signature([(
                "grep",
                Box::leak(format!(r#"{{"pattern":"{pat}","path":"a.rs"}}"#).into_boxed_str())
                    as &str,
            )]))
        };
        assert!(guard.observe(sig("foo")).is_none());
        assert!(guard.observe(sig("bar")).is_none());
        assert!(guard.observe(sig("baz")).is_none());
    }

    #[test]
    fn pagination_default_toggling_does_not_dodge_the_guard() {
        // "read a.rs", "read a.rs offset=1", "read a.rs offset=1 limit=0" all
        // mean the same read; they must share a signature.
        let mut guard = ReadLoopGuard::new(NudgeConfig::default());
        let bare = RoundClass::Read(read_signature([("read_text", r#"{"path":"a.rs"}"#)]));
        let with_offset = RoundClass::Read(read_signature([(
            "read_text",
            r#"{"path":"a.rs","offset":1}"#,
        )]));
        let full = read("a.rs", 1, 0);
        assert!(guard.observe(bare).is_none());
        assert!(guard.observe(with_offset).is_none());
        assert!(guard.observe(full).is_some(), "all three are the same read");
    }

    // ── path-bucket axis tests (via the RoundGuard trait) ───────────────────
    // These exercise the new path-bucket detector that catches the
    // similar-parameter escape: re-reading the same file at many different
    // offsets, where each exact signature differs so the exact-signature axis
    // never trips.

    fn guard_round<'a>(calls: &'a [(&'a str, &'a str)]) -> GuardRound<'a> {
        GuardRound {
            calls,
            all_read: true,
        }
    }

    /// Drive a `ReadLoopGuard` through the `RoundGuard` trait — the production
    /// path. This is what the agent loop does via `GuardRegistry`.
    fn trait_observe(guard: &mut ReadLoopGuard, round: GuardRound<'_>) -> GuardAction {
        RoundGuard::observe(guard, round)
    }

    #[test]
    fn path_bucket_catches_same_file_many_offsets() {
        // The case this fixes: read a.rs at many distinct offsets — each is a
        // distinct exact signature so the exact axis never trips, but the
        // path-bucket axis collapses them all to "read_text|a.rs" and trips at
        // PATH_THRESHOLD (8). We use 8 offsets (filling the window); the first
        // 7 are inert and the 8th trips.
        let mut guard = ReadLoopGuard::new(NudgeConfig::default());
        let offsets = [1, 50, 100, 150, 200, 250, 300, 350];
        for (i, &offset) in offsets.iter().enumerate() {
            let args = format!(r#"{{"path":"a.rs","offset":{offset},"limit":50}}"#);
            let call = ("read_text", args.as_str());
            let action = trait_observe(&mut guard, guard_round(&[call]));
            if i < offsets.len() - 1 {
                assert_eq!(
                    action,
                    GuardAction::Continue,
                    "offset {offset} (read #{}) should not trip",
                    i + 1
                );
            } else {
                match action {
                    GuardAction::Inject(msg) => assert!(
                        msg.contains("a.rs") && msg.contains("same file"),
                        "path nudge should name the file and mention the issue, got: {msg}"
                    ),
                    other => panic!("offset {offset} should trip path axis, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn path_bucket_does_not_fire_on_distinct_files() {
        // Reading several different files is legitimate exploration — each
        // lands in its own path bucket, so the path axis never accumulates a
        // repeat. Must not trip regardless of how many files are read.
        let mut guard = ReadLoopGuard::new(NudgeConfig::default());
        for file in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"] {
            let args = format!(r#"{{"path":"{file}","offset":1,"limit":50}}"#);
            let call = ("read_text", args.as_str());
            assert_eq!(
                trait_observe(&mut guard, guard_round(&[call])),
                GuardAction::Continue,
                "reading {file} should not trip"
            );
        }
    }

    #[test]
    fn progress_round_resets_both_axes() {
        // A non-read round (all_read=false) clears both the exact-signature and
        // path-bucket windows.
        let mut guard = ReadLoopGuard::new(NudgeConfig::default());
        for offset in [1, 50, 100] {
            let args = format!(r#"{{"path":"a.rs","offset":{offset},"limit":50}}"#);
            let call = ("read_text", args.as_str());
            trait_observe(&mut guard, guard_round(&[call]));
        }
        // Progress round: a write.
        let progress = GuardRound {
            calls: &[],
            all_read: false,
        };
        trait_observe(&mut guard, progress);
        // After reset, the same file can be read a few times without tripping.
        for offset in [1, 50] {
            let args = format!(r#"{{"path":"a.rs","offset":{offset},"limit":50}}"#);
            let call = ("read_text", args.as_str());
            assert_eq!(
                trait_observe(&mut guard, guard_round(&[call])),
                GuardAction::Continue,
                "after progress reset, reads should not trip"
            );
        }
    }

    #[test]
    fn guard_registry_merges_actions() {
        // Two guards that both fire: the registry merges their Inject actions.
        struct AlwaysNudge {
            label: &'static str,
            fired: bool,
        }
        impl RoundGuard for AlwaysNudge {
            fn observe(&mut self, _round: GuardRound<'_>) -> GuardAction {
                if self.fired {
                    GuardAction::Continue
                } else {
                    self.fired = true;
                    GuardAction::Inject(format!("nudge-{}", self.label))
                }
            }
        }
        let mut registry = GuardRegistry::new();
        registry.register(Box::new(AlwaysNudge {
            label: "a",
            fired: false,
        }));
        registry.register(Box::new(AlwaysNudge {
            label: "b",
            fired: false,
        }));
        let action = registry.observe(GuardRound {
            calls: &[],
            all_read: true,
        });
        match action {
            GuardAction::Inject(msg) => {
                assert!(msg.contains("nudge-a"), "merged nudge missing a: {msg}");
                assert!(msg.contains("nudge-b"), "merged nudge missing b: {msg}");
            }
            other => panic!("expected Inject, got {other:?}"),
        }
    }

    // ── GuardAction::Block merge semantics ──────────────────────────────────

    #[test]
    fn block_is_more_severe_than_inject() {
        // A Block (hard mask) outranks an Inject (soft nudge) of the same round.
        let merged = GuardAction::Block {
            signatures: vec!["read_text|a.rs".into()],
            message: "blocked".into(),
        }
        .merge(GuardAction::Inject("nudge".into()));
        assert!(
            matches!(merged, GuardAction::Block { .. }),
            "Block should win over Inject, got {merged:?}"
        );
        // And the block absorbs the inject's message into its own.
        if let GuardAction::Block { message, .. } = merged {
            assert!(message.contains("blocked"), "block message kept: {message}");
            assert!(message.contains("nudge"), "inject folded in: {message}");
        }
    }

    #[test]
    fn two_blocks_merge_their_signature_sets() {
        let merged = GuardAction::Block {
            signatures: vec!["read_text|a.rs".into()],
            message: "first".into(),
        }
        .merge(GuardAction::Block {
            signatures: vec!["read_text|b.rs".into()],
            message: "second".into(),
        });
        match merged {
            GuardAction::Block {
                signatures,
                message,
            } => {
                assert_eq!(signatures.len(), 2, "signature sets union: {signatures:?}");
                assert!(signatures.contains(&"read_text|a.rs".to_string()));
                assert!(signatures.contains(&"read_text|b.rs".to_string()));
                assert!(message.contains("first") && message.contains("second"));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn abort_outranks_block() {
        let merged = GuardAction::Abort("boom".into()).merge(GuardAction::Block {
            signatures: vec!["x".into()],
            message: "blocked".into(),
        });
        assert!(
            matches!(merged, GuardAction::Abort(_)),
            "Abort should win over Block, got {merged:?}"
        );
    }

    // ── RoundGuardState::is_blocked surgical precision ──────────────────────
    // These test the mask the dispatch layer consults. The whole point of the
    // signature-level (not tool-level) block is precision: it must catch the
    // exact looping read while leaving everything else runnable.

    #[test]
    fn blocked_mask_catches_exact_repeat_after_block_action() {
        // Drive the guard to a Block, then feed the action through
        // RoundGuardState (as the agent loop does) and assert the mask catches
        // the same read but spares other reads.
        let mut state = RoundGuardState::new({
            let mut reg = GuardRegistry::new();
            reg.register(Box::new(ReadLoopGuard::new(NudgeConfig::default())));
            reg
        });
        let args =
            |offset, limit| format!(r#"{{"path":"a.rs","offset":{offset},"limit":{limit}}}"#);
        // Six identical reads → nudge at 3, Block at 6.
        let mut last_action = GuardAction::Continue;
        for _ in 0..6 {
            let call = ("read_text", args(1, 50));
            state.set_round(vec![(call.0.into(), call.1)], true);
            last_action = state.take_action();
        }
        assert!(
            matches!(last_action, GuardAction::Block { .. }),
            "6th read should escalate to Block, got {last_action:?}"
        );
        // The mask (populated by take_action) now blocks the identical read.
        assert!(
            state.is_blocked("read_text", &args(1, 50)),
            "the exact repeated read should be blocked"
        );
    }

    #[test]
    fn blocked_mask_spares_different_offset_of_same_file_for_exact_block() {
        // When the exact axis blocks `a.rs@1,50`, a read of `a.rs` at a
        // *different* offset is NOT caught — that is a different signature and
        // may be legitimate (the model pivoting to a new line range is exactly
        // what we want to encourage). The path-bucket block is the one that
        // covers all offsets; the exact block does not.
        let mut state = RoundGuardState::new({
            let mut reg = GuardRegistry::new();
            reg.register(Box::new(ReadLoopGuard::new(NudgeConfig::default())));
            reg
        });
        let args =
            |offset, limit| format!(r#"{{"path":"a.rs","offset":{offset},"limit":{limit}}}"#);
        for _ in 0..6 {
            let call = ("read_text", args(1, 50));
            state.set_round(vec![(call.0.into(), call.1)], true);
            let _ = state.take_action();
        }
        // Same file, different offset — not blocked (exact block is surgical).
        assert!(
            !state.is_blocked("read_text", &args(200, 50)),
            "a different offset of the same file should NOT be blocked by an exact-axis block"
        );
    }

    #[test]
    fn blocked_mask_spares_different_file_entirely() {
        // A block on `a.rs` must not touch reads of `b.rs`.
        let mut state = RoundGuardState::new({
            let mut reg = GuardRegistry::new();
            reg.register(Box::new(ReadLoopGuard::new(NudgeConfig::default())));
            reg
        });
        let args = |path, offset, limit| {
            format!(r#"{{"path":"{path}","offset":{offset},"limit":{limit}}}"#)
        };
        for _ in 0..6 {
            let call = ("read_text", args("a.rs", 1, 50));
            state.set_round(vec![(call.0.into(), call.1)], true);
            let _ = state.take_action();
        }
        assert!(
            !state.is_blocked("read_text", &args("b.rs", 1, 50)),
            "a different file should never be blocked by an a.rs block"
        );
    }

    #[test]
    fn blocked_mask_spares_different_tool() {
        // A block on `read_text` must not touch `grep` — the mask is keyed on
        // the full signature, which includes the tool name.
        let mut state = RoundGuardState::new({
            let mut reg = GuardRegistry::new();
            reg.register(Box::new(ReadLoopGuard::new(NudgeConfig::default())));
            reg
        });
        let read_args = r#"{"path":"a.rs","offset":1,"limit":50}"#;
        for _ in 0..6 {
            state.set_round(vec![("read_text".into(), read_args.into())], true);
            let _ = state.take_action();
        }
        assert!(
            !state.is_blocked("grep", r#"{"pattern":"foo","path":"a.rs"}"#),
            "grep should never be blocked by a read_text block"
        );
    }

    #[test]
    fn path_bucket_block_catches_all_offsets_of_that_file() {
        // When the *path-bucket* axis blocks a file (the many-offset escape), a
        // read of that file at ANY offset is caught — unlike the exact-axis
        // block above. We trigger it by reading one file at many distinct
        // offsets until the path axis escalates to a Block.
        let mut state = RoundGuardState::new({
            let mut reg = GuardRegistry::new();
            reg.register(Box::new(ReadLoopGuard::new(NudgeConfig::default())));
            reg
        });
        let args = |offset| format!(r#"{{"path":"a.rs","offset":{offset},"limit":50}}"#);
        // Push past PATH_THRESHOLD (8) then past ESCALATE_AT (6 more) to
        // escalate. Each round is a distinct exact signature (different
        // offset), so the exact axis never latches; the path axis accumulates
        // `read_text|a.rs`.
        let offsets = [1, 50, 100, 150, 200, 250, 300, 400, 500, 600];
        let mut saw_block = false;
        for off in offsets {
            let call = ("read_text", args(off));
            state.set_round(vec![(call.0.into(), call.1)], true);
            if let GuardAction::Block { .. } = state.take_action() {
                saw_block = true;
            }
        }
        assert!(
            saw_block,
            "should escalate to a path-bucket Block at some point"
        );
        // Any offset of a.rs is now blocked.
        assert!(
            state.is_blocked("read_text", &args(999)),
            "a path-bucket block should catch any offset of the file"
        );
        // But a different file is still fine.
        assert!(
            !state.is_blocked("read_text", r#"{"path":"b.rs","offset":1,"limit":50}"#),
            "path-bucket block on a.rs should spare b.rs"
        );
    }
}

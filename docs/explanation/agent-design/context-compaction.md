# Context compaction

Compaction is the second context-relief layer (pruning →
[compaction](context-compaction.md) → overflow recovery). Where
[pruning](context-pruning.md) clears stale tool *bodies* cheaply and silently,
compaction reclaims **conversation-level** space: it summarizes older complete
turns into a durable checkpoint, archives the originals, and replaces them in
the model-visible window. It is lossy at the dialogue level and comparatively
expensive (it usually calls the model), so it runs rarely but deeply — and,
unlike pruning, it **surfaces a transcript notice**.

This page is the design reference for compaction. For the cheaper layer that
runs first, see [Context pruning](context-pruning.md); for where both sit in a
turn, see [Harness architecture](harness.md#context-relief).

## Thresholds are model-relative

Every relief threshold is a fraction of the **active model's context window**,
measured in tokens and re-seeded whenever the provider switches (ADR-0019). The
declarative `CompactionPolicy` (`neenee-core/src/pressure.rs`) carries the
fractions; `CompactionPolicy::resolve(window_tokens)` turns them into a concrete
`ContextBudget`:

| Fraction | Default | Resolved threshold | Drives |
|----------|---------|--------------------|--------|
| `prune_utilization` | 0.65 | `prune_threshold_tokens` | [Pruning](context-pruning.md) |
| `utilization` | 0.85 | `compaction_threshold_tokens` | **Compaction trigger** |
| `target_utilization` | 0.25 | `target_tokens` | **Post-compaction size** |

So on a 1M-token model compaction fires near 850k tokens and compresses the
active window back toward ~250k — a deep cut that buys many rounds before the
next one. When the window is unknown (local/unregistered models) a conservative
`fallback_window_tokens` (32_000) substitutes so relief still engages.

## How pressure is measured

Thresholds are tokens; the live transcript must be measured in the same unit to
compare against them. `effective_pressure_tokens`
(`neenee-core/src/pressure.rs`) is the single policy for that, shared by both
relief layers:

- A provider-reported `prompt_tokens` is *ground truth* for what the model
  actually saw, so it is preferred when present **and plausible** — at least
  half of the independent chars/4 estimate. Relays and local servers sometimes
  report `0` or absurdly small usage; trusting that would under-count pressure
  and risk overflow, so an implausible report is treated as missing.
- Otherwise the cheap `estimate_tokens` proxy (chars ÷ 4) is used.

The bias is deliberately conservative: under-counting risks overflow (the
expensive failure), while over-counting only prunes a little early. Today every
call site passes `reported = None` — the `Provider` trait does not yet surface
usage, and threading it through the streaming adapters is a separate epic
(ADR-0019). Centralising the policy here means wiring real usage later is a
one-line change at each call site, not a logic rewrite.

## What compaction does

The entry point is `compact_turn_history`
(`neenee-agent/src/orchestration.rs`), which delegates the heavy lifting to
`run_compaction` (`neenee-store/src/session.rs`). The boundary is the **start of
an older complete user turn** — never mid-turn — so the model-visible history
always begins coherently.

1. **Select.** `CompactionSelection` splits the history into an archived *head*
   (older complete turns), a verbatim *tail* (the most recent
   `compaction_preserve_turns` turns, default 6, kept provider-native), and the
   *previous summary* extracted from any prior checkpoint.
2. **Summarize.** By default (`compaction_summarize = true`) the active model
   writes an anchored, structured summary of the head. The previous summary is
   carried forward, so each compaction **updates** the running summary rather
   than restarting it. If the model call fails, a deterministic newest-first
   excerpt summary (capped per message by `EXCERPT_CAP`) is the fallback — the
   system never depends on the happy path.
3. **Reassemble.** The summary becomes a checkpoint message prefixed with
   `CHECKPOINT_HEADER` (`"[Conversation checkpoint] …"`). That header doubles as
   a classifier: it excludes the checkpoint from the user-turn count and lets
   the next compaction find and extend the prior summary. System messages are
   **regenerated** on the next turn rather than archived into model context.

The result is a `ContextReliefResult` (active = checkpoint + tail; archived =
the original head), committed durably exactly like a prune.

## Hooks and veto

`run_compaction` takes a `CompactionHooks` implementation. `pre_compact` returns
a `CompactionDecision` that can **veto** a compaction (or inject context) before
it happens; `post_compact` observes the committed checkpoint. The interactive
runner supplies `RelayCompactionHooks`, which is also what turns a committed
compaction into the user-facing `AgentResponse::Compacted` event via
`send_compaction`. These names stay in the `Compaction*` family on purpose:
after ADR-0021, only the summarizing layer emits `Compacted`, so the
vocabulary is accurate (pruning uses the neutral `ContextRelief*` mechanism).

## Durability and the transcript notice

Compaction commits through `SessionStore::commit_context_relief` — the same
archive-and-replace mechanism pruning uses — appending a
`SessionEvent::ContextReliefCommitted` event. The complete pre-compaction
transcript is recoverable from the durable session even though the model now
sees only the checkpoint plus the recent tail.

Unlike pruning, compaction is **visible**: it emits `AgentResponse::Compacted`,
which the TUI renders as `Compacted N messages: X -> Y chars.`. A user seeing
that notice knows a real summarization happened.

## Manual and reactive compaction

- **Manual.** `/compact` (`BuiltinCmd::Compact`) runs the exact same operation
  on demand, independent of the threshold.
- **Reactive overflow recovery.** If a provider reports context overflow
  *before* any `ToolCall` event, the runner may compact and retry the same
  logical turn once (`compacted_after_overflow`). Overflow *after* tool activity
  is terminal, so tool side effects are never replayed.

## Configuration

| Key | Default | Effect |
|-----|---------|--------|
| `[compaction].utilization` | 0.85 | Window fraction that triggers compaction. |
| `[compaction].target_utilization` | 0.25 | Active-window size compaction compresses toward. |
| `compaction_preserve_turns` | 6 | Most recent turns kept verbatim after the checkpoint. |
| `compaction_summarize` | `true` | Use the model for an anchored summary; `false` (or any failure) uses the deterministic excerpt fallback. |

## References

- ADR-0009 — uncapped agentic loop; compaction as the runaway backstop.
- ADR-0019 — model-relative thresholds derived from the active window.
- ADR-0021 — the `ContextRelief*` rename; only compaction keeps the
  `Compaction`/`Compacted` vocabulary and the transcript notice.
- [Context pruning](context-pruning.md) — the cheaper layer that runs first.
- `neenee-store/src/session.rs` — `run_compaction`, `CompactionSelection`,
  `CHECKPOINT_HEADER`, `CompactionHooks`.

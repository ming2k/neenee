# 0046. Reasoning is opt-in, controlled per model

- **Status:** Accepted
- **Date:** 2026-07-10

## Context

Extended thinking (the `thinking` on/off switch) and reasoning effort (the
`output_config.effort` depth throttle) are genuinely orthogonal on the Anthropic
Messages API — ADR-0045 already established that they are *per-model* properties
(keyed in `[model_reasoning."<model-id>"]`), not per-provider.

But two things were still wrong:

1. **Reasoning defaulted ON for every model that declares `effort_levels`.** A
   first-party Claude model shipped a `thinking: {type:"adaptive"}` object on
   every request with no user opt-in — `ThinkingConfig::for_model` flipped the
   switch on for any model with a non-empty capability set. A user who just
   wanted to chat paid the latency and token cost of extended thinking whether
   they asked for it or not. The capability to reason ("this model *can* think")
   was conflated with the decision to reason ("this model *should* think now").

2. **Reasoning was also still settable at the provider level.** The stage-1
   provider key editor exposed effort/thinking rows for the `anthropic` provider,
   the custom-provider create/edit form carried `Effort`/`Thinking` fields, and
   `SwitchProvider`/`AddProvider`/`EditProvider` carried `effort`/`thinking`
   parameters persisted to the flat `anthropic_effort`/`anthropic_thinking`
   config keys. This contradicted ADR-0045's "per-model" claim and gave the user
   two overlapping surfaces (provider-level and model-level) for the same knobs,
   with confusing precedence.

The user's intent, stated directly: move the effort/thinking controls out of the
provider editor entirely, into the per-model `e` editor; default to **no
thinking**; only when the user configures a model does it think, at the chosen
effort. ("不写默认没 think，写的默认有 think 且为对应 effort".)

## Decision

1. **Reasoning is opt-in.** `ThinkingConfig::for_model` now always returns
   *thinking off, no explicit effort* — regardless of the model's
   `effort_levels`. No model reasons on its own. The capability set (which
   levels a model honors *once thinking is on*) stays model-derived; *whether*
   it thinks is the user's choice.

2. **The per-model `[model_reasoning]` entry is the single opt-in surface.** An
   entry's mere presence opts the model in: thinking defaults **on** (the
   recommended Claude mode) unless the entry explicitly sets `thinking = false`,
   and a set `effort` applies at that depth (else the model default, with
   `output_config` omitted to stay lean). A model not listed never reasons. This
   is the "写的默认有 think 且为对应 effort" contract — applied uniformly to
   built-in models (via the table) and custom Anthropic-relay channels (via the
   channel's `effort`/`thinking` fields, presence = opted in).

3. **Reasoning is removed from the provider level.** The stage-1 provider key
   editor no longer shows effort/thinking rows; the custom-provider create/edit
   form no longer has `Effort`/`Thinking` fields; `SwitchProvider`,
   `AddProvider`, and `EditProvider` no longer carry `effort`/`thinking`. A
   provider is created/authed at the provider level and reasoned with (or not)
   per model from the stage-2 model `e` editor.

4. **The model list shows a model's effort only when it is actually opted in.**
   A model with thinking on shows `◆ think on · <effort>`; an unconfigured model
   — the common case — shows nothing, keeping the list quiet and making opted-in
   models stand out.

## Alternatives considered

- **Collapse effort and thinking into one selector** (`off → low → … → max`).
  Rejected: it would lose the wire-level orthogonality (effort without thinking
  is a legitimate, useful state) and force a coupled model. Kept as two
  independent knobs, just moved to model-level and defaulted off.

- **Keep the provider-level knobs as a fallback default.** Rejected: two
   overlapping surfaces with implicit precedence is exactly the confusion this
   ADR removes. One surface, one precedence (per-model entry wins; absence =
   off).

- **Drop the now-deprecated `anthropic_effort`/`anthropic_thinking` config keys
  entirely.** Rejected for now: removing them would break existing `config.toml`
  files on load. They are kept (and still deserialize) but **no longer read** by
  the catalog; the doc marks them deprecated with a migration pointer to
  `[model_reasoning]`. They can be removed in a future release once migration has
  had time to land.

## Consequences

- **Positive:** one mental model — "a model thinks only if you told it to, and
  you tell it per model". Lower default latency/token cost for users who don't
  want thinking. A quieter model list where configured models are visually
  distinct.

- **Negative / migration:** existing users who relied on the provider-wide
  `anthropic_effort`/`anthropic_thinking` (or on Claude thinking by default) now
  get thinking **off** until they add a `[model_reasoning."<model-id>"]` entry
  (or set the channel's effort/thinking). This is an intentional behavior
  change; the deprecated keys still load without error but no longer take effect.

- **Neutral:** `EditProviderModel` and `EditModelReasoning` (per-model) are
  unchanged — they were already model-scoped. The provider-level request
  variants are narrower.

## References

- [ADR-0045](0045-extract-neenee-tui-view.md) — established effort/thinking as
  per-model properties; this ADR completes that move by making them the *only*
  surface and flipping the default to opt-in.
- `crates/neenee-providers/src/anthropic_compat.rs` — `ThinkingConfig::for_model`
  and the opt-in default.
- `crates/neenee-agent/src/catalog.rs` — `apply_per_model_reasoning`
  (presence-opts-in) and `user_channel_to_channel`.
- `docs/reference/configuration.md` — the `[model_reasoning]` table reference.

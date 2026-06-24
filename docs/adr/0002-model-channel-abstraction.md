# 0002. Model/channel abstraction and picker redesign

- **Status:** Accepted
- **Date:** 2026-06-19

> **Update (2026-06-24):** Implemented. The `Transport` / `Channel` /
> `ProviderEntry` types and the catalog construction path shipped in
> `neenee-core::catalog`. Built-in presets materialize a single `"default"`
> channel; the multi-channel-per-model capability is live for **user-defined**
> entries (a config entry may declare several channels with `default_channel`
> selecting one — see `neenee-agent::catalog` tests). Status promoted from
> Proposed accordingly.

## Context

Model selection today spreads one logical concept across five overlapping
structures, and cannot express "the same model reached through several
delivery paths":

1. **Conflated terminology.** `Provider` (trait), `default_provider`
   (`crates/neenee/src/config.rs:15`), `OpenAiProviderSpec`
   (`crates/neenee-core/src/providers.rs:1112`), and `ModelSolution`
   (`crates/neenee-tui/src/lib.rs:338`) all describe the same
   endpoint-plus-model-plus-key tuple. None of them separate the model from
   the channel that delivers it.
2. **No one-to-many model → channel.** A user who wants Gemini through
   Google AI Studio versus Vertex AI, or any model through a self-hosted
   OpenAI-compatible relay, has no way to express that one model has several
   delivery paths. Every preset hard-codes exactly one endpoint. The catalog
   has no way to say "this model has three delivery paths, switch between
   them." `OPENAI_PROVIDER_SPECS`
   (`crates/neenee-core/src/providers.rs:1132`) hard-codes one endpoint per
   id.
3. **Flat, unranked picker.** The `SOLUTIONS` list
   (`crates/neenee-tui/src/lib.rs:360`) is static and alphabetical. There
   is no fuzzy filter, no favorite pinning, no recency ordering. Selecting
   a preset walks through as many as three sequential modals
   (`Modal::Endpoint`, `Modal::ApiKey`, `Modal::ModelName` in the `Modal`
   enum at `crates/neenee-tui/src/lib.rs:436`), each replacing the input line.
4. **No "default" fast path.** `default_provider` exists in config, but
   `/models` always opens the full list. There is no "press Enter to take
   the default and dismiss" path, and no in-picker way to change the
   default or mark a favorite.
5. **Persistence is single-bucket.** The recent-model signal (which would
   drive recency sort) has nowhere to live. Today every model-related
   field is crammed into `config.toml`, which is correct for user
   preferences but wrong for program-generated usage telemetry.

## Decision

Collapse the five overlapping structures into a two-layer **model /
channel** abstraction, split persistence by XDG intent, and redesign the
`/models` picker around fuzzy search, favorites, recency, and an inline
channel editor.

### 1. Two-layer data model

- **Model** — a logical LLM identified by a stable canonical id (for
  example `gemini:2.0-flash`), carrying only capability metadata
  (context window, reasoning, native tools). A Model owns one or more
  Channels.
- **Channel** — one delivery path for a Model. A Channel pairs a
  `Transport` (OpenAI-compatible, Gemini-native, Vertex AI, Llama server)
  with endpoint configuration (base URL, user agent, API-key source). The
  `Provider` trait implementation is chosen by `Transport`; channels are
  data, not one struct per vendor.
- **Catalog** — the union of built-in models (read-only, replacing both
   `SOLUTIONS` and `OPENAI_PROVIDER_SPECS`) and user-defined models
   (read-write). Catalog lookup replaces `openai_provider_spec`
   (`crates/neenee-core/src/providers.rs:1190`) and the bespoke dispatch
   that used to live in `make_provider` in `main.rs` (phase 1 moved it to
   `Channel::build` in `crates/neenee-core/src/catalog.rs` and
   `build_provider_for` in `crates/neenee/src/catalog.rs`).

A user-facing **ModelEntry** adds the per-user state that is *not*
intrinsic to the model: the preferred (default) channel index, a
`favorite` flag, and the model id override. Built-in entries are
overlay-merged with user entries so user customizations (a new channel,
an overridden default) survive catalog updates without forking the
built-in data.

### 2. Persistence split by XDG intent

The "would a user hand-edit this file?" test splits storage in two:

| Data | Location | Rationale |
|------|----------|-----------|
| User-defined models, channel choice, `favorite` flag, default-model pointer, model id override, API keys | `$XDG_CONFIG_HOME/neenee/` (`config.toml`, or a dedicated `models.toml` table) | User preference: hand-editable, shareable, version-controllable. Same intent as today's `default_provider`. |
| `last_used` timestamps driving recency sort | `$XDG_STATE_HOME/neenee/model_usage.json`, next to `history.json` (`crates/neenee/src/paths.rs:141`) | Program-generated usage telemetry. Loss affects ordering only, never configuration. Same intent as slash-command history. |

API keys keep their current resolution order (env var beats config beats
inline) and stay out of `data` and `state`. Two new accessors are added
to `Dirs`: `model_usage_file()` (state) and, if models move to a separate
file, `models_file()` (config). The four XDG roots are already plumbed by
`paths.rs`.

### 3. Picker redesign

`/models` opens a single fuzzy picker:

- **Default fast path.** When a default model is configured, pressing
  `Enter` on an unfiltered picker activates the default and dismisses the
  modal in one keystroke.
- **Fuzzy filter.** The top of the modal is a search box; filtering reuses
  the fzf-style `fuzzy_match` in `crates/neenee-tui/src/fuzzy.rs` and
  highlights matched positions.
- **Sort order.** Favorites first, then `last_used` descending, then
  alphabetical. The ordering is materialized from the merged Catalog plus
  the state file, never stored as a list.
- **Row content.** Model name, active channel label, default indicator,
  favorite indicator, and the existing key-ready signal from
  `provider_key_status` (`crates/neenee/src/main.rs:924`).
- **Inline channel switching.** `Tab` cycles the active channel for the
  highlighted model (for example Google AI Studio → Vertex AI for Gemini,
  or a self-hosted relay for an OpenAI-compatible model). The selected
  channel is written back to config as the model's default channel.
- **In-picker actions.** `f` toggles favorite, `d` sets default, `e`
  opens the editor, `Enter` activates.
- **Unified editor.** `e` opens one modal with channel tabs (`Tab` /
  `Shift-Tab`), replacing the three sequential modals `ApiKey` /
  `Endpoint` / `ModelName`. Each tab exposes that channel's editable
  fields (base URL, API key, user agent, model id override, default
  channel). The three legacy modal variants are deleted.

Writeback discipline: activating a model updates only the state file's
`last_used`. Changing favorite, default, channel selection, or any editor
field updates config. Both paths reuse the existing
`fsutil::atomic_write_*` helpers.

## Alternatives considered

- **Keep the flat list; add favorites and recency only.** Rejected: the
  one-model-one-endpoint assumption is the root cause of the Gemini
  Studio/Vertex gap. Layering ranking on top of the current `SOLUTIONS`
  list leaves the channel question unanswered, with no way to attach a
  second delivery path to an existing model.
- **One Channel per ModelEntry, duplicates for each path.** Rejected:
  forces the user to re-enter the model id, API key, and display name
  three times to express one Gemini setup, and breaks the "switch
  delivery path with `Tab`" interaction. It also fragments favorite and
  recency signals across what is conceptually one model.
- **Persist everything in `data`.** Rejected: a user-defined Vertex
  channel is a hand-editable preference, not program state. Putting it in
  `$XDG_DATA_HOME` makes configurations non-portable across machines and
  loses them on data-directory cleanup, contradicting the intent of
  `config.toml`.
- **Persist everything in `config`.** Rejected: `last_used` timestamps
  are program-generated telemetry that changes on every activation.
  Writing them to `config.toml` churns a user-edited file, creates
  merge conflicts for users who version-control their config, and
  conflicts with the established precedent that `history.json` lives in
  state.
- **Keep the three sequential modals; add a channel dropdown.** Rejected:
  the sequential modal chain is itself a usability regression (each step
  borrows the input line and hides the model list). A tabbed editor
  subsumes all three modal variants with less code.

## Consequences

- Positive: one concept per layer (Model, Channel, Catalog, ModelEntry);
  closes the Studio/Vertex gap for any model, not just Gemini, and lets a
  user-defined OpenAI-compatible relay attach to an existing model instead
  of living as a separate entry; shrinks the TUI by deleting three modal
  variants and the `SOLUTIONS` ↔ `OPENAI_PROVIDER_SPECS` duplication;
  makes the picker fast for the common case (default + `Enter`); gives
  fuzzy search, favorites, and recency for free.
- Negative: a breaking change to the on-disk config schema. The scattered
  per-provider fields (`openai_api_key`, `gemini_model`, `llama_base_url`,
  …) migrate into `[[models]]` entries with channels. A one-time
  migration reads the legacy fields, writes the new layout, and leaves
  the legacy fields ignored. `default_provider` is replaced by a
  default-model pointer keyed by canonical model id.
- Neutral: built-in catalog entries become immutable data; user
  customization overlays them. Preset ids are unique; there is no alias
  mapping layer.
- Migration is sequenced so each step is independently shippable and
  behavior-preserving: (1) introduce `ModelEntry` / `Channel` / `Catalog`
  types and a loader that materializes them from the *existing* config
  fields, with startup and the `SwitchProvider` handler
  (`crates/neenee/src/main.rs:1244`) reading from the Catalog; (2) add
  the state file and `last_used` writeback on activation; (3) ship the
  new picker (fuzzy filter, sort order, default fast path, `f` / `d` /
  `Tab`); (4) ship the tabbed editor and delete `Modal::ApiKey` /
  `Modal::Endpoint` / `Modal::ModelName`; (5) the on-disk schema.

  Step (5) landed as an **additive `[[models]]` overlay** rather than a
  destructive migration: a new `[[models]]` table (with `[[models.channels]]`)
  lets users define multi-channel models that the catalog loader merges on top
  of the built-ins (override by id, or append), and a `default_model` pointer
  is preferred over the legacy `default_provider`. The scattered per-provider
  fields are retained as the built-in source so existing configs and the
  concurrent provider-set refactors keep working untouched. A later step can
  retire the scattered fields once the provider set stabilizes; until then
  `[[models]]` is the opt-in path for multi-channel and per-model
  customization. The phase-3/4 picker and editor are single-channel in their
  UI surface; surfacing multi-channel channel tabs in the editor is the
  natural follow-on now that the data layer exists.

## References

- [Providers](../reference/providers.md) — current provider catalog and
  dispatch sites; the "Provider catalog" table is reshaped into Channel
  rows.
- [How to add a provider](../how-to/add-a-provider.md) — the `id` field
  becomes a Model id; a new Channel entry replaces the single-endpoint
  assumption.
- `crates/neenee/src/paths.rs` — the four XDG roots the new accessors
  hang off.
- [ADR 0001](0001-tool-rendering-redesign.md) — precedent for a
  pillar-numbered decision with a sequenced migration.

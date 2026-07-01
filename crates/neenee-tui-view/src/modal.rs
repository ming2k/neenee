//! Modal identity and surface-recess policy.
//!
//! [`Modal`] is a fieldless discriminant naming *which* overlay is open; it is
//! the seam shared between the view layer (modal geometry via
//! [`render::primitives::modal_area`](crate::render), per-modal renderers) and
//! the app shell (which tracks the active modal as state). [`Recess`] is the
//! single source of truth for how the live surface recedes behind a modal.

#[derive(PartialEq, Clone, Copy, Debug, Default)]
pub enum Modal {
    #[default]
    None,
    /// Two-stage provider/model picker (`Ctrl+P` / `/model`). **Stage 1** is a
    /// ranked *provider* list (`App::providers_filtered`); **stage 2**
    /// (`App::picker_provider` = `Some`) is the model sub-list for a
    /// drilled-into multi-model provider (`App::provider_models_filtered`).
    /// Enter on a multi-model provider drills into stage 2; on a single-model
    /// provider (or any stage-2 row) it activates that (provider, model). Each
    /// stage mirrors the input-history modal's two-mode design: it opens in
    /// **browse** mode (composer line not borrowed, typing inert) and `/` drops
    /// into a **search** sub-layer that borrows the line as a live fuzzy query
    /// (`App::model_search` distinguishes the two). Esc in search returns to
    /// browse; Esc in stage 2 returns to stage 1; Esc in stage 1 (or an outside
    /// click) closes and restores the draft.
    Provider,
    /// Input-history recall (Ctrl+R). A two-mode surface: it opens in **browse**
    /// mode — a plain reverse-chronological list (newest first, top-focused)
    /// where the composer line is not borrowed and typing is inert — and `/`
    /// drops into a **search** sub-layer that borrows the line as a live fuzzy
    /// query (`App::history_search` distinguishes the two). The name is kept for
    /// continuity even though browsing, not searching, is now the default.
    /// Rows come from `App::history_rows`; Enter inserts the focused entry into
    /// the composer for editing (never sends). The first Esc in search returns to
    /// browse, the second (or an outside click) closes and restores the draft.
    HistorySearch,
    Permission,
    Question,
    /// Unified provider editor: edit the API key and model-id
    /// of a catalog entry in one place. Reached via `e` in the picker or
    /// `Enter` on a no-key model. Replaces the sequential ApiKey / Endpoint /
    /// ModelName modal chain.
    ModelEditor,
    /// Provider-template chooser: a short list of curated templates (Custom
    /// Anthropic relay / OpenAI-compatible / Gemini) shown when adding a provider.
    /// Reached from the "＋ Add provider" row at the bottom of the picker's stage-1
    /// list. `↑/↓` move; `Enter` opens the [`Self::CustomProvider`] editor seeded
    /// from the chosen template; `Esc` returns to the picker. See
    /// `App::template_choice` and [`crate::providers::PROVIDER_TEMPLATES`].
    ProviderTemplate,
    /// Provider editor: a per-template form (Name, Base URL, Token, and — for the
    /// OpenAI-compatible template — Model) for defining a user provider without
    /// editing config.toml by hand. The protocol and seeded models come from the
    /// template chosen in [`Self::ProviderTemplate`]; `Tab`/`BackTab` cycle the
    /// visible fields, and the focused field borrows the composer line (like
    /// [`Self::ModelEditor`]). `Enter` saves (→ `AgentRequest::AddProvider`) and
    /// activates; `Esc` returns to the picker. See `App::custom_field` and
    /// friends.
    CustomProvider,
    /// Add-model overlay for a custom provider: pick a model from the provider's
    /// protocol candidates (cycled with `←/→`) or the synthetic "Custom…" slot
    /// (free-text id in the borrowed input line). `Enter` sends
    /// `AgentRequest::AddProviderModel`; `Esc` returns to the stage-2 model list.
    /// Reached from the "＋ Add model" row in a custom provider's stage-2 list.
    AddModel,
    Help,
    Sessions,
    /// Tools manager modal: a centered, dismissable, selectable list of every
    /// session tool — builtins, `mcp:<server>`, `pursuit`, `plan` — each with a
    /// `Space` toggle to enable/disable it. Opened with the `/tools` slash
    /// command. `App::modal_index` is its selection cursor; data comes from
    /// the session-context snapshot.
    Tools,
    /// MCP manager modal: a centered, dismissable, selectable list of every
    /// configured MCP server with its connection status (connected / disabled /
    /// failed) and tool count. Opened with the `/mcp` slash command. `Space`
    /// toggles a server on/off for the session (connect/disconnect, applied
    /// live without rewriting config.toml); `r` reconnects the selected server.
    /// `App::modal_index` is its selection cursor; data comes from the
    /// session-context snapshot (its `mcp` pane).
    Mcp,
    /// Skills modal: a centered, dismissable, selectable list of every loaded
    /// skill, each with a short hint and its enabled state. Opened with the
    /// `/skills` slash command (intercepted locally, never sent to the
    /// backend). `Enter` toggles a per-row detail expansion (full description,
    /// version, source, tags) tracked in `App::skills_expanded`; `r` reloads
    /// the skill registry by sending `/skills reload` to the backend.
    /// `App::modal_index` is its selection cursor; data comes from the
    /// session-context snapshot (its `skills` pane).
    Skills,
    /// Permissions manager modal: a centered, dismissable overlay listing the
    /// session's cached "always allow" rules with per-row revoke and a
    /// clear-all action. Opened with the `/permissions` slash command. This
    /// is the management surface — distinct from [`Modal::Permission`] (the
    /// inline real-time approval sheet).
    Permissions,
    /// Config manager modal: a centered, dismissable overlay listing the
    /// configurable categories (Nudge, …). Opened with the `/config` slash
    /// command (intercepted locally, never sent to the backend). `Enter` /
    /// `Space` drills into a category's sub-page ([`Modal::ConfigNudge`]);
    /// `Esc` closes.
    Config,
    /// Nudge sub-page of the config manager. Reached from [`Modal::Config`]
    /// by selecting the "Nudge" row. Shows the master `enabled` switch and
    /// the four tunable thresholds (`window`, `threshold`, `escalate_at`,
    /// `path_threshold`). `Space` toggles the enabled flag; `←`/`→` adjust
    /// the selected threshold; `Esc` returns to the config root. Edits are
    /// sent as `AgentRequest::UpdateNudgeConfig` and the harness replies with
    /// `AgentResponse::NudgeConfigUpdated`, which re-seeds the snapshot.
    ConfigNudge,
    /// Transcript layout sub-page of the config manager. Reached from
    /// [`Modal::Config`] by selecting the "Layout" row. Lists the layout
    /// strategies (Compact / Round-band); `Space` or `Enter` applies the
    /// selected strategy, which is sent as `AgentRequest::UpdateTuiLayout`
    /// and persisted to `config.toml`. The harness replies with
    /// `AgentResponse::TuiLayoutUpdated`, which re-seeds
    /// `App::transcript_layout`. `Esc` returns to the config root.
    ConfigLayout,
    /// Activity overview: the current pursuit (objective + checklist), the live
    /// plan-progress breakdown, and the running turn/round/model/elapsed/
    /// status. Opened by clicking the activity bar. The body scrolls via
    /// `App::activity_scroll`.
    Activity,
    /// Token-source report: a read-only breakdown of how many tokens each
    /// provider+model reported authoritatively (upstream `usage`) vs. how many
    /// were filled in by the local char-class estimator. Opened by clicking
    /// the context meter in the hint bar. Esc / outside-click closes.
    TokenReport,
    /// Interactive-input injection panel (L3.5 β): shown when a `bash` command
    /// is classified interactive and the agent cannot supply its own input.
    /// Borrows the composer input line (like `Provider`/`ModelEditor`) for
    /// free-text entry; masks the typed text when the request is `secret`
    /// (password/passphrase). `Enter` submits (→ `AgentRequest::InputReply`),
    /// `Esc` cancels (→ empty reply → command runs with closed stdin and fails
    /// fast with a non-interactive remedy hint).
    InputInjection,
}

/// How the live surface recedes while a modal owns the foreground.
///
/// A terminal cannot alpha-blend, so a modal expresses "the background has
/// receded" in one of three ways instead of painting a translucent veil. This
/// is the single source of truth that both the footer-collapse decision
/// (`App`/event loop) and the per-frame recess pass (`render::recess_backdrop`)
/// consult, so layout and paint can never disagree about what a modal does to
/// the surface beneath it.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Recess {
    /// The modal floats on the fully-live surface. No dimming, no occlusion —
    /// used by lightweight overlays that never take over (Question, Permission).
    None,
    /// The surface stays mounted and is darkened in place so the centered modal
    /// reads as the focal layer while context (transcript, input, hint bar,
    /// activity bar) remains visible. The brightness factor comes from
    /// [`Theme::modal_dim_factor`](crate::render::Theme::modal_dim_factor).
    Dim,
    /// Full takeover: the footer collapses to zero height and the surface is
    /// occluded with a solid fill. Reserved for context-switching flows
    /// (session selection) where a clean slate is the intent.
    Takeover,
}

impl Modal {
    /// The recess policy for this modal — the single source of truth that the
    /// footer-collapse flag and the per-frame recess pass both key off.
    pub fn recess(self) -> Recess {
        match self {
            // Float: lightweight overlays that never touch the surface.
            Modal::None | Modal::Question | Modal::Permission => Recess::None,
            // Context switch: the one modal that fully owns the screen.
            Modal::Sessions => Recess::Takeover,
            // Everything else recedes the surface for focus while keeping it
            // visible (transcript, chrome, and all).
            _ => Recess::Dim,
        }
    }

    /// Whether this modal closes when the user clicks outside its rect
    /// (click-outside-to-dismiss). True for the read-only / info overlays
    /// (Help, Session, Sessions, Activity) and for the history
    /// modal and the model picker: their filter query is ephemeral and the real
    /// composer draft is safely parked in `stashed_input`, so an outside click
    /// closes them and restores the draft (via `App::restore_history_draft`) —
    /// exactly like Esc. Entry modals that hold precious in-progress input
    /// (ModelEditor, Question) and the permission sheet stay open so an
    /// accidental click never discards an API key or a pending decision.
    ///
    /// This is the single source of truth for *which* modals are
    /// click-dismissable; the event loop records the renderer's actual panel
    /// rect for these modals and leaves every other modal without an
    /// outside-click target.
    pub fn dismissable_by_outside_click(self) -> bool {
        matches!(
            self,
            Modal::Help
                | Modal::Tools
                | Modal::Mcp
                | Modal::Skills
                | Modal::Sessions
                | Modal::Permissions
                | Modal::Config
                | Modal::ConfigNudge
                | Modal::ConfigLayout
                | Modal::Activity
                | Modal::HistorySearch
                | Modal::Provider
                | Modal::TokenReport
        )
    }

    /// Whether this modal renders its own text caret (and thus owns the
    /// terminal cursor while active) — the modals that borrow the composer
    /// input line as a free-text field. Read-only / info overlays (Help,
    /// Session, Activity, …) and the decision sheets (Question, Permission)
    /// do not own a caret; while they are open the terminal cursor is hidden
    /// so the host IME has no stale anchor to bind to. This is the modal half
    /// of `App::caret_owner`; the composer half is decided there from
    /// `focused_target` / `in_envoy_view`.
    pub fn owns_caret(self) -> bool {
        matches!(
            self,
            Modal::Provider
                | Modal::ModelEditor
                | Modal::AddModel
                | Modal::CustomProvider
                | Modal::HistorySearch
        )
    }
}

/// Which section the Activity modal is showing. Each section is opened
/// independently by clicking the corresponding segment on the activity bar,
/// so there is no tab strip or Left/Right cycling — the variant simply
/// controls which content the modal body renders.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum ActivityTab {
    Activity,
    Todos,
}

impl ActivityTab {
    /// Modal title shown in the header.
    pub fn title(self) -> &'static str {
        match self {
            ActivityTab::Activity => "Activity",
            ActivityTab::Todos => "Todos",
        }
    }
}

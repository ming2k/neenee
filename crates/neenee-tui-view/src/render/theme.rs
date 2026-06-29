//! Color palette used across the renderer.

use neenee_tui::Color;

/// Styles used during rendering.
pub struct Theme {
    pub user_fg: Color,
    pub error_fg: Color,
    pub system_fg: Color,
    pub code_fg: Color,
    pub code_bg: Color,
    pub heading_fg: Color,
    pub quote_fg: Color,
    pub dim_fg: Color,
    pub selected_bg: Color,
    // opencode-style semantic design tokens.
    /// Base background painted across the entire terminal frame so the TUI
    /// owns every pixel rather than relying on the terminal emulator default.
    pub app_bg: Color,
    /// Primary foreground text.
    pub text: Color,
    /// Muted/secondary text.
    pub text_muted: Color,
    /// Intermediate foreground for an interactive step header that is under
    /// the pointer but not in its expanded/active state — sits between
    /// `text_muted` (idle) and `text` (expanded) so hover reads as a softer
    /// affordance than "open".
    pub text_hover: Color,
    /// Solid background for panels (modals, sheets).
    pub panel_bg: Color,
    /// Background for the live input box; brighter than `user_panel_bg` so the
    /// active prompt stands out from already-sent messages.
    pub input_bg: Color,
    /// Used for sent user messages so they read as read-only compared to the
    /// live input box.
    pub user_panel_bg: Color,
    /// Background for user messages staged in the send queue (waiting for
    /// the in-flight turn to finish). Dimmer than `user_panel_bg` so a
    /// queued message reads as more "pending" than a delivered one without
    /// losing the panel affordance.
    pub user_panel_bg_queued: Color,
    /// Slightly raised background for footer/option bars.
    pub element_bg: Color,
    /// Background for menus / suggestion popups.
    pub menu_bg: Color,
    /// Dim overlay drawn behind modals to fake alpha.
    pub backdrop: Color,
    /// Brightness multiplier (0.0–1.0) applied to every cell of the live
    /// surface while a [`Recess::Dim`](crate::modal::Recess) modal is open.
    /// The terminal cannot alpha-blend, so a dim-recess modal darkens the
    /// transcript/chrome in place by scaling each color by this factor — lower
    /// is darker. This is the single knob for how strongly an open modal
    /// recedes the background for focus.
    pub modal_dim_factor: f32,
    /// Brand / selection color.
    pub primary: Color,
    pub warning: Color,
    pub success: Color,
    pub info: Color,
    /// Diff banding. Every block-level code/text surface shares one
    /// design contract (see the disclosure module): colors flow through
    /// theme tokens rather than magic `Color::Rgb` literals, so retuning
    /// the palette in one place retunes every block. The diff block is
    /// the reference renderer — it owns the row/highlight pair — and the
    /// flat code blocks (read / bash / listing / grep / markdown) reuse
    /// the same token system via [`code_surface`](Theme::code_surface).
    /// Low-chroma row tint so added/removed blocks read at a glance.
    pub diff_add_bg: Color,
    pub diff_del_bg: Color,
    /// Brighter per-word highlight tint layered on top of the row band;
    /// the exact edited word sits on this brighter surface.
    pub diff_add_hl: Color,
    pub diff_del_hl: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user_fg: Color::Rgb(165, 177, 164),
            error_fg: Color::Rgb(190, 111, 104),
            system_fg: Color::Rgb(111, 116, 110),
            code_fg: Color::Rgb(166, 178, 163),
            code_bg: Color::Rgb(17, 19, 18),
            heading_fg: Color::Rgb(190, 194, 181),
            quote_fg: Color::Rgb(156, 145, 118),
            dim_fg: Color::Rgb(94, 99, 94),
            selected_bg: Color::Rgb(38, 48, 44),
            // Zen palette: ink-black base, charcoal surfaces, quiet sage
            // accents. Contrast comes from luminance, not saturated hue, so
            // the interface stays calm while preserving semantic cues.
            app_bg: Color::Rgb(7, 8, 8),
            text: Color::Rgb(213, 213, 205),
            text_muted: Color::Rgb(119, 125, 117),
            text_hover: Color::Rgb(175, 180, 172),
            panel_bg: Color::Rgb(14, 15, 15),
            input_bg: Color::Rgb(18, 19, 19),
            user_panel_bg: Color::Rgb(17, 22, 19),
            user_panel_bg_queued: Color::Rgb(9, 12, 11),
            element_bg: Color::Rgb(21, 23, 22),
            menu_bg: Color::Rgb(17, 19, 18),
            backdrop: Color::Rgb(3, 4, 4),
            // Halves surface luminance behind a dim-recess modal — clearly
            // recessed for focus, still readable for context.
            modal_dim_factor: 0.5,
            primary: Color::Rgb(142, 161, 145),
            warning: Color::Rgb(181, 149, 93),
            success: Color::Rgb(117, 148, 117),
            info: Color::Rgb(128, 153, 156),
            // Diff banding. Lifted from the ad-hoc literals that used to live
            // inline in `draw_diff_content`; kept here as the single source so
            // every block-level surface can share one design contract.
            diff_add_bg: Color::Rgb(18, 31, 22),
            diff_del_bg: Color::Rgb(32, 20, 20),
            diff_add_hl: Color::Rgb(42, 64, 48),
            diff_del_hl: Color::Rgb(64, 40, 40),
        }
    }
}

/// Semantic accessors (ADR-0001 P4): renderers reference intent
/// (surface / body / raised / ok / err / …) rather than the raw palette field
/// names, so the palette can be retuned in one place. The fields stay `pub`
/// for `Theme::default()` construction; new rendering code should prefer these.
impl Theme {
    // ── Surfaces (backgrounds) ──
    /// Frame background — the base everything sits on.
    pub fn surface(&self) -> Color {
        self.app_bg
    }
    /// Step body / content surface.
    pub fn body(&self) -> Color {
        self.menu_bg
    }
    /// Raised surface (header bands, footer bars).
    pub fn raised(&self) -> Color {
        self.element_bg
    }
    /// Modal / sheet surface.
    pub fn panel(&self) -> Color {
        self.panel_bg
    }
    /// Live input-box surface.
    pub fn input_surface(&self) -> Color {
        self.input_bg
    }
    /// Sent-user-message surface.
    pub fn user_surface(&self) -> Color {
        self.user_panel_bg
    }
    /// Surface for a user message staged in the send queue. Dimmer than
    /// [`Theme::user_surface`] so pending reads differently from delivered.
    pub fn user_surface_queued(&self) -> Color {
        self.user_panel_bg_queued
    }
    /// Dim overlay behind modals.
    pub fn backdrop(&self) -> Color {
        self.backdrop
    }
    /// Brightness factor (0.0–1.0) the dim-recess pass scales the live surface
    /// by. Lower is darker. See [`Theme::modal_dim_factor`](struct.Theme.html#structfield.modal_dim_factor).
    pub fn modal_dim_factor(&self) -> f32 {
        self.modal_dim_factor
    }
    /// Selection highlight background.
    pub fn selected(&self) -> Color {
        self.selected_bg
    }

    // ── Foregrounds ──
    pub fn fg(&self) -> Color {
        self.text
    }
    pub fn muted(&self) -> Color {
        self.text_muted
    }
    /// Foreground for an interactive step header while collapsed but under the
    /// pointer — an intermediate tone between `muted()` (idle) and `fg()`
    /// (expanded/active), so hover reads as a softer affordance than "open".
    pub fn hover(&self) -> Color {
        self.text_hover
    }
    pub fn dim(&self) -> Color {
        self.dim_fg
    }
    pub fn brand(&self) -> Color {
        self.primary
    }
    pub fn ok(&self) -> Color {
        self.success
    }
    pub fn warn(&self) -> Color {
        self.warning
    }
    pub fn err(&self) -> Color {
        self.error_fg
    }
    pub fn info(&self) -> Color {
        self.info
    }
    pub fn code_text(&self) -> Color {
        self.code_fg
    }
    pub fn code_surface(&self) -> Color {
        self.code_bg
    }
    /// Diff block row band — the low-chroma tint a whole added line sits on.
    /// The reference block-level renderer's colors are first-class tokens so
    /// every block-level surface shares one palette contract.
    pub fn diff_add_bg(&self) -> Color {
        self.diff_add_bg
    }
    /// Diff block row band for a whole removed line.
    pub fn diff_del_bg(&self) -> Color {
        self.diff_del_bg
    }
    /// Diff block per-word highlight on an added line (brighter than the row band).
    pub fn diff_add_hl(&self) -> Color {
        self.diff_add_hl
    }
    /// Diff block per-word highlight on a removed line (brighter than the row band).
    pub fn diff_del_hl(&self) -> Color {
        self.diff_del_hl
    }
    pub fn heading(&self) -> Color {
        self.heading_fg
    }
    pub fn quote(&self) -> Color {
        self.quote_fg
    }
    pub fn user_text(&self) -> Color {
        self.user_fg
    }
    pub fn system_text(&self) -> Color {
        self.system_fg
    }
}

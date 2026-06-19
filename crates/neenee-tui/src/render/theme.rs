//! Color palette used across the renderer.

use ratatui::style::Color;

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
    /// Solid background for panels (modals, sheets).
    pub panel_bg: Color,
    /// Background for the live input box; brighter than `user_panel_bg` so the
    /// active prompt stands out from already-sent messages.
    pub input_bg: Color,
    /// Used for sent user messages so they read as read-only compared to the
    /// live input box.
    pub user_panel_bg: Color,
    /// Slightly raised background for footer/option bars.
    pub element_bg: Color,
    /// Background for menus / suggestion popups.
    pub menu_bg: Color,
    /// Tinted band behind the user's own messages (no role label is shown).
    pub user_bg: Color,
    /// Dim overlay drawn behind modals to fake alpha.
    pub backdrop: Color,
    /// Brand / selection color.
    pub primary: Color,
    pub warning: Color,
    pub success: Color,
    pub info: Color,
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
            panel_bg: Color::Rgb(14, 15, 15),
            input_bg: Color::Rgb(18, 19, 19),
            user_panel_bg: Color::Rgb(11, 12, 12),
            element_bg: Color::Rgb(21, 23, 22),
            menu_bg: Color::Rgb(17, 19, 18),
            user_bg: Color::Rgb(18, 24, 21),
            backdrop: Color::Rgb(3, 4, 4),
            primary: Color::Rgb(142, 161, 145),
            warning: Color::Rgb(181, 149, 93),
            success: Color::Rgb(117, 148, 117),
            info: Color::Rgb(128, 153, 156),
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
    /// Card body / content surface.
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
    /// Tint behind the user's own messages.
    pub fn user_tint(&self) -> Color {
        self.user_bg
    }
    /// Dim overlay behind modals.
    pub fn backdrop(&self) -> Color {
        self.backdrop
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

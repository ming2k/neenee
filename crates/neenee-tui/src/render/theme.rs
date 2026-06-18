//! Color palette used across the renderer.

use ratatui::style::Color;

/// Styles used during rendering.
pub struct Theme {
    pub user_fg: Color,
    pub assistant_fg: Color,
    pub error_fg: Color,
    pub system_fg: Color,
    pub code_fg: Color,
    pub code_bg: Color,
    pub heading_fg: Color,
    pub quote_fg: Color,
    pub dim_fg: Color,
    pub selected_bg: Color,
    pub header_bg: Color,
    pub accent: Color,
    // opencode-style semantic design tokens.
    /// Base background painted across the entire terminal frame so the TUI
    /// owns every pixel rather than relying on the terminal emulator default.
    pub app_bg: Color,
    /// Primary foreground text.
    pub text: Color,
    /// Muted/secondary text.
    pub text_muted: Color,
    /// Solid background for panels (modals, sheets, input).
    pub panel_bg: Color,
    /// Slightly dimmer than `panel_bg`; used for sent user messages so they
    /// read as read-only compared to the live input box.
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
    pub border_subtle: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user_fg: Color::Rgb(137, 180, 250),
            assistant_fg: Color::Rgb(205, 214, 244),
            error_fg: Color::Rgb(243, 139, 168),
            system_fg: Color::Rgb(127, 132, 156),
            code_fg: Color::Rgb(148, 226, 213),
            code_bg: Color::Rgb(22, 24, 35),
            heading_fg: Color::Rgb(94, 234, 212),
            quote_fg: Color::Rgb(249, 226, 175),
            dim_fg: Color::Rgb(127, 132, 156),
            selected_bg: Color::Rgb(30, 50, 70),
            header_bg: Color::Rgb(22, 24, 35),
            accent: Color::Rgb(94, 234, 212),
            // Cool palette: cyan / teal / sky — no purple-pink.
            app_bg: Color::Rgb(15, 16, 25),
            text: Color::Rgb(205, 214, 244),
            text_muted: Color::Rgb(122, 132, 153),
            panel_bg: Color::Rgb(22, 24, 35),
            user_panel_bg: Color::Rgb(18, 20, 30),
            element_bg: Color::Rgb(33, 37, 54),
            menu_bg: Color::Rgb(27, 30, 44),
            user_bg: Color::Rgb(29, 35, 54),
            backdrop: Color::Rgb(8, 9, 14),
            primary: Color::Rgb(34, 211, 238),
            warning: Color::Rgb(250, 204, 21),
            success: Color::Rgb(74, 222, 128),
            info: Color::Rgb(125, 211, 252),
            border_subtle: Color::Rgb(45, 50, 70),
        }
    }
}

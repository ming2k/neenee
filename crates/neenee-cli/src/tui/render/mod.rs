//! Rendering engine: draws the transcript (and footer chrome) using ratatui
//! while recording semantic-to-screen layout information.

mod chrome;
mod composer;
mod design;
mod empty_state;
mod markdown_table;
mod message_body;
mod notice;
mod overlays;
mod primitives;
mod step;
mod text_layout;
mod theme;
/// Per-tool presentation registry: each tool's icon, collapsed summary,
/// optional preview, and expanded-body classification. `document.rs` and
/// `step/renderers.rs` dispatch through its `*_for` entry points instead of
/// matching on tool names (see tools/mod.rs).
pub(crate) mod tools;

#[cfg(test)]
mod snapshot_tests;

pub use chrome::draw_activity_bar;
pub use chrome::{HintBarView, draw_completion_menu, draw_hint_bar};
pub use composer::{INPUT_MSG_IDX, draw_composer};
use design::{
    COMPOSER_MAX_HEIGHT_DIVISOR, COMPOSER_MIN_HEIGHT, COMPOSER_PROMPT_PREFIX_COLS,
    COMPOSER_RIGHT_PAD_COLS, COMPOSER_VERTICAL_CHROME_ROWS, FOOTER_H_INSET, HINT_BAR_ROWS,
    MESSAGE_GAP_ROWS, REASONING_TRACE_BLOCK_GAP_ROWS, REASONING_TRACE_BODY_TOP_GAP_ROWS,
    SIDE_BANNER_ROWS, STATUS_BAR_ROWS, STEP_MIN_WIDTH, SUBAGENT_BAR_ROWS,
    TOOL_STEP_BODY_TOP_GAP_ROWS, TOOL_STEP_CHILDREN_GAP_ROWS, TRANSCRIPT_BODY_PREFIX_COLS,
    TRANSCRIPT_BODY_RIGHT_INSET, TRANSCRIPT_H_INSET,
};
/// Parse a raw logo file into clamped display lines for the empty-state hero.
/// Re-exported so the startup loader and the renderer share one clamp rule.
pub(crate) use empty_state::parse_logo;
#[cfg(test)]
use markdown_table::{build_table_render, shrink_column_widths};
use message_body::draw_message_body;
use notice::draw_notice;
pub(crate) use overlays::{
    ActivityModalView, draw_activity_modal, draw_armed_toast, draw_copy_toast, draw_help_modal,
    draw_history_modal, draw_model_editor, draw_model_picker, draw_models_modal,
    draw_permission_sheet, draw_question_modal, draw_session_modal, draw_sessions_modal,
    draw_tool_step_detail_overlay,
};
pub use primitives::recess_backdrop;
use primitives::viewport_rect;
use step::{
    StickyStep, draw_reasoning_trace, draw_side_banner, draw_sticky_summary_if_needed,
    draw_subagent_bar, draw_subagent_inline_step, draw_tool_step,
};
#[cfg(test)]
use text_layout::WrappedLine;
#[cfg(test)]
use text_layout::{
    block_selection_range, line_selection, prohibited_line_end, prohibited_line_start,
};
pub use theme::Theme;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block as RtBlock, Paragraph},
};

use crate::tui::document::TranscriptMessage;
use crate::tui::layout::{InteractiveTarget, LayoutMap};
use crate::tui::selection::SelectionState;
#[cfg(test)]
use neenee_core::{PermissionRequest, ProviderPickerSnapshot, UserQuestionRequest};
#[cfg(test)]
use std::collections::HashMap;

/// Outer centered rect of the currently-open dismissable overlay modal, so the
/// event loop can close it when the user clicks on the backdrop (i.e. outside
/// the panel) — mirroring Esc.
///
/// Only the read-only / info overlays are covered: modals that paint no
/// full-screen backdrop (Permission) or that borrow the composer input and
/// therefore need their own Esc/restore path (Provider / ModelEditor /
/// HistorySearch) return `None`, so a stray click never discards an in-progress
/// filter or API key. The percentages mirror the `centered_rect(...)` call each
/// of these modals paints in `overlays.rs`; if a modal's geometry changes
/// there, update it here too so the click-outside hit-test stays exact.
pub fn modal_outer_rect(modal: &crate::tui::app::Modal, frame: &Frame) -> Option<Rect> {
    use crate::tui::app::Modal;
    // Single source of truth for *which* modals are click-dismissable lives
    // on the `Modal` type itself; this fn only adds the geometry.
    if !modal.dismissable_by_outside_click() {
        return None;
    }
    let (percent_x, percent_y) = match modal {
        Modal::Help => (58, 70),
        Modal::ToolStepDetail => (92, 84),
        Modal::Activity => (72, 70),
        Modal::Session => (76, 70),
        Modal::Sessions => (80, 64),
        _ => return None,
    };
    Some(primitives::centered_rect(
        percent_x,
        percent_y,
        viewport_rect(frame),
    ))
}

/// Inner rect of a transcript-area region after reserving the uniform
/// [`TRANSCRIPT_H_INSET`] left+right `app_bg` gutters. Use this as the render target
/// for any solid-background band (step headers/bodies, child tool steps) so
/// the band sits inside the gutters rather than spanning edge to edge. The
/// surrounding cells keep `app_bg` from the global frame fill.
pub(super) fn transcript_band_rect(area: Rect) -> Rect {
    Rect::new(
        area.x + TRANSCRIPT_H_INSET,
        area.y,
        area.width.saturating_sub(2 * TRANSCRIPT_H_INSET).max(1),
        area.height,
    )
}

pub struct TranscriptView<'a> {
    pub messages: &'a [TranscriptMessage],
    pub scroll: u16,
    pub selection: &'a SelectionState,
    /// Transient running status shown in a thin bar above the input box.
    /// Empty / "idle" means the status bar is hidden; every other value
    /// (including "responding") keeps the bar up for the full turn lifecycle.
    pub activity: &'a str,
    /// Spinner animation phase (cycles through braille frames while active).
    pub spinner_phase: usize,
    /// The current input-box text (masked while the API-key modal is open). The
    /// transcript layout reads this so the input box can grow to fit its wrapped text.
    pub input: &'a str,
    /// Byte offset of the caret inside `input` (mirrors `App::byte_cursor`).
    /// The box grows one extra row when the caret rests past the last wrapped
    /// line (e.g. just after an inserted newline), so its height matches what
    /// [`composer::draw_composer`] actually renders.
    pub byte_cursor: usize,
    /// When true, the hint bar and input box are hidden (overlay modal open).
    pub chrome_hidden: bool,
    /// When set, the view is zoomed into a subagent task: a navigation bar is
    /// rendered and `messages` is the focused task's child stream.
    pub subagent_bar: Option<SubagentBarInfo>,
    /// When set, the view is inside a `/btw` side conversation (ADR-0017): a
    /// top banner is rendered reading `Side from main · <status> · Esc back`.
    /// Carries the coarse primary-session status to surface.
    pub side_banner: Option<neenee_core::ParentStatus>,
    /// Active pursuit, if any. Surfaced on the activity bar as a `⟴ <objective>`
    /// badge so the user can tell at a glance the turn is part of a larger goal.
    pub pursuit: Option<&'a neenee_core::Pursuit>,
    /// Live unified task list, if any. Surfaced on the activity bar as
    /// `plan d/t`. The full per-item breakdown lives in the Activity modal.
    pub todos: Option<&'a neenee_core::TodoList>,
    /// Session-review alert (ADR-0016), or empty when inactive. While
    /// non-empty the activity bar appends a `⚠ <alert> — Esc to interrupt`
    /// segment.
    pub review_alert: String,
    /// Wall-clock instant the current turn started, or `None` between turns.
    /// Drives the muted `<elapsed>` segment in the activity bar.
    pub turn_started_at: Option<std::time::Instant>,
    /// Message index of the step (tool step or reasoning trace) whose header
    /// currently rests under the mouse pointer (inline or sticky pinned), so
    /// the next draw lights it up to the intermediate hover tone as a click
    /// affordance. `None` whenever the pointer is elsewhere or an overlay
    /// modal is open.
    pub hovered_step: Option<usize>,
    /// Keyboard-focused activatable target. Part of the in-progress focus-
    /// navigation feature (see `UiState::has_focused_target`); the render side
    /// is ready but no caller sets it yet, hence the targeted allow.
    #[allow(dead_code)]
    pub focused_target: Option<InteractiveTarget>,
    /// User-supplied ASCII logo lines (from `$XDG_CONFIG_HOME/neenee/logo.txt`)
    /// that replace the built-in wordmark on the empty-state hero. `None` when
    /// no user logo is configured; the hero falls back to the built-in art.
    /// Ignored entirely when the transcript is non-empty.
    pub logo: Option<&'a [String]>,
    pub theme: &'a Theme,
}

/// Info for the subagent navigation bar (shown when zoomed into a task).
pub struct SubagentBarInfo {
    /// Label for the focused subagent (its task description).
    pub label: String,
    /// 1-based index of the focused subagent among its siblings.
    pub index: usize,
    /// Total number of sibling subagent tasks.
    pub total: usize,
}

/// Layout information returned by [`draw_transcript`].
pub struct TranscriptRender {
    /// The input box area.
    pub input_rect: Rect,
    /// The hint-bar area pinned below the input box (zero-sized when hidden).
    pub hint_rect: Rect,
    /// Screen rect of the activity bar for the current frame, so clicks inside
    /// it open the Activity modal. `None` when no activity bar is shown (idle,
    /// streaming, subagent view, or chrome hidden).
    pub activity_rect: Option<Rect>,
    /// Total height (in lines) of the rendered message stream, ignoring the
    /// viewport clip. Used by the app loop to pin the view to the bottom.
    pub content_lines: usize,
    /// Height of the transcript viewport.
    pub view_height: u16,
    /// The expanded step whose body is currently scrolled into view, so the app
    /// can render/click a sticky header pinned under the HUD bar. `None` when no
    /// expanded step body covers the top of the viewport.
    pub sticky: Option<StickyInfo>,
}

/// A sticky pinned step summary (returned to the app for click handling).
pub struct StickyInfo {
    pub message_idx: usize,
    pub rect: Rect,
    /// The content-line index of the real summary inside the stream. The app
    /// uses this to re-anchor the scroll offset when the user collapses the
    /// pinned step, so the real summary takes the sticky's place at the top of
    /// the viewport instead of jumping to unrelated content.
    pub summary_line: usize,
}

/// Draw the main transcript area, recording layout info.
pub fn draw_transcript(
    frame: &mut Frame,
    layout_map: &mut LayoutMap,
    view: TranscriptView<'_>,
) -> TranscriptRender {
    let TranscriptView {
        messages,
        scroll,
        selection,
        activity,
        spinner_phase,
        input,
        byte_cursor,
        chrome_hidden,
        subagent_bar,
        side_banner,
        pursuit,
        todos,
        review_alert,
        turn_started_at,
        hovered_step,
        theme,
        ..
    } = view;
    let full = frame.area();
    // Components render inside the vertical viewport margins (1 cell top and
    // bottom); only the background fill uses the full terminal rect.
    let viewport = viewport_rect(frame);

    // Paint the entire frame with the app background so the TUI owns every
    // pixel rather than leaving gaps at the terminal emulator's default color.
    frame.render_widget(
        RtBlock::default().style(Style::default().bg(theme.surface())),
        full,
    );

    let size = viewport;

    // When zoomed into a subagent task, the footer (status bar, plan panel,
    // input box, hint bar) is hidden: the task detail page is a read-only view
    // whose only chrome is the subagent navigation bar.
    let in_subagent = subagent_bar.is_some();

    // The status bar (animated spinner + activity text) sits on its own line
    // directly above the input box. It is shown for every active phase —
    // including streaming ("responding"), which is the longest phase and the
    // one where the breathing dot's liveness signal matters most — and hidden
    // only when the harness is idle, so the row returns to the transcript.
    let status_active =
        !chrome_hidden && !in_subagent && !activity.is_empty() && activity != "idle";
    let status_height: u16 = if status_active { STATUS_BAR_ROWS } else { 0 };

    // The input box grows with its content: the typed text wraps onto new
    // lines and the box expands to fit, up to roughly half the terminal so the
    // transcript history always stays visible. The inner text width reserves the
    // footer insets, the `> ` prompt prefix, and the matching right pad so the
    // height calculation wraps at the same width the composer renders.
    let input_text_width = (size.width as usize)
        .saturating_sub(
            (2 * FOOTER_H_INSET) as usize + COMPOSER_PROMPT_PREFIX_COLS + COMPOSER_RIGHT_PAD_COLS,
        )
        .max(1);
    let input_wrapped_lines = composer::input_row_count(input, input_text_width, byte_cursor);
    let desired_input_height = input_wrapped_lines as u16 + COMPOSER_VERTICAL_CHROME_ROWS;
    let max_input_height = (size.height / COMPOSER_MAX_HEIGHT_DIVISOR).max(COMPOSER_MIN_HEIGHT);
    let input_box_height = if in_subagent {
        0
    } else {
        desired_input_height.min(max_input_height)
    };
    // The hint bar is a single-line status strip pinned directly below the
    // input box. It carries the model + context-usage info. Hidden alongside
    // the rest of the chrome while an overlay modal is open.
    let hint_height: u16 = if chrome_hidden || in_subagent {
        0
    } else {
        HINT_BAR_ROWS
    };
    let footer_height: u16 = if chrome_hidden || in_subagent {
        0
    } else {
        status_height + input_box_height + hint_height
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),                // Transcript (header was here, now removed)
            Constraint::Length(footer_height), // Status? + input box + hint bar
        ])
        .split(size);

    // 1. Transcript History
    // When zoomed into a subagent, reserve a 1-line navigation band at the
    // bottom of the transcript viewport for the subagent bar.
    let (mut transcript_area, subagent_bar_rect) = if subagent_bar.is_some() {
        let sub = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(SUBAGENT_BAR_ROWS)])
            .split(chunks[0]);
        (sub[0], Some(sub[1]))
    } else {
        (chunks[0], None)
    };
    // `/btw` side banner (ADR-0017): a 1-line band at the TOP of the
    // transcript viewport reading `Side from main · <status> · Esc back`.
    // The side view keeps the footer (composer), unlike the subagent zoom,
    // so only this top band is carved off.
    if let Some(status) = side_banner {
        let sub = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(SIDE_BANNER_ROWS), Constraint::Min(0)])
            .split(transcript_area);
        draw_side_banner(frame, sub[0], status, theme);
        transcript_area = sub[1];
    }
    let mut current_y = transcript_area.y;
    // Account for scroll
    let mut skip_rows = scroll as usize;
    // Total stream height, counted independently of the viewport clip so the
    // app loop can follow the bottom.
    let mut content_lines: usize = 0;
    // Expanded steps collected during the pass, for the sticky pinned header.
    let mut sticky_steps: Vec<StickyStep> = Vec::new();
    // The last model attribution badge drawn into the stream. A badge is shown
    // once at the start of an assistant turn and again only when the producing
    // model changes, so a session that mixes providers stays traceable without
    // repeating the label on every message of a single-model run.
    let mut last_shown_attribution: Option<(String, String)> = None;

    // Empty-state replacement (ADR-0033): when the session has no messages and
    // no subagent/side view is open, the transcript is replaced by a centered
    // logo hero rather than rendering an empty stream. This is a component
    // substitution, not transcript content — the hero never participates in
    // scroll, selection, or attribution, so the whole message-rendering
    // pipeline (loop, badges, sticky pinning) is skipped. The footer below
    // renders exactly as in a live session.
    let show_empty_state = messages.is_empty() && subagent_bar.is_none() && side_banner.is_none();

    if show_empty_state {
        empty_state::draw_empty_state(frame, transcript_area, view.logo, theme);
        // Account for the hero so the app loop does not treat the session as a
        // zero-height stream (which would mis-pin the scroll position).
        content_lines = empty_state::empty_state_content_lines(view.logo);
    } else {
        for (mi, msg) in messages.iter().enumerate() {
            // Model attribution badge: shown above the first assistant-side
            // message of a turn (reasoning, text, or tool step) and whenever the
            // producing provider/model changes. Tool results and tool steps share
            // the turn's model, so a single badge per model-run keeps the
            // transcript clean while remaining fully traceable.
            let is_assistant_side =
                msg.role == neenee_core::Role::Assistant || msg.is_thinking() || msg.is_tool_step();
            if is_assistant_side {
                if let Some(attribution) = msg.attribution_label() {
                    if last_shown_attribution.as_ref() != Some(&attribution) {
                        draw_attribution_badge(
                            frame,
                            transcript_area,
                            &attribution,
                            &mut skip_rows,
                            &mut current_y,
                            &mut content_lines,
                            theme,
                        );
                        last_shown_attribution = Some(attribution);
                    }
                }
            }

            // Render blocks
            if msg.is_notice() {
                draw_notice(
                    frame,
                    transcript_area,
                    msg,
                    &mut skip_rows,
                    &mut current_y,
                    &mut content_lines,
                    theme,
                );
            } else if msg.is_subagent_task() {
                draw_subagent_inline_step(
                    frame,
                    transcript_area,
                    msg,
                    mi,
                    theme,
                    layout_map,
                    &mut skip_rows,
                    &mut current_y,
                    &mut content_lines,
                    hovered_step == Some(mi),
                );
            } else if msg.is_tool_step() {
                draw_tool_step(
                    frame,
                    transcript_area,
                    msg,
                    mi,
                    selection,
                    theme,
                    layout_map,
                    &mut skip_rows,
                    &mut current_y,
                    &mut content_lines,
                    &mut sticky_steps,
                    hovered_step == Some(mi),
                );
            } else if msg.is_thinking() {
                draw_reasoning_trace(
                    frame,
                    transcript_area,
                    msg,
                    mi,
                    selection,
                    theme,
                    layout_map,
                    &mut skip_rows,
                    &mut current_y,
                    &mut content_lines,
                    &mut sticky_steps,
                    hovered_step == Some(mi),
                );
            } else {
                draw_message_body(
                    frame,
                    transcript_area,
                    msg,
                    mi,
                    selection,
                    theme,
                    layout_map,
                    &mut skip_rows,
                    &mut current_y,
                    &mut content_lines,
                    true,
                );
            }

            // Spacing between messages. A user message's panel already ends with a
            // bottom transition row (▀) that separates it from the next message, so
            // the extra blank line is omitted there to keep the gap to a single row
            // (otherwise the sent message sits two rows above the following body).
            // The exception is when the next message is a step (thinking or tool
            // step): a blank row between the user panel's transition and the step
            // header keeps the two visually distinct. This matches the spacing
            // produced by live reasoning streams and restored history.
            let next_is_step = messages.get(mi + 1).is_some_and(|next| {
                next.is_thinking() || next.is_tool_step() || next.is_subagent_task()
            });
            if msg.role != neenee_core::Role::User || next_is_step {
                content_lines += MESSAGE_GAP_ROWS;
                if skip_rows > 0 {
                    skip_rows = skip_rows.saturating_sub(1);
                } else if current_y < transcript_area.y + transcript_area.height {
                    current_y += MESSAGE_GAP_ROWS as u16;
                }
            }
        }
    } // end else (non-empty transcript branch)

    // Record the visible transcript content rect so clicks on gap rows
    // (which carry no registered region) still switch keyboard focus to
    // Browse. The rect spans the horizontal band inside the gutters —
    // matching the user's mental model that the outer gutters are not
    // transcript clicks — and the rows where content was actually drawn,
    // clamped to the viewport so empty space below the last message stays
    // inert. `current_y` already stops advancing once it reaches the
    // viewport bottom, so this is a faithful bound on visible content.
    // Skipped for the empty-state hero, which owns its own rect and is not
    // part of the interactive transcript surface.
    if !show_empty_state {
        let band = transcript_band_rect(transcript_area);
        let content_bottom = current_y.min(transcript_area.y + transcript_area.height);
        if content_bottom > transcript_area.y {
            layout_map.set_transcript_content_rect(Rect::new(
                band.x,
                transcript_area.y,
                band.width,
                content_bottom - transcript_area.y,
            ));
        }
    }

    // Subagent navigation band, drawn across the full transcript width (inside the
    // app_bg gutters) so it reads as a continuous bar pinned above the input.
    if let (Some(bar), Some(rect)) = (subagent_bar.as_ref(), subagent_bar_rect) {
        draw_subagent_bar(frame, rect, bar, theme);
    }

    // The footer stacks, from top to bottom: the transient activity bar (when
    // active), the input box, and the persistent hint bar. The activity bar
    // sits directly above the input so the live transcript progress reads as
    // "what is happening right now" right next to where the user types, and it
    // doubles as the click target that opens the Activity modal (the pursuit and
    // plan summaries that used to live here now scroll inside that modal and
    // as inline notices in the transcript).
    let footer_x = chunks[1].x + FOOTER_H_INSET;
    let footer_w = chunks[1].width.saturating_sub(2 * FOOTER_H_INSET);

    let status_y = chunks[1].y;

    // The transient activity bar sits directly above the input box. It stays
    // up for the entire active turn lifecycle (queued → responding → tool
    // work → finalizing), including the streaming phase, and hides only when
    // idle. Keeping it up during "responding" avoids a layout shift at the
    // stream boundary and sustains the breathing-dot liveness anchor
    // (ADR-0008) through the longest phase.
    // Returns its rect so the event loop can hit-test clicks → Activity modal.
    let activity_rect = if status_active {
        draw_activity_bar(
            frame,
            Rect::new(footer_x, status_y, footer_w, STATUS_BAR_ROWS),
            pursuit,
            todos,
            &review_alert,
            turn_started_at,
            activity,
            spinner_phase,
            theme,
        )
    } else {
        None
    };

    // The input box sits directly below the activity bar (when active), or at
    // the top of the footer otherwise.
    let input_rect = Rect::new(
        footer_x,
        status_y + status_height,
        footer_w,
        input_box_height,
    );

    // The hint bar sits directly below the input box and carries the model
    // and context-usage info. Rendered last so its rect is computed even
    // though its draw call is delegated to the app loop (which owns the
    // masked input state).
    let hint_rect = if hint_height > 0 {
        Rect::new(
            footer_x,
            status_y + status_height + input_box_height,
            footer_w,
            hint_height,
        )
    } else {
        Rect::new(0, 0, 0, 0)
    };

    // Sticky pinned summary: if an expanded step's body covers the top of the
    // viewport (its summary is scrolled out of view), pin its summary to the
    // line directly under the HUD bar so the user can always collapse it.
    let sticky_info =
        draw_sticky_summary_if_needed(frame, transcript_area, &sticky_steps, scroll, theme);

    TranscriptRender {
        input_rect,
        hint_rect,
        activity_rect,
        content_lines,
        view_height: transcript_area.height,
        sticky: sticky_info,
    }
}

/// Draw a single-line model attribution badge above an assistant turn.
///
/// The badge labels which provider/model produced the following response, so a
/// session that mixes models stays traceable. It occupies one content line
/// (scrollable like any other). It reads as a turn heading rather than body
/// content, so it sits at the transcript horizontal inset (flush with the gutter)
/// instead of the body's leading indent, and is followed by a blank row that
/// separates it from the turn's content. Rendered in muted text so it reads as
/// metadata. The provider half is dropped when empty (e.g. providers without an
/// id).
fn draw_attribution_badge(
    frame: &mut Frame,
    area: Rect,
    attribution: &(String, String),
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    theme: &Theme,
) {
    let (provider, model) = attribution;
    // `provider · model`, dropping the provider half (and separator) when the
    // provider id is empty so untagged/legacy providers show just the model.
    let label = if provider.is_empty() {
        model.clone()
    } else {
        format!("{} · {}", provider, model)
    };

    // Badge line. Counts toward scroll height even when scrolled/clipped out;
    // only drawn and advanced when the row falls inside the viewport.
    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < area.y + area.height {
        let prefix = " ".repeat(TRANSCRIPT_H_INSET as usize);
        let line = Line::from(vec![
            Span::styled(prefix, Style::default()),
            Span::styled("◆ ", Style::default().fg(theme.dim())),
            Span::styled(label, Style::default().fg(theme.muted())),
        ]);
        let rect = Rect::new(area.x, *current_y, area.width, 1);
        frame.render_widget(Paragraph::new(line), rect);
        *current_y += 1;
    }

    // Blank row separating the heading from the turn's content.
    *content_lines += MESSAGE_GAP_ROWS;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(MESSAGE_GAP_ROWS);
    } else if *current_y < area.y + area.height {
        *current_y += MESSAGE_GAP_ROWS as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::render::text_layout::wrap_text;
    use unicode_width::UnicodeWidthStr;

    /// Smoke-render every redesigned component into a buffer to catch panics
    /// (border math, rect underflows, empty content) without a live terminal.
    #[test]
    fn redesigned_components_render_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let mut thinking = TranscriptMessage::thinking("Reasoning about the task step by step.");
                thinking.set_thinking_expanded(true);
                let mut tool = TranscriptMessage::tool_step("call_1", "list_dir", r#"{"path":"."}"#);
                tool.set_tool_step_expanded(true);
                tool.finish_tool_step("call_1", "file_a\nfile_b", neenee_core::ToolOutput::text("file_a\nfile_b"), 12);
                let messages = vec![
                    TranscriptMessage::new(neenee_core::Role::User, "hi"),
                    TranscriptMessage::new(
                        neenee_core::Role::Assistant,
                        "Here is a table:\n\n| Tool | Count |\n| --- | ---: |\n| read | 1 |\n| webfetch | 250 |",
                    ),
                    thinking,
                    tool,
                ];
                let _ = draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "waiting for model",
                        spinner_phase: 0,
                        input: "hello",
                        byte_cursor: 5,
                        chrome_hidden: false,
                        subagent_bar: None,
                        side_banner: None,
                        pursuit: None,
                        todos: None,
                        review_alert: String::new(),
                        turn_started_at: None,
                        hovered_step: None,
                        focused_target: None,
                        logo: None,
                        theme: &theme,
                    },
                );
                draw_composer(
                    f,
                    Rect::new(0, 21, 80, 3),
                    "hello",
                    5,
                    true,
                    true,
                    &theme,
                    &mut LayoutMap::new(),
                    true,
                    &mut 0,
                    &SelectionState::None,
                );
                draw_completion_menu(
                    f,
                    &mut layout_map,
                    &[
                        crate::tui::Completion {
                            label: "/pursue".to_string(),
                            description: "Pursue a pursuit".to_string(),
                            replace_start: 0,
                            replace_end: 0,
                        },
                        crate::tui::Completion {
                            label: "/clear".to_string(),
                            description: "Clear".to_string(),
                            replace_start: 0,
                            replace_end: 0,
                        },
                    ],
                    Some(0),
                    Rect::new(0, 20, 80, 3),
                    &theme,
                );
                draw_copy_toast(f, "copied to clipboard", false, &theme);
                draw_armed_toast(f, "press Ctrl+C again to exit", &theme);
            })
            .unwrap();

        // Modals + permission sheet on a fresh frame.
        terminal
            .draw(|f| {
                draw_models_modal(
                    f,
                    &mut LayoutMap::new(),
                    &[],
                    "mock",
                    0,
                    &HashMap::new(),
                    &ProviderPickerSnapshot::default(),
                    "",
                    0,
                    &theme,
                );
                let history_roster: Vec<String> = vec!["a".to_string()];
                let ranked: Vec<(usize, crate::tui::fuzzy::FuzzyMatch)> =
                    crate::tui::fuzzy::rank(&history_roster, "");
                draw_history_modal(
                    f,
                    &mut LayoutMap::new(),
                    &history_roster,
                    "",
                    0,
                    &ranked,
                    0,
                    &theme,
                );
                draw_model_editor(f, "OpenAI", 0, "", "gpt-4o", "", 0, &theme);
                draw_help_modal(f, &theme);
                draw_sessions_modal(
                    f,
                    &[
                        neenee_core::SessionOverview {
                            id: "abc123".to_string(),
                            overview: "Refactor the renderer".to_string(),
                            created_at: 0,
                            updated_at: 0,
                            message_count: 12,
                            active: true,
                        },
                        neenee_core::SessionOverview {
                            id: "def456".to_string(),
                            overview: "Fix the tool_call_id bug".to_string(),
                            created_at: 0,
                            updated_at: 0,
                            message_count: 4,
                            active: false,
                        },
                    ],
                    0,
                    &theme,
                );
                let question_request = UserQuestionRequest {
                    id: "q1".to_string(),
                    questions: vec![neenee_core::UserQuestion {
                        header: Some("Style".to_string()),
                        question: "Which error handling crate?".to_string(),
                        options: vec![
                            neenee_core::UserQuestionOption {
                                label: "anyhow (Recommended)".to_string(),
                                description: Some("Simple".to_string()),
                            },
                            neenee_core::UserQuestionOption {
                                label: "thiserror".to_string(),
                                description: Some("Structured".to_string()),
                            },
                        ],
                        multi_select: false,
                    }],
                };
                draw_question_modal(
                    f,
                    &question_request,
                    0,
                    &[vec![1]],
                    &[String::new()],
                    1,
                    &theme,
                );
                // Session context modal: every pane must render without panicking
                // across (a) an unknown provider + empty snapshot, (b) a fully
                // populated snapshot exercising the Skills / Permissions / Tools
                // list panes and the MCP per-server tool names.
                let snapshot = neenee_core::SessionContextSnapshot {
                    model: neenee_core::ModelInfo {
                        provider: "gemini".to_string(),
                        model: "gemini-2.5-flash".to_string(),
                        display_name: "Gemini 2.5 Flash".to_string(),
                        context_window: 1_000_000,
                        api_key_ready: true,
                        description: "Google Gemini 2.5 Flash".to_string(),
                        capabilities: vec!["tool calling".to_string()],
                    },
                    tools: vec![neenee_core::ToolInfo {
                        name: "bash".to_string(),
                        description: "run a shell command".to_string(),
                        access: neenee_core::ToolAccess::Execute,
                        enabled: true,
                        source: "builtin".to_string(),
                    }],
                    permissions: vec![neenee_core::PermissionRuleInfo {
                        tool: "bash".to_string(),
                        scope: "*".to_string(),
                    }],
                    skills: vec![neenee_core::SkillInfo {
                        name: "rust-expert".to_string(),
                        description: "Rust help".to_string(),
                        version: Some("1.0.0".to_string()),
                        enabled: true,
                        source: "repo".to_string(),
                        tags: vec!["rust".to_string()],
                    }],
                    mcp: vec![neenee_core::McpServerInfo {
                        name: "fs".to_string(),
                        connected: true,
                        disabled: false,
                        failure: None,
                        tool_names: vec!["read_file".to_string(), "write_file".to_string()],
                    }],
                };
                let mut key_status = HashMap::new();
                key_status.insert("gemini".to_string(), true);
                for (tab, idx) in [
                    (crate::tui::SessionTab::Model, 0),
                    (crate::tui::SessionTab::Mcp, 0),
                    (crate::tui::SessionTab::Skills, 0),
                    (crate::tui::SessionTab::Permissions, 0),
                    (crate::tui::SessionTab::Tools, 0),
                ] {
                    draw_session_modal(
                        f,
                        tab,
                        "custom-unknown",
                        "some-model",
                        &key_status,
                        &[],
                        Some(&snapshot),
                        idx,
                        &mut 0,
                        &theme,
                    );
                }
                // And once with no snapshot, to cover the placeholder fallbacks.
                draw_session_modal(
                    f,
                    crate::tui::SessionTab::Model,
                    "custom-unknown",
                    "some-model",
                    &key_status,
                    &[],
                    None,
                    0,
                    &mut 0,
                    &theme,
                );
            })
            .unwrap();

        terminal
            .draw(|f| {
                let request = PermissionRequest {
                    id: "p1".to_string(),
                    tool: "bash".to_string(),
                    label: "bash".to_string(),
                    description: "run a command".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                    scope: "*".to_string(),
                };
                let rect = ratatui::layout::Rect::new(0, 0, 60, 3);
                let _ = draw_permission_sheet(f, &request, 0, false, false, 0, rect, &theme);
            })
            .unwrap();
    }

    /// Render both the compact subagent step (root view) and the zoomed-in
    /// subagent view with its navigation bar, ensuring no layout panics.
    #[test]
    fn subagent_step_and_view_render_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // Root view: a completed subagent task renders as a compact step.
        let mut task = TranscriptMessage::tool_step(
            "task_1",
            "subagent",
            r#"{"description":"explore the codebase","prompt":"..."}"#,
        );
        task.push_subagent_event(&neenee_core::SubagentEvent::ToolCall {
            id: "inner".into(),
            name: "grep".into(),
            arguments: r#"{"pattern":"foo"}"#.into(),
        });
        task.finish_tool_step(
            "task_1",
            "found 3 matches",
            neenee_core::ToolOutput::text("found 3 matches"),
            1200,
        );
        let root_messages = vec![
            TranscriptMessage::new(neenee_core::Role::User, "explore please"),
            task,
        ];

        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let _ = draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &root_messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "running subagent",
                        spinner_phase: 0,
                        input: "",
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: None,
                        side_banner: None,
                        pursuit: None,
                        todos: None,
                        review_alert: String::new(),
                        turn_started_at: None,
                        hovered_step: None,
                        focused_target: None,
                        logo: None,
                        theme: &theme,
                    },
                );
            })
            .unwrap();

        // Zoomed-in subagent view: the task's children are the message stream
        // and the navigation bar is shown.
        let children = root_messages[1].subagent_children().unwrap().to_vec();
        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let _ = draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &children,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "",
                        spinner_phase: 0,
                        input: "",
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: Some(SubagentBarInfo {
                            label: "explore the codebase".to_string(),
                            index: 1,
                            total: 1,
                        }),
                        side_banner: None,
                        pursuit: None,
                        todos: None,
                        review_alert: String::new(),
                        turn_started_at: None,
                        hovered_step: None,
                        focused_target: None,
                        logo: None,
                        theme: &theme,
                    },
                );
            })
            .unwrap();
    }

    #[test]
    fn line_selection_intersects_wrapped_lines() {
        use crate::tui::layout::SemanticCursor;
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 2),
            head: SemanticCursor::new(0, 0, 8),
        };
        let range = block_selection_range(&sel, 0, 0);

        // Line covering bytes 0..5 ("hello"): selected from 2 to end.
        let first = WrappedLine {
            text: "hello".to_string(),
            start_byte: 0,
            end_byte: 5,
        };
        assert_eq!(line_selection(range, &first), Some((2, 5)));

        // Line covering bytes 5..10 ("world"): selected up to head char (8 → rel 3, inclusive → 4).
        let second = WrappedLine {
            text: "world".to_string(),
            start_byte: 5,
            end_byte: 10,
        };
        assert_eq!(line_selection(range, &second), Some((0, 4)));

        // A line after the selection has no overlap.
        let third = WrappedLine {
            text: "after".to_string(),
            start_byte: 10,
            end_byte: 15,
        };
        assert_eq!(line_selection(range, &third), None);
    }

    #[test]
    fn block_selection_covers_middle_blocks_fully() {
        use crate::tui::layout::SemanticCursor;
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 3),
            head: SemanticCursor::new(0, 2, 1),
        };
        assert_eq!(block_selection_range(&sel, 0, 0), Some((3, None)));
        assert_eq!(block_selection_range(&sel, 0, 1), Some((0, None)));
        assert_eq!(block_selection_range(&sel, 0, 2), Some((0, Some(1))));
        assert_eq!(block_selection_range(&sel, 0, 3), None);
        assert_eq!(block_selection_range(&sel, 1, 0), None);
    }

    #[test]
    fn test_wrap_text() {
        let lines = wrap_text("hello world", 5);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[1].text, " worl");
        assert_eq!(lines[2].text, "d");
    }

    #[test]
    fn test_wrap_with_newlines() {
        let lines = wrap_text("hi\nthere", 10);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "hi");
        assert_eq!(lines[1].text, "there");
    }

    #[test]
    fn wrap_avoids_cjk_punctuation_at_line_start() {
        let lines = wrap_text("人生需要坚持，才能前进。", 12);
        assert!(lines.len() > 1);
        assert!(lines.iter().skip(1).all(|line| {
            line.text
                .chars()
                .next()
                .is_none_or(|ch| !prohibited_line_start(ch))
        }));
        assert!(lines.iter().all(|line| {
            line.text
                .chars()
                .last()
                .is_none_or(|ch| !prohibited_line_end(ch))
        }));
    }

    /// The input box must reserve only a single content row for a short input
    /// but grow to fit wrapped text when the input is long.
    #[test]
    fn input_box_grows_with_wrapped_content() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let messages: Vec<TranscriptMessage> = Vec::new();

        fn render_with(theme: &Theme, messages: &[TranscriptMessage], input: &str) -> Rect {
            let backend = TestBackend::new(40, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut rect = Rect::default();
            terminal
                .draw(|f| {
                    let mut layout_map = LayoutMap::new();
                    let r = draw_transcript(
                        f,
                        &mut layout_map,
                        TranscriptView {
                            messages,
                            scroll: 0,
                            selection: &SelectionState::None,
                            activity: "",
                            spinner_phase: 0,
                            input,
                            byte_cursor: input.len(),
                            chrome_hidden: false,
                            subagent_bar: None,
                            side_banner: None,
                            pursuit: None,
                            todos: None,
                            review_alert: String::new(),
                            turn_started_at: None,
                            hovered_step: None,
                            focused_target: None,
                            logo: None,
                            theme,
                        },
                    );
                    rect = r.input_rect;
                })
                .unwrap();
            rect
        }

        // Short input: one content line + two padding rows = 3.
        let short = render_with(&theme, &messages, "hi");
        assert_eq!(short.height, 3);

        // Long input wraps across many lines on a 40-wide terminal; the box
        // must grow beyond the single-line baseline.
        let long_input = "word ".repeat(40);
        let tall = render_with(&theme, &messages, &long_input);
        assert!(
            tall.height > 3,
            "wrapped input should grow the box, got height {}",
            tall.height
        );
        // ...but never more than half the terminal.
        assert!(tall.height <= 12);
    }

    /// An empty composer must still record a layout-map region for its single
    /// text row. Without it a click inside the empty box can't resolve to a
    /// cursor, so the click handler never switches keyboard focus back to the
    /// Compose zone (it stays stuck in Browse). See `draw_composer` /
    /// `composer_wrapped`.
    #[test]
    fn draw_composer_records_region_for_empty_input() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let backend = TestBackend::new(30, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut layout_map = LayoutMap::new();
        let input_rect = Rect::new(0, 0, 30, 3);
        terminal
            .draw(|f| {
                draw_composer(
                    f,
                    input_rect,
                    "",
                    0,
                    true,
                    true,
                    &theme,
                    &mut layout_map,
                    true,
                    &mut 0,
                    &SelectionState::None,
                );
            })
            .unwrap();

        // The empty text row sits one line below the box's top edge.
        let cursor = layout_map
            .hit_test(
                input_rect.x + COMPOSER_PROMPT_PREFIX_COLS as u16,
                input_rect.y + 1,
            )
            .expect("click inside empty input box must resolve to a cursor");
        assert_eq!(cursor.message_idx, INPUT_MSG_IDX);
        assert_eq!(cursor.byte_offset, 0);
    }

    /// `draw_composer` must not panic for tricky inputs and should place the caret
    /// on the second wrapped line when the cursor sits past the first wrap.
    #[test]
    fn draw_composer_wraps_and_positions_caret() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let backend = TestBackend::new(20, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        // "aaaa bbbb cccc" wraps within the ~17-wide inner area; cursor at the
        // very end should be on a later line, not off the box.
        let input = "aaaa bbbb cccc dddd eeee";
        terminal
            .draw(|f| {
                draw_composer(
                    f,
                    Rect::new(0, 0, 20, 8),
                    input,
                    input.len(),
                    true,
                    true,
                    &theme,
                    &mut LayoutMap::new(),
                    true,
                    &mut 0,
                    &SelectionState::None,
                );
            })
            .unwrap();
    }

    /// Sent user messages and the composer must render as solid panels whose
    /// text keeps a 2-column inner padding on both sides (in the panel bg),
    /// matching the header. This locks the geometry so a refactor can't quietly
    /// drop the right-side padding again.
    #[test]
    fn user_message_and_composer_keep_symmetric_panel_padding() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let user_bg = theme.user_surface();
        let input_bg = theme.input_surface();
        let app_bg = theme.surface();
        let width = 60u16;
        let backend = TestBackend::new(width, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        // A long user message fills the first wrapped line edge to edge, so the
        // right-side padding is only present if the wrap width reserves it.
        let messages = vec![TranscriptMessage::new(
            neenee_core::Role::User,
            "x".repeat(200),
        )];
        let long_input = "y".repeat(200);

        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                // draw_transcript only computes the input box geometry; the composer
                // itself is drawn separately (as the live app does), using the
                // returned input_rect.
                let render = draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "",
                        spinner_phase: 0,
                        input: &long_input,
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: None,
                        side_banner: None,
                        pursuit: None,
                        todos: None,
                        review_alert: String::new(),
                        turn_started_at: None,
                        hovered_step: None,
                        focused_target: None,
                        logo: None,
                        theme: &theme,
                    },
                );
                let mut input_scroll = 0;
                draw_composer(
                    f,
                    render.input_rect,
                    &long_input,
                    0,
                    true,
                    true,
                    &theme,
                    &mut layout_map,
                    false,
                    &mut input_scroll,
                    &SelectionState::None,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();

        // Find the first user-message text row: col 0,1 are the app_bg outer
        // gutter, col 2,3 are the left inner pad (user_panel_bg), col 4 starts
        // the text. Scan for the row whose col 4 is 'x' under user_panel_bg.
        let user_row = (0..buffer.area.height)
            .find(|&y| {
                let c4 = &buffer[(4, y)];
                c4.symbol() == "x" && c4.bg == user_bg
            })
            .expect("a user message text row should be rendered");

        // Left: 2-col app_bg outer gutter, then 2-col user_panel_bg inner pad.
        assert_eq!(buffer[(0, user_row)].bg, app_bg, "left outer gutter");
        assert_eq!(buffer[(1, user_row)].bg, app_bg, "left outer gutter");
        assert_eq!(
            buffer[(2, user_row)].bg,
            user_bg,
            "left inner padding must be user_panel_bg"
        );
        assert_eq!(
            buffer[(3, user_row)].bg,
            user_bg,
            "left inner padding is 2 cols, not 1"
        );
        assert_eq!(buffer[(4, user_row)].symbol(), "x", "text starts at col 4");

        // Right: 2-col user_panel_bg inner pad, then 2-col app_bg outer gutter.
        // user_text_width = (60 - 4) - 4 = 52 -> text fills cols 4..56.
        assert_eq!(
            buffer[(56, user_row)].symbol(),
            " ",
            "right inner padding must stay clear of wrapped text"
        );
        assert_eq!(buffer[(56, user_row)].bg, user_bg, "right inner padding");
        assert_eq!(buffer[(57, user_row)].bg, user_bg, "right inner padding");
        assert_eq!(buffer[(58, user_row)].bg, app_bg, "right outer gutter");
        assert_eq!(buffer[(59, user_row)].bg, app_bg, "right outer gutter");

        // Composer: the input panel starts at x = FOOTER_H_INSET (2). `›` at
        // x=2, text from x=4, and a 2-col right pad in input_bg before the
        // app_bg gutter at the far right.
        let composer_row = (0..buffer.area.height)
            .find(|&y| {
                let c4 = &buffer[(4, y)];
                c4.symbol() == "y" && c4.bg == input_bg
            })
            .expect("a composer text row should be rendered");
        assert_eq!(buffer[(2, composer_row)].symbol(), "›", "composer prompt");
        assert_eq!(
            buffer[(4, composer_row)].symbol(),
            "y",
            "composer text starts at col 4"
        );
        // full_w (composer panel) = 60 - 2*FOOTER_H_INSET = 56, panel spans
        // x=2..58. Right pad at x=56,57 (input_bg), gutter x=58,59 (app_bg).
        assert_eq!(
            buffer[(56, composer_row)].bg,
            input_bg,
            "composer right inner padding"
        );
        assert_eq!(
            buffer[(57, composer_row)].bg,
            input_bg,
            "composer right inner padding"
        );
        assert_eq!(
            buffer[(58, composer_row)].bg,
            app_bg,
            "composer right outer gutter"
        );
        assert_eq!(
            buffer[(59, composer_row)].bg,
            app_bg,
            "composer right outer gutter"
        );
    }

    /// A queued user message (one staged in the send queue waiting for the
    /// in-flight turn to finish) must render with the dimmer
    /// `user_panel_bg_queued` band and a visible "⏸ Queued" badge so the user
    /// can tell their message is pending, not delivered.
    #[test]
    fn queued_user_message_renders_badge_and_dimmer_bg() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let queued_bg = theme.user_surface_queued();
        let delivered_bg = theme.user_surface();
        let width = 40u16;
        let backend = TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).unwrap();

        let messages = vec![
            TranscriptMessage::new(neenee_core::Role::User, "first queued").queued(),
            TranscriptMessage::new(neenee_core::Role::User, "second queued").queued(),
        ];

        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let _ = draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "",
                        spinner_phase: 0,
                        input: "",
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: None,
                        side_banner: None,
                        pursuit: None,
                        todos: None,
                        review_alert: String::new(),
                        turn_started_at: None,
                        hovered_step: None,
                        focused_target: None,
                        logo: None,
                        theme: &theme,
                    },
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();

        // Both queued panels must carry the queued bg, never the delivered bg.
        // Scan the inner-pad columns (2,3) of every row for any cell painted
        // with the delivered bg — that would mean a queued message leaked the
        // wrong surface.
        for y in 0..buffer.area.height {
            for x in 2..4 {
                let bg = buffer[(x, y)].bg;
                assert_ne!(
                    bg, delivered_bg,
                    "queued panels must never carry the delivered bg, found at ({},{})",
                    x, y
                );
            }
        }

        // Each queued user message renders one "⏸ Queued" badge row. Count
        // rows whose inner-padding cells carry the queued bg AND whose
        // first-content cell starts with the pause glyph.
        let badge_count = (0..buffer.area.height)
            .filter(|&y| buffer[(2, y)].bg == queued_bg && buffer[(4, y)].symbol() == "⏸")
            .count();
        assert_eq!(
            badge_count, 2,
            "each queued user message must render one badge row, got {}",
            badge_count
        );
    }

    /// The transcript content rect must be recorded after rendering so that
    /// clicks on gap rows (which carry no region) still switch keyboard focus
    /// to Browse. It must span the horizontal band inside the outer gutters
    /// (clicks in the gutters are not transcript clicks) and the vertical
    /// extent of drawn content, including the inter-message gap row.
    #[test]
    fn transcript_content_rect_spans_band_and_gap_rows() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let width = 40u16;
        let backend = TestBackend::new(width, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        // Two assistant text messages so a `MESSAGE_GAP_ROWS` blank row is
        // emitted between them — that row is rendered but never registered.
        let messages = vec![
            TranscriptMessage::new(neenee_core::Role::Assistant, "first".to_string()),
            TranscriptMessage::new(neenee_core::Role::Assistant, "second".to_string()),
        ];
        let mut layout_map = LayoutMap::new();
        terminal
            .draw(|f| {
                draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "",
                        spinner_phase: 0,
                        input: "",
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: None,
                        side_banner: None,
                        pursuit: None,
                        todos: None,
                        review_alert: String::new(),
                        turn_started_at: None,
                        hovered_step: None,
                        focused_target: None,
                        logo: None,
                        theme: &theme,
                    },
                );
            })
            .unwrap();

        let rect = layout_map
            .transcript_content_rect()
            .expect("content rect must be recorded when messages are drawn");
        // Horizontal band excludes the outer `TRANSCRIPT_H_INSET` gutters.
        assert_eq!(rect.x, TRANSCRIPT_H_INSET);
        assert_eq!(rect.width, width - 2 * TRANSCRIPT_H_INSET);

        // The whole point of the rect: a gap row between the two messages is
        // rendered but carries no region (clicking it does not resolve to a
        // cursor). It must still fall inside the content rect so the click
        // handler can switch focus to Browse.
        let gap_y = (rect.y..rect.y + rect.height)
            .find(|&y| layout_map.region_at(rect.x, y).is_none())
            .expect("there must be at least one unregistered gap row between the two messages");
        assert!(rect.y <= gap_y && gap_y < rect.y + rect.height);
    }

    /// Wide tables (including CJK content) must keep borders intact and never
    /// overflow the viewport: columns shrink to fit, cell text wraps, and
    /// every rendered line stays within the available width.
    #[test]
    fn wide_table_shrinks_columns_and_keeps_borders_intact() {
        use crate::tui::document::TableAlignment;

        let headers = vec![
            "工具".to_string(),
            "类型".to_string(),
            "底层实现".to_string(),
            "关键特性".to_string(),
        ];
        let rows = vec![
            vec![
                "bash".to_string(),
                "Write".to_string(),
                "std::process::Command（sh -c / cmd /C）".to_string(),
                "执行 shell 命令，支持 timeout，输出截断".to_string(),
            ],
            vec![
                "read_file".to_string(),
                "Read".to_string(),
                "std::fs::read_to_string".to_string(),
                "支持 offset/limit".to_string(),
            ],
        ];
        let aligns = vec![
            TableAlignment::None,
            TableAlignment::None,
            TableAlignment::None,
            TableAlignment::None,
        ];

        // ── Narrow terminal (34 cols): table is far wider, must shrink ──
        let lines = build_table_render(&headers, &rows, &aligns, 34).lines;
        assert!(!lines.is_empty(), "table must produce output");

        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.width() <= 34,
                "line {i} overflows: {} cols: {}",
                line.width(),
                line
            );
        }
        assert!(lines.first().unwrap().starts_with('┌'));
        assert!(lines.last().unwrap().starts_with('└'));
        assert!(
            lines.iter().any(|l| l.starts_with('├')),
            "missing header/body separator"
        );
        // Two body rows → one separator between them (plus one after header).
        let sep_count = lines.iter().filter(|l| l.starts_with('├')).count();
        assert_eq!(
            sep_count, 2,
            "expected 2 separators (header→body + row→row), got {sep_count}"
        );
        let pipe_counts: Vec<usize> = lines
            .iter()
            .filter(|l| l.starts_with('│'))
            .map(|l| l.matches('│').count())
            .collect();
        assert!(!pipe_counts.is_empty(), "must have data lines");
        assert!(
            pipe_counts.iter().all(|&c| c == pipe_counts[0]),
            "all data lines must have the same number of column separators"
        );

        // ── Wide terminal (80 cols): table fits without shrinking ──
        let wide_lines = build_table_render(&headers, &rows, &aligns, 76).lines;
        for (i, line) in wide_lines.iter().enumerate() {
            assert!(
                line.width() <= 76,
                "wide line {i} overflows: {} cols",
                line.width()
            );
        }
        // When it fits, the table should be shorter (no wrapping needed).
        assert!(
            wide_lines.len() <= lines.len(),
            "wide table should have fewer lines than shrunk table"
        );
    }

    #[test]
    fn shrink_columns_preserves_minimum_and_proportions() {
        // Intrinsic [10, 5, 20], target 24, min 3.
        // total_min = 9, shrinkable = 26, available = 15.
        // col0: 3 + 7*15/26 = 3 + 4 = 7
        // col1: 3 + 2*15/26 = 3 + 1 = 4
        // col2: 3 + 17*15/26 = 3 + 9 = 12
        let result = shrink_column_widths(&[10, 5, 20], 24, 3);
        assert_eq!(result.len(), 3);
        assert!(result.iter().all(|&w| w >= 3), "must respect minimum");
        assert!(
            result.iter().sum::<usize>() <= 24,
            "must fit within target, got {}",
            result.iter().sum::<usize>()
        );
        // Largest intrinsic column stays largest after shrinking.
        let max_val = *result.iter().max().unwrap();
        let max_idx = result.iter().position(|&v| v == max_val).unwrap();
        assert_eq!(max_idx, 2);
    }

    #[test]
    fn shrink_columns_with_tiny_target_returns_all_minimum() {
        let result = shrink_column_widths(&[10, 20, 30], 5, 3);
        assert_eq!(result, vec![3, 3, 3]);
    }

    /// Drive `draw_history_modal` against a real buffer across every input
    /// state the Ctrl+R picker can land in. The assertions are deliberately
    /// structural ("does not panic, produces a non-empty frame") because the
    /// fuzzy highlight math is already covered by `fuzzy::tests`; here we
    /// only need to prove the renderer consumes each state without exploding.
    #[test]
    fn history_modal_renders_every_query_state() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let history = vec![
            "git status".to_string(),
            "git commit -am 'ship it'".to_string(),
            "cargo test".to_string(),
            "review the diff before sending".to_string(),
        ];

        let cases: &[(&str, usize)] = &[
            ("", history.len()), // empty query → everything surfaces
            ("git", 2),          // partial match → subset with highlights
            ("zzz", 0),          // no subsequence → empty placeholder
        ];

        for (query, expected_matches) in cases {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut ranked = crate::tui::fuzzy::rank(&history, query);
            crate::tui::fuzzy::sort_by_score(&mut ranked);
            assert_eq!(
                ranked.len(),
                *expected_matches,
                "query {:?} should surface {} entries",
                query,
                expected_matches
            );
            terminal
                .draw(|f| {
                    draw_history_modal(
                        f,
                        &mut LayoutMap::new(),
                        &history,
                        query,
                        query.chars().count(),
                        &ranked,
                        0,
                        &theme,
                    );
                })
                .expect("draw must not panic");
        }

        // Empty history must render the "(no history yet)" placeholder rather
        // than indexing into an empty slice.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let empty: Vec<String> = Vec::new();
        let ranked: Vec<(usize, crate::tui::fuzzy::FuzzyMatch)> =
            crate::tui::fuzzy::rank(&empty, "");
        terminal
            .draw(|f| {
                draw_history_modal(f, &mut LayoutMap::new(), &empty, "", 0, &ranked, 0, &theme);
            })
            .expect("empty-history draw must not panic");
    }

    /// With no messages, `draw_transcript` renders the empty-state hero in
    /// place of the stream: `content_lines` is non-zero (so the app loop does
    /// not treat it as a zero-height stream) and the call does not panic.
    #[test]
    fn empty_session_renders_empty_state_with_nonzero_height() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let messages: Vec<TranscriptMessage> = Vec::new();

        let mut render_opt: Option<TranscriptRender> = None;
        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                render_opt = Some(draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "idle",
                        spinner_phase: 0,
                        input: "",
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: None,
                        side_banner: None,
                        pursuit: None,
                        todos: None,
                        review_alert: String::new(),
                        turn_started_at: None,
                        hovered_step: None,
                        focused_target: None,
                        logo: None,
                        theme: &theme,
                    },
                ));
            })
            .expect("empty-session draw must not panic");
        let render = render_opt.expect("draw_transcript must return a render");

        // The empty-state hero replaces the transcript; it occupies the logo
        // rows plus a gap, never zero, so scroll-follow logic stays honest.
        assert!(
            render.content_lines > 0,
            "empty state should report non-zero content_lines"
        );
        assert!(render.sticky.is_none(), "no sticky header on empty state");
        assert!(
            render.view_height > 0,
            "view_height should reflect the viewport, not be zero"
        );
    }

    /// A non-empty session skips the empty-state branch entirely — the hero
    /// never competes with real content.
    #[test]
    fn nonempty_session_does_not_render_empty_state() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let messages = vec![TranscriptMessage::new(neenee_core::Role::User, "hello")];

        let mut render_opt: Option<TranscriptRender> = None;
        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                render_opt = Some(draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "idle",
                        spinner_phase: 0,
                        input: "",
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: None,
                        side_banner: None,
                        pursuit: None,
                        todos: None,
                        review_alert: String::new(),
                        turn_started_at: None,
                        hovered_step: None,
                        focused_target: None,
                        logo: None,
                        theme: &theme,
                    },
                ));
            })
            .expect("non-empty draw must not panic");
        let render = render_opt.expect("draw_transcript must return a render");

        // With a real message the stream is rendered normally — content_lines
        // reflects at least one rendered message rather than the fixed
        // empty-state height.
        assert!(
            render.content_lines > 0,
            "non-empty session should render its messages"
        );
    }

    /// A user-supplied logo (from `logo.txt`) replaces the built-in wordmark
    /// on the empty state, and `content_lines` tracks its (clamped) height so
    /// scroll accounting stays honest. A four-line user logo yields six
    /// reported lines (4 + blank gap + tagline), distinct from the built-in
    /// wordmark's height.
    #[test]
    fn empty_session_uses_user_logo_and_reports_its_height() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = Theme::default();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let messages: Vec<TranscriptMessage> = Vec::new();
        // Four lines → reported content is 4 + 2 (gap + tagline) = 6.
        let logo: Vec<String> = vec![
            "  N N  ".to_string(),
            " N N N ".to_string(),
            "  N N  ".to_string(),
            "       ".to_string(),
        ]
        .into_iter()
        .chain(std::iter::repeat_n("xxxxx".to_string(), 0))
        .collect();

        let mut render_opt: Option<TranscriptRender> = None;
        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                render_opt = Some(draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "idle",
                        spinner_phase: 0,
                        input: "",
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: None,
                        side_banner: None,
                        pursuit: None,
                        todos: None,
                        review_alert: String::new(),
                        turn_started_at: None,
                        hovered_step: None,
                        focused_target: None,
                        logo: Some(&logo),
                        theme: &theme,
                    },
                ));
            })
            .expect("user-logo empty-state draw must not panic");
        let render = render_opt.expect("draw_transcript must return a render");

        // 4 logo lines + 1 blank gap + 1 tagline = 6 content lines.
        assert_eq!(
            render.content_lines, 6,
            "user-logo content_lines must be logo rows + gap + tagline"
        );
    }
}

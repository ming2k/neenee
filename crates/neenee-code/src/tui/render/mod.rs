//! Rendering engine: draws the transcript (and footer chrome) using neenee-tui
//! while recording semantic-to-screen layout information.

mod chrome;
mod composer;
mod design;
mod disclosure;
mod empty_state;
mod markdown_table;
mod message_body;
mod notice;
mod overlays;
mod primitives;
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
    TOOL_STEP_BODY_TOP_GAP_ROWS, TOOL_STEP_CHILDREN_GAP_ROWS, TRANSCRIPT_BODY_LEADING_INDENT,
    TRANSCRIPT_H_INSET,
};
use disclosure::{
    StickyStep, draw_reasoning_trace, draw_side_banner, draw_sticky_summary_if_needed,
    draw_subagent_bar, draw_subagent_inline_step, draw_tool_step,
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
    draw_permission_sheet, draw_permissions_manager, draw_question_modal, draw_session_modal,
    draw_sessions_modal, draw_tool_step_detail_overlay,
};
pub use primitives::recess_backdrop;
use primitives::viewport_rect;
#[cfg(test)]
use text_layout::WrappedLine;
#[cfg(test)]
use text_layout::{
    block_selection_range, line_selection, prohibited_line_end, prohibited_line_start,
};
pub use theme::Theme;

use neenee_tui::{
    Block as RtBlock, Constraint, Direction, Frame, Layout, Line, Paragraph, Rect, Span, Style,
};

use crate::tui::document::TranscriptMessage;
use crate::tui::layout::{InteractiveTarget, LayoutMap};
use crate::tui::selection::{CellDragInfo, SelectionState};
#[cfg(test)]
use neenee_core::{PermissionRequest, ProviderPickerSnapshot, UserQuestionRequest};
#[cfg(test)]
use std::collections::HashMap;

/// Inner rect of a transcript-area region after reserving the uniform
/// [`TRANSCRIPT_H_INSET`] left+right `app_bg` gutters. This is the **single
/// point** where the horizontal inset is applied — called exactly three times
/// in `draw_transcript`: once for the content stream (the `band` every
/// downstream component receives), and once each for the subagent bar and side
/// banner rects so they align with the content band. Individual components no
/// longer clip or hand-pad their own gutter; they trust the rect they receive.
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
    pub cell_selection: Option<&'a CellDragInfo>,
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
    /// Keyboard-focused activatable target. When `Some`, the matching step's
    /// summary line is painted with the focus-ring cue (a reversed fg/bg bar)
    /// so keyboard navigation via `Ctrl+↑`/`Ctrl+↓` has a clear, unambiguous
    /// visual indicator that does not compete with the hover/expand luminance
    /// channel. `None` means no step is focused.
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
    /// Screen rect of the `todos d/t` segment on the activity bar, so a click
    /// on it opens the Activity modal directly on the Todos section. `None`
    /// when no todos are shown (empty task list or bar hidden).
    pub todos_rect: Option<Rect>,
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
        cell_selection,
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
        focused_target,
        logo,
        theme,
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
    // The persistent todos badge (right-pinned) keeps the activity row alive
    // even when the harness is idle, so an active task list is always visible
    // — not only while a turn is running.
    let has_visible_todos = todos.map(|l| !l.items.is_empty()).unwrap_or(false);
    let activity_row_needed =
        status_active || (has_visible_todos && !chrome_hidden && !in_subagent);
    let status_height: u16 = if activity_row_needed {
        STATUS_BAR_ROWS
    } else {
        0
    };

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
        (
            sub[0],
            // The bar spans the inset band so it aligns with the transcript
            // content rather than edge-to-edge.
            Some(transcript_band_rect(sub[1])),
        )
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
        // The banner spans the inset band, matching the transcript content.
        draw_side_banner(frame, transcript_band_rect(sub[0]), status, theme);
        transcript_area = sub[1];
    }
    // Apply the uniform horizontal inset (`TRANSCRIPT_H_INSET` on each side)
    // exactly once, here at the transcript-stream entry point. Every
    // downstream component receives `band` — an already-inset rect — so none
    // of them re-clips or hand-pads a leading gutter. The empty-state hero is
    // the sole exception: it centers across the full viewport, so it keeps
    // `transcript_area` (un-inset). Banners and bars are rendered from their
    // own layout-split rects before this point, so they are unaffected.
    let band = transcript_band_rect(transcript_area);
    let mut current_y = band.y;
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
        empty_state::draw_empty_state(frame, transcript_area, logo, theme);
        // Account for the hero so the app loop does not treat the session as a
        // zero-height stream (which would mis-pin the scroll position).
        content_lines = empty_state::empty_state_content_lines(logo);
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
                            band,
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
                    band,
                    msg,
                    &mut skip_rows,
                    &mut current_y,
                    &mut content_lines,
                    theme,
                );
            } else if msg.is_subagent_task() {
                draw_subagent_inline_step(
                    frame,
                    band,
                    msg,
                    mi,
                    theme,
                    layout_map,
                    &mut skip_rows,
                    &mut current_y,
                    &mut content_lines,
                    hovered_step == Some(mi),
                    focused_target == Some(InteractiveTarget::tool_step(mi)),
                );
            } else if msg.is_tool_step() {
                draw_tool_step(
                    frame,
                    band,
                    msg,
                    mi,
                    selection,
                    cell_selection,
                    theme,
                    layout_map,
                    &mut skip_rows,
                    &mut current_y,
                    &mut content_lines,
                    &mut sticky_steps,
                    hovered_step == Some(mi),
                    focused_target == Some(InteractiveTarget::tool_step(mi)),
                );
            } else if msg.is_thinking() {
                draw_reasoning_trace(
                    frame,
                    band,
                    msg,
                    mi,
                    selection,
                    cell_selection,
                    theme,
                    layout_map,
                    &mut skip_rows,
                    &mut current_y,
                    &mut content_lines,
                    &mut sticky_steps,
                    hovered_step == Some(mi),
                    focused_target == Some(InteractiveTarget::thinking(mi)),
                );
            } else {
                draw_message_body(
                    frame,
                    band,
                    msg,
                    mi,
                    selection,
                    cell_selection,
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
            //
            // Collapsed tool steps stack flush: a batch of parallel/sequential
            // collapsed tool-call headers forms a compact log block with no blank
            // rows between them. The separating row is supplied *only* by an
            // expanded step's body — its top gap (`TOOL_STEP_BODY_TOP_GAP_ROWS`)
            // separates it from its own header, and this message-level row supplies
            // its bottom separator to the next step's header. So a collapsed tool
            // step followed by any tool step suppresses the row.
            let next_is_tool_step = messages
                .get(mi + 1)
                .is_some_and(|next| next.is_tool_step() || next.is_subagent_task());
            let collapsed_tool_into_tool_step =
                msg.is_tool_step() && msg.tool_step_expanded() == Some(false) && next_is_tool_step;
            let next_is_step = messages.get(mi + 1).is_some_and(|next| {
                next.is_thinking() || next.is_tool_step() || next.is_subagent_task()
            });
            if collapsed_tool_into_tool_step {
                // Flush stack: no separating row between adjacent collapsed tool
                // steps. Scroll accounting (content_lines / current_y) is left
                // untouched because no rows are consumed.
            } else if msg.role != neenee_core::Role::User || next_is_step {
                content_lines += MESSAGE_GAP_ROWS;
                if skip_rows > 0 {
                    skip_rows = skip_rows.saturating_sub(1);
                } else if current_y < band.y + band.height {
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
        let content_bottom = current_y.min(band.y + band.height);
        if content_bottom > band.y {
            layout_map.set_transcript_content_rect(Rect::new(
                band.x,
                band.y,
                band.width,
                content_bottom - band.y,
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
    // `draw_activity_bar` returns an `ActivityBarHit` carrying both the full
    // bar rect (→ `activity_rect`) and the `todos d/t` segment rect
    // (→ `todos_rect`, so a click there opens the Todos section directly).
    let (activity_rect, todos_rect) = if activity_row_needed {
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
        .map(|hit| (Some(hit.bar_rect), hit.todos_rect))
        .unwrap_or((None, None))
    } else {
        (None, None)
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
    let sticky_info = draw_sticky_summary_if_needed(frame, band, &sticky_steps, scroll, theme);

    TranscriptRender {
        input_rect,
        hint_rect,
        activity_rect,
        todos_rect,
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
        // The area arrives already inset, so the badge starts at the edge.
        let line = Line::from(vec![
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
        let theme = Theme::default();
        let mut terminal = neenee_tui::TestTerminal::new(80, 30);

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
                        cell_selection: None,
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
            });

        // Modals + permission sheet on a fresh frame.
        terminal.draw(|f| {
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
                &mut 0,
                true,
                false,
                true,
                &theme,
            );
            draw_model_editor(f, "OpenAI", 0, "", "gpt-4o", "", 0, &theme);
            {
                let mut scroll = 0;
                draw_help_modal(f, &mut scroll, &theme);
            }
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
            let mut hit_map = crate::tui::layout::ModalHitMap::new();
            draw_question_modal(
                f,
                &mut hit_map,
                &question_request,
                0,
                &[vec![1]],
                &[String::new()],
                1,
                &mut 0,
                true,
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
            for idx in [0usize, 2] {
                draw_session_modal(
                    f,
                    "custom-unknown",
                    "some-model",
                    &key_status,
                    &[],
                    Some(&snapshot),
                    idx,
                    &mut 0,
                    true,
                    &theme,
                );
            }
            // And once with no snapshot, to cover the placeholder fallbacks.
            draw_session_modal(
                f,
                "custom-unknown",
                "some-model",
                &key_status,
                &[],
                None,
                0,
                &mut 0,
                true,
                &theme,
            );
        });

        terminal.draw(|f| {
            let request = PermissionRequest {
                id: "p1".to_string(),
                tool: "bash".to_string(),
                label: "bash".to_string(),
                description: "run a command".to_string(),
                arguments: r#"{"command":"ls"}"#.to_string(),
                scope: "*".to_string(),
            };
            let rect = neenee_tui::Rect::new(0, 0, 60, 3);
            let mut hit_map = crate::tui::layout::ModalHitMap::new();
            let _ =
                draw_permission_sheet(f, &mut hit_map, &request, 0, false, false, 0, rect, &theme);
        });
    }

    /// Render both the compact subagent step (root view) and the zoomed-in
    /// subagent view with its navigation bar, ensuring no layout panics.
    #[test]
    fn subagent_step_and_view_render_without_panicking() {
        let theme = Theme::default();
        let mut terminal = neenee_tui::TestTerminal::new(80, 30);

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

        terminal.draw(|f| {
            let mut layout_map = LayoutMap::new();
            let _ = draw_transcript(
                f,
                &mut layout_map,
                TranscriptView {
                    messages: &root_messages,
                    scroll: 0,
                    selection: &SelectionState::None,
                    cell_selection: None,
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
        });

        // Zoomed-in subagent view: the task's children are the message stream
        // and the navigation bar is shown.
        let children = root_messages[1].subagent_children().unwrap().to_vec();
        terminal.draw(|f| {
            let mut layout_map = LayoutMap::new();
            let _ = draw_transcript(
                f,
                &mut layout_map,
                TranscriptView {
                    messages: &children,
                    scroll: 0,
                    selection: &SelectionState::None,
                    cell_selection: None,
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
        });
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
        let theme = Theme::default();
        let messages: Vec<TranscriptMessage> = Vec::new();

        fn render_with(theme: &Theme, messages: &[TranscriptMessage], input: &str) -> Rect {
            let mut terminal = neenee_tui::TestTerminal::new(40, 24);
            let mut rect = Rect::default();
            terminal.draw(|f| {
                let mut layout_map = LayoutMap::new();
                let r = draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        cell_selection: None,
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
            });
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
    /// cursor, so the click handler can't clear a focused step to hand typing
    /// back to the prompt. See `draw_composer` / `composer_wrapped`.
    #[test]
    fn draw_composer_records_region_for_empty_input() {
        let theme = Theme::default();
        let mut terminal = neenee_tui::TestTerminal::new(30, 5);
        let mut layout_map = LayoutMap::new();
        let input_rect = Rect::new(0, 0, 30, 3);
        terminal.draw(|f| {
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
        });

        // The empty text row sits one line below the box's top edge.
        let cursor = layout_map
            .cursor_at(
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
        let theme = Theme::default();
        let mut terminal = neenee_tui::TestTerminal::new(20, 12);
        // "aaaa bbbb cccc" wraps within the ~17-wide inner area; cursor at the
        // very end should be on a later line, not off the box.
        let input = "aaaa bbbb cccc dddd eeee";
        terminal.draw(|f| {
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
        });
    }

    /// The caret must land flush against the final glyph at the end of the
    /// input, measured in display columns — i.e. exactly where the grid painted
    /// the text. This is the CJK regression: a buggy grapheme-floor returned the
    /// last grapheme *start*, leaving the caret two columns short of a wide
    /// glyph (one for ASCII). The caret column must equal the rendered width of
    /// the text, for both wide and narrow glyphs.
    #[test]
    fn draw_composer_caret_flush_against_final_grapheme() {
        let theme = Theme::default();

        for (label, input, expected_cols) in [
            ("cjk", "中文", 4usize),
            ("ascii", "ab", 2),
            ("mixed", "a中", 3),
        ] {
            let mut terminal = neenee_tui::TestTerminal::new(20, 5);
            terminal.draw(|f| {
                draw_composer(
                    f,
                    Rect::new(0, 0, 20, 4),
                    input,
                    input.len(),
                    true,
                    true,
                    &theme,
                    &mut LayoutMap::new(),
                    false,
                    &mut 0,
                    &SelectionState::None,
                );
            });
            let cursor = match terminal.cursor() {
                neenee_tui::CursorState::Visible(x, y) => (x, y),
                other => panic!("{label}: caret should be visible, got {other:?}"),
            };
            // The text row sits one line below the box's top `▄` edge, and the
            // caret follows the `› ` prefix plus the full rendered width.
            assert_eq!(
                cursor,
                (
                    (COMPOSER_PROMPT_PREFIX_COLS + expected_cols) as u16,
                    super::design::COMPOSER_TEXT_ROW_OFFSET,
                ),
                "{label}: caret not flush with end of {input:?}"
            );
        }
    }

    /// A CJK selection highlight must paint BOTH columns of each wide glyph
    /// (head + continuation), cover exactly the selected glyphs, and leave the
    /// trailing pad on the panel background — no extra glyph, no half-highlighted
    /// wide char. Exercises the full-3-CJK selection the live bug report used.
    #[test]
    fn composer_cjk_selection_covers_full_width_glyphs() {
        use crate::tui::layout::SemanticCursor;
        let theme = Theme::default();
        let panel_bg = theme.input_surface();
        let sel_bg = theme.selected();
        let input = "中文测"; // 3 wide glyphs = 6 cols (cols 2..8)
        // Select all three. Head points AT 测 (byte 6); the inclusive-head model
        // includes the glyph under the head, so the range is [0, 9) = "中文测".
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(INPUT_MSG_IDX, 0, 0),
            head: SemanticCursor::new(INPUT_MSG_IDX, 0, 6),
        };
        let mut terminal = neenee_tui::TestTerminal::new(20, 5);
        terminal.draw(|f| {
            draw_composer(
                f,
                Rect::new(0, 0, 20, 4),
                input,
                input.len(),
                true,
                false,
                &theme,
                &mut LayoutMap::new(),
                false,
                &mut 0,
                &sel,
            );
        });
        let g = terminal.buffer();
        let y = super::design::COMPOSER_TEXT_ROW_OFFSET;
        // Cols: 0='›', 1=gap, 2-7='中文测'(sel), 8+=panel tail.
        for (col, label, expect_sel) in [
            (2usize, "中 head", true),
            (3, "中 cont", true),
            (4, "文 head", true),
            (5, "文 cont", true),
            (6, "测 head", true),
            (7, "测 cont", true),
            (8, "tail 0", false),
            (9, "tail 1", false),
        ] {
            let cell = g.get(col as u16, y).unwrap();
            let want = if expect_sel { sel_bg } else { panel_bg };
            assert_eq!(
                cell.bg, want,
                "{label} at col {col}: bg {:?} expected {:?}",
                cell.bg, want
            );
        }
        // While a selection is active the caller passes `show_caret = false`
        // (see the event loop), so no terminal caret is placed on top of the
        // highlighted glyphs — the "appended flickering character" symptom.
        assert!(
            matches!(terminal.cursor(), neenee_tui::CursorState::Hidden),
            "caret must be hidden while a selection is active"
        );
    }

    #[test]
    fn composer_two_cjk_select_all_has_no_extra_glyph_or_tail_highlight() {
        use crate::tui::layout::SemanticCursor;

        let theme = Theme::default();
        let panel_bg = theme.input_surface();
        let sel_bg = theme.selected();
        let input = "你好";
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(INPUT_MSG_IDX, 0, 0),
            head: SemanticCursor::new(INPUT_MSG_IDX, 0, input.len()),
        };
        let mut terminal = neenee_tui::TestTerminal::new(16, 5);

        terminal.draw(|f| {
            draw_composer(
                f,
                Rect::new(0, 0, 16, 4),
                input,
                input.len(),
                true,
                false,
                &theme,
                &mut LayoutMap::new(),
                false,
                &mut 0,
                &sel,
            );
        });

        let y = super::design::COMPOSER_TEXT_ROW_OFFSET;
        let buffer = terminal.buffer();

        assert_eq!(buffer.get(2, y).unwrap().symbol(), "你");
        assert_eq!(buffer.get(2, y).unwrap().width, 2);
        assert_eq!(buffer.get(3, y).unwrap().symbol(), " ");
        assert_eq!(buffer.get(3, y).unwrap().width, 0);
        assert_eq!(buffer.get(4, y).unwrap().symbol(), "好");
        assert_eq!(buffer.get(4, y).unwrap().width, 2);
        assert_eq!(buffer.get(5, y).unwrap().symbol(), " ");
        assert_eq!(buffer.get(5, y).unwrap().width, 0);
        assert_eq!(
            buffer.get(6, y).unwrap().symbol(),
            " ",
            "tail cell must not contain a duplicate glyph"
        );

        for col in 2..=5 {
            assert_eq!(
                buffer.get(col, y).unwrap().bg,
                sel_bg,
                "col {col} should be selected"
            );
        }
        assert_eq!(
            buffer.get(6, y).unwrap().bg,
            panel_bg,
            "tail cell must remain on input panel background"
        );
        assert!(
            matches!(terminal.cursor(), neenee_tui::CursorState::Hidden),
            "caret must be hidden while a selection is active"
        );
    }

    /// Regression for the input-select bug: a click that starts a selection
    /// (anchor == head, a collapsed range) must highlight NOTHING, and a drag
    /// through the real click pipeline (layout_map → cursor_at) must highlight
    /// exactly the dragged glyphs with the correct background. The prior
    /// `inclusive_grapheme_end`-on-a-point logic lit up one glyph on every
    /// click and flickered as the drag moved — "an extra changing character
    /// appears and the selection background misbehaves".
    #[test]
    fn composer_collapsed_click_highlights_nothing_drag_highlights_cleanly() {
        let theme = Theme::default();
        let panel_bg = theme.input_surface();
        let sel_bg = theme.selected();
        let input = "中文测";
        let rect = Rect::new(0, 0, 20, 4);
        let text_row = super::design::COMPOSER_TEXT_ROW_OFFSET;

        // Record input regions so cursor_at can resolve real drag positions.
        let mut layout_map = LayoutMap::new();
        let mut rec = neenee_tui::TestTerminal::new(20, 5);
        rec.draw(|f| {
            draw_composer(
                f,
                rect,
                input,
                input.len(),
                true,
                false,
                &theme,
                &mut layout_map,
                true,
                &mut 0,
                &SelectionState::None,
            );
        });
        let anchor = layout_map.cursor_at(rect.x + 2, rect.y + text_row).unwrap();
        assert_eq!(anchor.byte_offset, 0);

        fn row_bgs(
            input: &str,
            rect: Rect,
            text_row: u16,
            theme: &Theme,
            sel: &SelectionState,
        ) -> Vec<neenee_tui::Color> {
            let mut t = neenee_tui::TestTerminal::new(20, 5);
            t.draw(|f| {
                draw_composer(
                    f,
                    rect,
                    input,
                    input.len(),
                    true,
                    false,
                    theme,
                    &mut LayoutMap::new(),
                    false,
                    &mut 0,
                    sel,
                );
            });
            (0..10u16)
                .map(|c| t.buffer().get(c, text_row).unwrap().bg)
                .collect()
        }

        // 1) Collapsed click (anchor == head): no glyph may carry the selection bg.
        let collapsed = SelectionState::Range {
            anchor,
            head: anchor,
        };
        for (col, bg) in row_bgs(input, rect, text_row, &theme, &collapsed)
            .into_iter()
            .enumerate()
        {
            assert_ne!(bg, sel_bg, "collapsed click lit up col {col}");
            let _ = panel_bg;
        }

        // 2) Drag onto 测's first column (byte 6): inclusive head selects all
        //    three glyphs; the trailing pad stays on the panel bg.
        let head = layout_map.cursor_at(rect.x + 6, rect.y + text_row).unwrap();
        assert_eq!(head.byte_offset, 6);
        let drag = SelectionState::Range { anchor, head };
        let bgs = row_bgs(input, rect, text_row, &theme, &drag);
        // cols 0,1 = prefix; 2..8 = "中文测" (selected); 8,9 = tail (panel).
        for (col, &bg) in bgs[2..8].iter().enumerate() {
            assert_eq!(bg, sel_bg, "col {} should be selected", col + 2);
        }
        for (col, &bg) in bgs[8..10].iter().enumerate() {
            assert_eq!(bg, panel_bg, "col {} should be panel tail", col + 8);
        }

        // 3) Drag to the second visual column of 中. The hit-test cursor maps
        // both columns of a wide glyph to that glyph's byte start; with an
        // inclusive head this selects 中 only, not the next glyph.
        let head = layout_map.cursor_at(rect.x + 3, rect.y + text_row).unwrap();
        assert_eq!(head.byte_offset, 1);
        let drag = SelectionState::Range { anchor, head };
        let bgs = row_bgs(input, rect, text_row, &theme, &drag);
        for (col, &bg) in bgs[2..4].iter().enumerate() {
            assert_eq!(bg, sel_bg, "col {} should select 中", col + 2);
        }
        for (col, &bg) in bgs[4..8].iter().enumerate() {
            assert_eq!(bg, panel_bg, "col {} should remain unselected", col + 4);
        }
    }

    #[test]
    fn user_message_and_composer_keep_symmetric_panel_padding() {
        let theme = Theme::default();
        let user_bg = theme.user_surface();
        let input_bg = theme.input_surface();
        let app_bg = theme.surface();
        let width = 60u16;
        let mut terminal = neenee_tui::TestTerminal::new(width, 24);

        // A long user message fills the first wrapped line edge to edge, so the
        // right-side padding is only present if the wrap width reserves it.
        let messages = vec![TranscriptMessage::new(
            neenee_core::Role::User,
            "x".repeat(200),
        )];
        let long_input = "y".repeat(200);

        terminal.draw(|f| {
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
                    cell_selection: None,
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
        });

        let buffer = terminal.buffer();

        // Find the first user-message text row. Layout (60-col terminal):
        //   cols 0,1  = global app_bg (viewport margin)
        //   cols 2,3  = user_panel_bg inner pad (USER_MESSAGE_TEXT_GAP_COLS)
        //   col  4+   = text
        let user_row = (0..buffer.area().height)
            .find(|&y| {
                let c4 = &buffer[(4, y)];
                c4.symbol() == "x" && c4.bg == user_bg
            })
            .expect("user message row exists");

        // Left: 2-col app_bg outer gutter (viewport margin + entry inset),
        // then 2-col user_panel_bg inner pad.
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
        // user_text_width = (band_w) - (TEXT_GAP + RIGHT_PAD) = (60-4) - 4 = 52
        // -> text fills cols 4..56.
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
        let composer_row = (0..buffer.area().height)
            .find(|&y| {
                let c4 = &buffer[(4, y)];
                c4.symbol() == "y" && c4.bg == input_bg
            })
            .expect("composer row exists");
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
        let theme = Theme::default();
        let queued_bg = theme.user_surface_queued();
        let delivered_bg = theme.user_surface();
        let width = 40u16;
        let mut terminal = neenee_tui::TestTerminal::new(width, 12);

        let messages = vec![
            TranscriptMessage::new(neenee_core::Role::User, "first queued").queued(),
            TranscriptMessage::new(neenee_core::Role::User, "second queued").queued(),
        ];

        terminal.draw(|f| {
            let mut layout_map = LayoutMap::new();
            let _ = draw_transcript(
                f,
                &mut layout_map,
                TranscriptView {
                    messages: &messages,
                    scroll: 0,
                    selection: &SelectionState::None,
                    cell_selection: None,
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
        });

        let buffer = terminal.buffer();

        // Both queued panels must carry the queued bg, never the delivered bg.
        // Scan the inner-pad columns (2,3) of every row for any cell painted
        // with the delivered bg — that would mean a queued message leaked the
        // wrong surface.
        for y in 0..buffer.area().height {
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
        let badge_count = (0..buffer.area().height)
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
        let theme = Theme::default();
        let width = 40u16;
        let mut terminal = neenee_tui::TestTerminal::new(width, 24);
        // Two assistant text messages so a `MESSAGE_GAP_ROWS` blank row is
        // emitted between them — that row is rendered but never registered.
        let messages = vec![
            TranscriptMessage::new(neenee_core::Role::Assistant, "first".to_string()),
            TranscriptMessage::new(neenee_core::Role::Assistant, "second".to_string()),
        ];
        let mut layout_map = LayoutMap::new();
        terminal.draw(|f| {
            draw_transcript(
                f,
                &mut layout_map,
                TranscriptView {
                    messages: &messages,
                    scroll: 0,
                    selection: &SelectionState::None,
                    cell_selection: None,
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
        });

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
            "Tool".to_string(),
            "Type".to_string(),
            "Implementation".to_string(),
            "Key Feature".to_string(),
        ];
        let rows = vec![
            vec![
                "bash".to_string(),
                "Write".to_string(),
                "std::process::Command (sh -c / cmd /C)".to_string(),
                "execute shell command, supports timeout, truncates output".to_string(),
            ],
            vec![
                "read_file".to_string(),
                "Read".to_string(),
                "std::fs::read_to_string".to_string(),
                "supports offset/limit".to_string(),
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

    /// Ragged body rows (fewer cells than the header, and more) must not panic
    /// the adaptive renderer and must still produce a rectangular grid: every
    /// data line carries the same number of `│` column separators. Regression
    /// test for the `index out of bounds: the len is 1 but the index is 1`
    /// panic at `markdown_table.rs` (`cell_styles[i]`) caused by a body row
    /// with a single cell in a two-column table.
    #[test]
    fn table_render_handles_ragged_rows_without_panicking() {
        use crate::tui::document::TableAlignment;

        let headers = vec!["A".to_string(), "B".to_string()];
        // 0, 1, 2, and 3 cells — exercises both the under- and over-wide paths.
        let rows = vec![
            vec![],
            vec!["only".to_string()],
            vec!["x".to_string(), "y".to_string()],
            vec!["p".to_string(), "q".to_string(), "r".to_string()],
        ];
        let aligns = vec![TableAlignment::None, TableAlignment::None];

        let table = build_table_render(&headers, &rows, &aligns, 40);
        assert!(!table.lines.is_empty(), "ragged table must still render");

        // Every data line must have the same number of column separators, i.e.
        // the grid stays rectangular regardless of input raggedness.
        let pipe_counts: Vec<usize> = table
            .lines
            .iter()
            .filter(|l| l.starts_with('│'))
            .map(|l| l.matches('│').count())
            .collect();
        assert!(!pipe_counts.is_empty(), "must have data lines");
        assert!(
            pipe_counts.iter().all(|&c| c == pipe_counts[0]),
            "ragged rows produced uneven column counts: {pipe_counts:?}"
        );

        // Every data line carries per-cell geometry for exactly `ncols` cells,
        // so hit-testing / selection never indexes out of bounds.
        for info in table.line_info.iter().flatten() {
            assert_eq!(
                info.col_spans.len(),
                2,
                "each data line must describe exactly 2 cells"
            );
        }
    }

    /// Inline-code / bold markup delimiters (`` ` ``, `**`) are rendered at zero
    /// width, so a column holding markup must be sized and wrapped by its
    /// *visible* width — otherwise the column is inflated, the wrapped text can
    /// split a `` `…` ``/`**…**` pair across lines, and data-row `│` separators
    /// drift out of line with the border grid. A plain table and a markup table
    /// carrying the same visible content must therefore share identical borders
    /// and the same line count (no spurious wrap).
    #[test]
    fn table_markup_columns_size_to_visible_width() {
        use crate::tui::document::TableAlignment;

        let plain = build_table_render(
            &["a".to_string(), "b".to_string()],
            &[vec!["bold".to_string(), "code".to_string()]],
            &[TableAlignment::None, TableAlignment::None],
            80,
        );
        let markup = build_table_render(
            &["a".to_string(), "b".to_string()],
            &[vec!["**bold**".to_string(), "`code`".to_string()]],
            &[TableAlignment::None, TableAlignment::None],
            80,
        );

        // Borders are markup-free, so plain and markup grids must match exactly
        // once columns are sized to visible width.
        let plain_borders: Vec<&String> =
            plain.lines.iter().filter(|l| !l.starts_with('│')).collect();
        let markup_borders: Vec<&String> = markup
            .lines
            .iter()
            .filter(|l| !l.starts_with('│'))
            .collect();
        assert_eq!(
            plain_borders, markup_borders,
            "markup must not inflate column width"
        );

        // The markup cell fits its column on a single line (no delimiter split):
        // same number of data lines as the plain version.
        let plain_data = plain.lines.iter().filter(|l| l.starts_with('│')).count();
        let markup_data = markup.lines.iter().filter(|l| l.starts_with('│')).count();
        assert_eq!(
            plain_data, markup_data,
            "markup must not introduce extra wrapped lines"
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
            let mut terminal = neenee_tui::TestTerminal::new(80, 24);
            let mut ranked = crate::tui::fuzzy::rank(&history, query);
            crate::tui::fuzzy::sort_by_score(&mut ranked);
            assert_eq!(
                ranked.len(),
                *expected_matches,
                "query {:?} should surface {} entries",
                query,
                expected_matches
            );
            terminal.draw(|f| {
                draw_history_modal(
                    f,
                    &mut LayoutMap::new(),
                    &history,
                    query,
                    query.chars().count(),
                    &ranked,
                    0,
                    &mut 0,
                    true,
                    false,
                    true,
                    &theme,
                );
            });
        }

        // Empty history must render the "(no history yet)" placeholder rather
        // than indexing into an empty slice.
        let mut terminal = neenee_tui::TestTerminal::new(80, 24);
        let empty: Vec<String> = Vec::new();
        let ranked: Vec<(usize, crate::tui::fuzzy::FuzzyMatch)> =
            crate::tui::fuzzy::rank(&empty, "");
        terminal.draw(|f| {
            draw_history_modal(
                f,
                &mut LayoutMap::new(),
                &empty,
                "",
                0,
                &ranked,
                0,
                &mut 0,
                true,
                false,
                true,
                &theme,
            );
        });
    }

    /// Browse mode (`search = false`) renders the plain list with the
    /// `/ to search` hint and no query field — the default state when the
    /// Ctrl+R modal first opens.
    #[test]
    fn history_modal_browse_mode_shows_search_hint() {
        let theme = Theme::default();
        let history = vec!["git status".to_string(), "cargo test".to_string()];
        // Browse rows are newest-first with empty (unhighlighted) matches.
        let ranked: Vec<(usize, crate::tui::fuzzy::FuzzyMatch)> = (0..history.len())
            .rev()
            .map(|i| {
                (
                    i,
                    crate::tui::fuzzy::FuzzyMatch {
                        score: 0,
                        positions: Vec::new(),
                    },
                )
            })
            .collect();
        let mut terminal = neenee_tui::TestTerminal::new(80, 24);
        terminal.draw(|f| {
            draw_history_modal(
                f,
                &mut LayoutMap::new(),
                &history,
                "",
                0,
                &ranked,
                0,
                &mut 0,
                true,
                false,
                false, // browse mode
                &theme,
            );
        });
        let buf = terminal.buffer();
        let screen: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(
            screen.contains("/ to search"),
            "browse header should advertise the search shortcut"
        );
    }

    /// A multi-line history entry collapses to its first line in the fuzzy
    /// list (so a long prompt never breaks the single-row grid), and the
    /// preview mode renders the full text verbatim. Both modes must consume a
    /// real buffer without panicking.
    #[test]
    fn history_modal_folds_multiline_and_previews_full_text() {
        let theme = Theme::default();
        let history = vec![
            "first line\nsecond line\nthird line".to_string(),
            "single line".to_string(),
        ];

        let mut terminal = neenee_tui::TestTerminal::new(80, 24);
        let ranked = crate::tui::fuzzy::rank(&history, "");

        // List mode: the multi-line entry must render as one row.
        terminal.draw(|f| {
            draw_history_modal(
                f,
                &mut LayoutMap::new(),
                &history,
                "",
                0,
                &ranked,
                0,
                &mut 0,
                true,
                false,
                true,
                &theme,
            );
        });
        let buf = terminal.buffer();
        // The continuation marker `↵` should appear somewhere — proving the
        // folded entry advertises its hidden content.
        let has_marker = buf.content.iter().any(|c| c.symbol() == "↵");
        assert!(has_marker, "multi-line entry should show the ↵ fold marker");

        // Preview mode: the full multi-line text renders without panic.
        terminal.draw(|f| {
            draw_history_modal(
                f,
                &mut LayoutMap::new(),
                &history,
                "",
                0,
                &ranked,
                0,
                &mut 0,
                true,
                true,
                true,
                &theme,
            );
        });
    }

    /// With no messages, `draw_transcript` renders the empty-state hero in
    /// place of the stream: `content_lines` is non-zero (so the app loop does
    /// not treat it as a zero-height stream) and the call does not panic.
    #[test]
    fn empty_session_renders_empty_state_with_nonzero_height() {
        let theme = Theme::default();
        let mut terminal = neenee_tui::TestTerminal::new(80, 24);
        let messages: Vec<TranscriptMessage> = Vec::new();

        let mut render_opt: Option<TranscriptRender> = None;
        terminal.draw(|f| {
            let mut layout_map = LayoutMap::new();
            render_opt = Some(draw_transcript(
                f,
                &mut layout_map,
                TranscriptView {
                    messages: &messages,
                    scroll: 0,
                    selection: &SelectionState::None,
                    cell_selection: None,
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
        });
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
        let theme = Theme::default();
        let mut terminal = neenee_tui::TestTerminal::new(80, 24);
        let messages = vec![TranscriptMessage::new(neenee_core::Role::User, "hello")];

        let mut render_opt: Option<TranscriptRender> = None;
        terminal.draw(|f| {
            let mut layout_map = LayoutMap::new();
            render_opt = Some(draw_transcript(
                f,
                &mut layout_map,
                TranscriptView {
                    messages: &messages,
                    scroll: 0,
                    selection: &SelectionState::None,
                    cell_selection: None,
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
        });
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
        let theme = Theme::default();
        let mut terminal = neenee_tui::TestTerminal::new(80, 24);
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
        terminal.draw(|f| {
            let mut layout_map = LayoutMap::new();
            render_opt = Some(draw_transcript(
                f,
                &mut layout_map,
                TranscriptView {
                    messages: &messages,
                    scroll: 0,
                    selection: &SelectionState::None,
                    cell_selection: None,
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
        });
        let render = render_opt.expect("draw_transcript must return a render");

        // 4 logo lines + 1 blank gap + 1 tagline = 6 content lines.
        assert_eq!(
            render.content_lines, 6,
            "user-logo content_lines must be logo rows + gap + tagline"
        );
    }

    /// An H1 heading renders with an UNDERLINED modifier. The underline must
    /// cover only the prefix + text cells and must not bleed into the trailing
    /// whitespace of the heading row. Inspects the rendered grid cells
    /// directly to pin the clamp in `draw_message_body`.
    #[test]
    fn h1_underline_clamps_to_text_extent() {
        let theme = Theme::default();
        let mut terminal = neenee_tui::TestTerminal::new(60, 12);
        let messages = vec![TranscriptMessage::new(
            neenee_core::Role::Assistant,
            "# QQ_H1_TEST\n\nbody text here\n",
        )];
        terminal.draw(|f| {
            let _ = draw_transcript(
                f,
                &mut LayoutMap::new(),
                TranscriptView {
                    messages: &messages,
                    scroll: 0,
                    selection: &SelectionState::None,
                    cell_selection: None,
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
        });
        let buffer = terminal.buffer();
        let width = buffer.area().width;
        let underline = neenee_tui::Modifier::UNDERLINE;

        let mut head = None;
        'outer: for y in 0..buffer.area().height {
            for x in 0..width {
                if buffer[(x, y)].symbol() == "Q" {
                    head = Some((x, y));
                    break 'outer;
                }
            }
        }
        let (hx, hy) = head.expect("heading 'Q' cell exists");

        // "QQ_H1_TEST" is 10 cells; prefix is 3 cells. All 13 are underlined.
        for x in hx..hx + 10 {
            assert!(
                buffer[(x, hy)].style.add.contains(underline),
                "heading text cell at x={x} must be UNDERLINED"
            );
        }
        let trailing = hx + 10;
        assert!(trailing < width, "trailing cell within grid");
        assert!(
            !buffer[(trailing, hy)].style.add.contains(underline),
            "underline must not bleed into trailing whitespace at x={trailing}"
        );
        assert!(
            !buffer[(width - 1, hy)].style.add.contains(underline),
            "underline must not reach the right edge"
        );
    }

    /// Same clamp check with a multi-codepoint emoji grapheme (ZWJ family) in
    /// the heading: `wrap_text` measures per-char (overcounting the sequence)
    /// while the grid renders per-grapheme, so this guards the underline width
    /// against the char-vs-grapheme measurement split.
    #[test]
    fn h1_underline_clamps_with_emoji_grapheme() {
        let theme = Theme::default();
        let mut terminal = neenee_tui::TestTerminal::new(60, 12);
        let messages = vec![TranscriptMessage::new(
            neenee_core::Role::Assistant,
            "# 👨‍👩‍👧 OKX\n\nbody\n",
        )];
        terminal.draw(|f| {
            let _ = draw_transcript(
                f,
                &mut LayoutMap::new(),
                TranscriptView {
                    messages: &messages,
                    scroll: 0,
                    selection: &SelectionState::None,
                    cell_selection: None,
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
        });
        let buffer = terminal.buffer();
        let width = buffer.area().width;
        let underline = neenee_tui::Modifier::UNDERLINE;

        let mut x_pos = None;
        'outer: for y in 0..buffer.area().height {
            for x in 0..width {
                if buffer[(x, y)].symbol() == "X" {
                    x_pos = Some((x, y));
                    break 'outer;
                }
            }
        }
        let (xx, xy) = x_pos.expect("heading 'X' cell exists");

        assert!(
            buffer[(xx, xy)].style.add.contains(underline),
            "heading 'X' text cell must be UNDERLINED"
        );
        let trailing = xx + 1;
        assert!(trailing < width, "trailing cell within grid");
        assert!(
            !buffer[(trailing, xy)].style.add.contains(underline),
            "underline must not bleed past emoji heading at x={trailing}"
        );
    }

    /// A wide (emoji) glyph in an H1 heading occupies a head cell plus a
    /// wide-continuation cell. The grid stores the continuation without the
    /// `add` modifiers (it is a non-emitted placeholder), but the diff skips
    /// continuations and emits the head's run style — so the backend prints
    /// the wide glyph while the UNDERLINED SGR is active, underlining both
    /// columns. This pins that emitted behavior at the `Draw`-command layer.
    #[test]
    fn h1_underline_emits_wide_glyph_in_underlined_run() {
        let theme = Theme::default();
        let width = 60u16;
        let mut terminal = neenee_tui::TestTerminal::new(width, 12);
        let messages = vec![TranscriptMessage::new(
            neenee_core::Role::Assistant,
            "# Hello😀\n\nbody\n",
        )];
        terminal.draw(|f| {
            let _ = draw_transcript(
                f,
                &mut LayoutMap::new(),
                TranscriptView {
                    messages: &messages,
                    scroll: 0,
                    selection: &SelectionState::None,
                    cell_selection: None,
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
        });
        let back = terminal.buffer();
        let front = neenee_tui::Grid::new(width, 12);
        let cmd = neenee_tui::diff::diff(back, &front);
        let underline = neenee_tui::Modifier::UNDERLINE;

        let wide_run_style = cmd.draws.iter().find_map(|d| match d {
            neenee_tui::Draw::Cells { style, cells, .. } => cells
                .iter()
                .any(|(sym, w)| sym == "😀" && *w == 2)
                .then_some(*style),
            _ => None,
        });
        let style =
            wide_run_style.expect("a Draw::Cells run containing wide glyph '😀' must be emitted");
        assert!(
            style.add.contains(underline),
            "wide glyph '😀' must be emitted in an UNDERLINED run so the terminal \
             underlines both columns, got add={:?}",
            style.add,
        );
    }

    /// Regression: a long H1 heading that wraps to multiple lines. The heading
    /// *prefix* (the leading indent on row 0 and the continuation indent
    /// on rows 1+) is decoration, not heading text, so it must NOT carry the
    /// UNDERLINED modifier. Previously the prefix shared the UNDERLINED style,
    /// which underlined the leading whitespace of every wrapped row — the
    /// underline appeared to "cross the line head" and cover the blank indent.
    ///
    /// We render a heading that wraps to ≥2 rows and assert that, on every
    /// row, the underline begins exactly at the text column (prefix width) and
    /// that the indent columns themselves are never underlined. The trailing
    /// blank columns must also stay un-underlined (the existing clamp).
    #[test]
    fn h1_underline_excludes_prefix_indent_on_wrapped_rows() {
        let theme = Theme::default();
        let mut terminal = neenee_tui::TestTerminal::new(20, 16);
        let messages = vec![TranscriptMessage::new(
            neenee_core::Role::Assistant,
            "# This is a very long heading that wraps to multiple lines\n\nbody\n",
        )];
        terminal.draw(|f| {
            let _ = draw_transcript(
                f,
                &mut LayoutMap::new(),
                TranscriptView {
                    messages: &messages,
                    scroll: 0,
                    selection: &SelectionState::None,
                    cell_selection: None,
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
        });
        let buffer = terminal.buffer();
        let width = buffer.area().width;
        let underline = neenee_tui::Modifier::UNDERLINE;

        // The heading prefix is "   " (3 columns); locate the heading's rows
        // as the contiguous non-blank rows at the top (before the blank gap +
        // body). The heading "This is a very long heading that wraps to
        // multiple lines" wraps to several rows here.
        let mut heading_rows: Vec<u16> = Vec::new();
        let mut found_body = false;
        for y in 0..buffer.area().height {
            let row_has_text = (0..width).any(|x| buffer[(x, y)].symbol() != " ");
            if !row_has_text {
                if !heading_rows.is_empty() {
                    found_body = true;
                }
                continue;
            }
            if found_body {
                break;
            }
            heading_rows.push(y);
        }
        assert!(
            heading_rows.len() >= 2,
            "heading must wrap to at least 2 rows, got {}",
            heading_rows.len()
        );

        for &y in &heading_rows {
            // Indent columns [0, text_start) must never be underlined.
            // The heading prefix is `TRANSCRIPT_BODY_LEADING_INDENT` cols
            // (matching body prose — see the `Block::Heading` arm), applied
            // inside the already-inset band: entry inset (TRANSCRIPT_H_INSET)
            // + heading prefix (TRANSCRIPT_BODY_LEADING_INDENT). Text starts
            // at col `TRANSCRIPT_H_INSET + TRANSCRIPT_BODY_LEADING_INDENT`.
            let text_start = super::TRANSCRIPT_H_INSET + super::TRANSCRIPT_BODY_LEADING_INDENT;
            for x in 0..text_start {
                let cell = &buffer[(x, y)];
                assert!(
                    !cell.style.add.contains(underline),
                    "indent cell at (x={x}, y={y}) must NOT be underlined \
                     (it is heading decoration, not text), symbol={:?}",
                    cell.symbol(),
                );
            }
            // The trailing blank tail (rightmost column) must not be underlined.
            let last = width - 1;
            assert!(
                !buffer[(last, y)].style.add.contains(underline),
                "trailing cell at (x={last}, y={y}) must NOT be underlined"
            );
            // And at least the first text column must be underlined (the
            // heading text itself is still underlined).
            let first_text_cell = &buffer[(text_start, y)];
            assert!(
                first_text_cell.style.add.contains(underline),
                "first heading-text cell at (x={text_start}, y={y}) must be UNDERLINED, \
                 symbol={:?}",
                first_text_cell.symbol(),
            );
        }
    }
}

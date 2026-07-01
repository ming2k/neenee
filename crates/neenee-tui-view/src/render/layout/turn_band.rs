//! Option C: round-banded layout. Each tool round is grouped into a labelled
//! band with a header row (`round N · model · K calls`), so the history reads
//! as discrete model-request chunks instead of one flush stream.
//!
//! ## Grouping model
//! A "round group" is a maximal run of consecutive assistant-side messages
//! (tool steps, reasoning traces, envoy tasks, assistant text) that share the
//! same `round` stamp. User messages and notices are *not* grouped — they
//! flow as Compact does, with normal gaps, and act as group *terminators*:
//! a group never spans across a user turn.
//!
//! Tool steps carry a `round: Option<u64>` (1-indexed, stamped from the
//! harness). Reasoning traces and assistant text in the same turn share it
//! when stamped; when a message's round is `None` (legacy / restored sessions
//! predating the stamp) it falls back to being rendered flush like Compact,
//! without a band, so old transcripts stay readable.
//!
//! ## Visual form
//! Each group with a *known* round gets, immediately before its first
//! message, a single-line header (with one blank row above it):
//!
//! ```text
//! ◆ round 2 · sonnet · 3 calls
//! ```
//!
//! rendered in an info-tone bold for the `◆ round N` anchor and muted for the
//! rest, using foreground color only — no background band. This keeps the
//! layout cheap (no per-cell background fill across the group's body, which
//! would require repaint coordination with every drawer) while giving each
//! round a clear, labelled anchor.

use neenee_tui::{Line, Modifier, Paragraph, Rect, Span, Style};

use crate::document::TranscriptMessage;

use super::{Stream, TranscriptLayout};

/// Round-banded layout. See module docs.
#[derive(Default)]
pub struct TurnBand;

impl TranscriptLayout for TurnBand {
    fn run(&mut self, stream: &mut Stream<'_, '_>) {
        let messages_len = stream.messages.len();
        let mut mi = 0;

        while mi < messages_len {
            let msg = &stream.messages[mi];

            // ── Detect the start of a round group ───────────────────────────
            // A group starts at an assistant-side message with a *known* round,
            // provided it is either the first message or the previous message
            // is not part of the same round (different round / user / notice).
            let group_round = round_of(msg);
            if msg.is_tool_step()
                && let Some(group_round) = group_round
                && is_group_start(stream, mi)
            {
                // Measure the group: how many consecutive same-round
                // assistant-side messages follow.
                let group_end = group_end(stream, mi);
                let calls = count_tool_calls(stream, mi, group_end);

                // One blank row above the header. The preceding message (a
                // different round / user / notice) already emits its own
                // trailing separator row via the non-grouped branch below, so
                // that row *is* this gap — we must not add a second one here,
                // or the header ends up with two blank rows above it. The only
                // case with no preceding separator is the very first message
                // (mi == 0); there we add a single leading row so the first
                // round header isn't flush against the top edge.
                if mi == 0 {
                    stream.message_gap();
                }

                draw_round_header(stream, group_round, msg, calls);

                // Blank row below the header separating it from the round's
                // first step.
                stream.message_gap();

                // Render the group's messages flush (no inter-message gaps), so
                // the round reads as one compact chunk. Tool steps inside a
                // group stack flush exactly like Compact's collapsed rule, but
                // unconditionally here (expanded steps still get their own body
                // via dispatch — the gap suppression only affects the blank
                // *separator* row, not the body).
                for gj in mi..group_end {
                    stream.badge(gj);
                    stream.dispatch(gj);
                    // No inter-message gap inside a group: flush stack.
                }

                // One blank row separates this group from whatever follows.
                stream.message_gap();

                mi = group_end;
                continue;
            }

            // ── Non-grouped message: Compact behavior ───────────────────────
            stream.badge(mi);
            stream.dispatch(mi);

            let next = stream.messages.get(mi + 1);
            let next_is_tool_step = next.is_some_and(|n| n.is_tool_step() || n.is_envoy_task());
            let collapsed_tool_into_tool_step =
                msg.is_tool_step() && msg.tool_step_expanded() == Some(false) && next_is_tool_step;
            let next_is_step =
                next.is_some_and(|n| n.is_thinking() || n.is_tool_step() || n.is_envoy_task());

            if collapsed_tool_into_tool_step {
                // Flush stack.
            } else if msg.role != neenee_core::Role::User || next_is_step {
                stream.message_gap();
            }

            mi += 1;
        }
    }
}

/// The round stamp of a message, or `None` if unknown.
fn round_of(msg: &TranscriptMessage) -> Option<u64> {
    msg.turn
}

/// Is `mi` the start of a new round group? True when the previous message does
/// not belong to the same round: i.e. there is no previous message, or the
/// previous one is user/notice, or it carries a different round stamp.
fn is_group_start(stream: &Stream<'_, '_>, mi: usize) -> bool {
    if mi == 0 {
        return true;
    }
    let prev = &stream.messages[mi - 1];
    if prev.role == neenee_core::Role::User || prev.is_notice() {
        return true;
    }
    round_of(prev) != round_of(&stream.messages[mi])
}

/// Exclusive end index of the round group starting at `start`: the first index
/// that is not an assistant-side message in the same round.
fn group_end(stream: &Stream<'_, '_>, start: usize) -> usize {
    let target_round = round_of(&stream.messages[start]);
    let mut end = start;
    while end < stream.messages.len() {
        let m = &stream.messages[end];
        if m.role == neenee_core::Role::User
            || m.is_notice()
            || round_of(m) != target_round
            || !(m.is_tool_step() || m.is_envoy_task() || m.is_thinking())
        {
            break;
        }
        end += 1;
    }
    end
}

/// Count the tool-call steps (tool steps + envoy tasks) in `[start, end)`.
fn count_tool_calls(stream: &Stream<'_, '_>, start: usize, end: usize) -> usize {
    stream.messages[start..end]
        .iter()
        .filter(|m| m.is_tool_step() || m.is_envoy_task())
        .count()
}

/// Paint the round header row: `◆ round N · model · K calls`, info-tone bold
/// anchor with muted metadata, no background band. One blank row sits above it
/// (added in the caller).
fn draw_round_header(
    stream: &mut Stream<'_, '_>,
    round: u64,
    msg: &TranscriptMessage,
    calls: usize,
) {
    // Always account for one content line even when scrolled out of view, so
    // scroll height stays faithful to what a user scrolling back would see.
    stream.content_lines += 1;
    if stream.skip_rows > 0 {
        stream.skip_rows -= 1;
        return;
    }
    if stream.current_y >= stream.viewport_bottom() {
        return;
    }

    let theme = stream.theme;
    let band = stream.band;

    // Two-tone label, no background band: `◆ round N` is the info-tone
    // anchor, the rest (model, call count) reads as muted metadata on the
    // same line.
    let accent = Style::default()
        .fg(theme.info())
        .add_modifier(Modifier::BOLD);
    let meta = Style::default().fg(theme.muted());

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(8);
    spans.push(Span::styled("◆ ", accent));
    spans.push(Span::styled(format!("round {}", round), accent));

    let model_name = msg
        .model
        .as_deref()
        .filter(|m| !m.is_empty())
        .map(crate::providers::model_display_name);
    if let Some(name) = &model_name {
        spans.push(Span::styled(format!(" · {}", name), meta));
    }
    let calls_seg = format!(" · {} {}", calls, if calls == 1 { "call" } else { "calls" });
    spans.push(Span::styled(calls_seg, meta));

    let line = Line::from(spans);
    let rect = Rect::new(band.x, stream.current_y, band.width, 1);
    stream.frame.render_widget(Paragraph::new(line), rect);
    stream.current_y += 1;
}

//! The original transcript layout: messages flush against each other with
//! single-row gaps, and adjacent collapsed tool steps stack with no gap at all.
//!
//! This is a verbatim extraction of the message loop that lived in
//! `draw_transcript` before the `layout` split. Behavior is byte-for-byte
//! identical to the pre-refactor renderer; it is the reference strategy and
//! the default.

use super::{Stream, TranscriptLayout};

/// Original flush-stack layout. See the module docs.
pub struct Compact;

impl TranscriptLayout for Compact {
    fn run(&mut self, stream: &mut Stream<'_, '_>) {
        let messages_len = stream.messages.len();
        for mi in 0..messages_len {
            let msg = &stream.messages[mi];

            // Model attribution badge above the first assistant-side message of
            // a turn / on model change.
            stream.badge(mi);

            // Per-kind drawer (height-cache fast path included).
            stream.dispatch(mi);

            // ── Inter-message spacing ───────────────────────────────────────
            // A user message's panel already ends with a bottom transition row
            // (▀) that separates it from the next message, so the extra blank
            // line is omitted there to keep the gap to a single row. The
            // exception is when the next message is a step (thinking or tool
            // step): a blank row between the user panel's transition and the
            // step header keeps the two visually distinct.
            //
            // Collapsed tool steps stack flush: a batch of parallel/sequential
            // collapsed tool-call headers forms a compact log block with no
            // blank rows between them. The separating row is supplied *only*
            // by an expanded step's body.
            let next = stream.messages.get(mi + 1);
            let next_is_tool_step = next.is_some_and(|n| n.is_tool_step() || n.is_envoy_task());
            let collapsed_tool_into_tool_step =
                msg.is_tool_step() && msg.tool_step_expanded() == Some(false) && next_is_tool_step;
            let next_is_step =
                next.is_some_and(|n| n.is_thinking() || n.is_tool_step() || n.is_envoy_task());

            if collapsed_tool_into_tool_step {
                // Flush stack: no separating row.
            } else if msg.role != neenee_core::Role::User || next_is_step {
                stream.message_gap();
            }
        }
    }
}

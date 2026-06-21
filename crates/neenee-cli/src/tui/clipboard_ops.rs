//! Async clipboard plumbing for the event loop. Copies and pastes run in
//! spawned tasks so a stuck system clipboard (arboard / wl-copy / wl-paste)
//! can never freeze the TUI's event poll; results flow back through channels
//! and are applied on the next frame.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use neenee_core::ImagePart;

use crate::tui::clipboard::{self, ClipboardRead, CopyOutcome};
use crate::tui::composer_attachments::{
    image_chip, paste_chip, paste_line_count, should_chip_paste,
};
use crate::tui::{App, Modal};

/// Bound on each clipboard operation. A stuck reader must never freeze the
/// event loop's poll cadence.
const CLIP_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) fn spawn_clipboard_copy(
    tx: &mpsc::UnboundedSender<Result<CopyOutcome, String>>,
    copy_pending: Arc<AtomicUsize>,
    text: String,
) {
    let tx = tx.clone();
    copy_pending.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        let result = match tokio::time::timeout(CLIP_TIMEOUT, clipboard::copy(&text)).await {
            Ok(inner) => inner,
            Err(_) => Err("clipboard copy timed out".to_string()),
        };
        let _ = tx.send(result);
        copy_pending.fetch_sub(1, Ordering::SeqCst);
    });
}

/// Read the system clipboard in a background task and deliver the result to
/// the event loop. Bounded by a timeout so a stuck clipboard reader can never
/// freeze paste feedback.
pub(super) fn spawn_clipboard_paste(tx: &mpsc::UnboundedSender<ClipboardRead>) {
    let tx = tx.clone();
    tokio::spawn(async move {
        let read = match tokio::time::timeout(CLIP_TIMEOUT, clipboard::read()).await {
            Ok(inner) => inner,
            Err(_) => ClipboardRead::Empty,
        };
        let _ = tx.send(read);
    });
}

/// Apply a completed clipboard paste: attach an image, insert text at the
/// cursor, or surface an error toast.
pub(super) fn apply_clipboard_paste(app: &mut App, read: ClipboardRead) {
    if app.active_modal != Modal::None {
        return;
    }
    match read {
        ClipboardRead::Image { data, mime } => {
            let encoded = clipboard::base64_image(&data);
            app.pending_images.push(ImagePart {
                mime,
                data: encoded,
            });
            // Insert a short `[Image #N]` chip at the cursor so the user has
            // a visible, atomic affordance for the staged attachment — the
            // chip is what they backspace to undo the paste. The trailing
            // space keeps the cursor on a word boundary so typing resumes
            // naturally.
            let n = app.pending_images.len();
            insert_chip_at_cursor(app, &image_chip(n));
            app.copy_toast_message = format!(
                "{n} image{} attached — enter to send",
                if n == 1 { "" } else { "s" }
            );
            app.copy_toast_failed = false;
            app.copy_toast_until = Some(std::time::Instant::now() + Duration::from_millis(1800));
        }
        ClipboardRead::Text(text) => {
            // Large pastes (multi-line or long enough to balloon the input
            // box) are staged behind a `[Pasted text #N +M lines]` chip
            // instead of being inlined verbatim. Short snippets keep flowing
            // through the cursor like an ordinary editor paste.
            if should_chip_paste(&text) {
                let n = app.pending_text_pastes.len() + 1;
                let line_count = paste_line_count(&text);
                app.pending_text_pastes.push(text);
                insert_chip_at_cursor(app, &paste_chip(n, line_count));
                app.copy_toast_message = format!(
                    "pasted {line_count} line{} as a chip",
                    if line_count == 1 { "" } else { "s" }
                );
            } else {
                let chars_to_insert = text.chars().count();
                let byte_pos = app
                    .input
                    .char_indices()
                    .map(|(i, _)| i)
                    .nth(app.cursor_position)
                    .unwrap_or(app.input.len());
                app.input.insert_str(byte_pos, &text);
                app.cursor_position += chars_to_insert;
                app.copy_toast_message = format!(
                    "pasted {chars_to_insert} char{}",
                    if chars_to_insert == 1 { "" } else { "s" }
                );
            }
            app.copy_toast_failed = false;
            app.copy_toast_until = Some(std::time::Instant::now() + Duration::from_millis(1200));
        }
        ClipboardRead::Empty => {
            app.copy_toast_message = "clipboard is empty".to_string();
            app.copy_toast_failed = true;
            app.copy_toast_until = Some(std::time::Instant::now() + Duration::from_millis(1200));
        }
    }
}

/// Splice `chip` followed by a single space into [`App::input`] at the
/// cursor, advancing the cursor past both. Shared by the image and large-
/// text paste paths so the chip's surrounding whitespace stays consistent —
/// the trailing space is what lets the chip-aware Backspace erase the whole
/// paste in one keystroke.
fn insert_chip_at_cursor(app: &mut App, chip: &str) {
    let byte_pos = app
        .input
        .char_indices()
        .map(|(i, _)| i)
        .nth(app.cursor_position)
        .unwrap_or(app.input.len());
    let mut spliced = String::with_capacity(chip.len() + 1);
    spliced.push_str(chip);
    spliced.push(' ');
    let extra_chars = spliced.chars().count();
    app.input.insert_str(byte_pos, &spliced);
    app.cursor_position += extra_chars;
}

pub(super) fn set_copy_feedback(app: &mut App, result: Result<CopyOutcome, String>) {
    match result {
        Ok(CopyOutcome::Native) => {
            app.copy_toast_message = "copied to clipboard".to_string();
            app.copy_toast_failed = false;
        }
        Ok(CopyOutcome::Osc52) => {
            app.copy_toast_message = "copy sent via OSC52".to_string();
            app.copy_toast_failed = false;
        }
        Err(error) => {
            let mut chars = error.chars();
            let prefix = chars.by_ref().take(48).collect::<String>();
            app.copy_toast_message = if chars.next().is_some() {
                format!("copy failed: {}...", prefix)
            } else {
                format!("copy failed: {}", prefix)
            };
            app.copy_toast_failed = true;
        }
    }
}

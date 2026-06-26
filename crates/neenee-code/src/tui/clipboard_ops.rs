//! Async clipboard plumbing for the event loop. Copies and pastes run in
//! spawned tasks so a stuck system clipboard (arboard / wl-copy / wl-paste)
//! can never freeze the TUI's event poll; results flow back through channels
//! and are applied on the next frame.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use neenee_core::{ImagePart, resolve_model};

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
///
/// On the main prompt (`Modal::None`) a paste follows the chip-or-inline
/// composer semantics — images stage as `[Image #N]` attachments and large
/// text blocks collapse into `[Pasted text #N +M lines]` chips. Inside a
/// free-text modal (provider editor, provider picker filter, history
/// search) the input line is borrowed as a single-line field, so the paste
/// splices the text inline at the cursor with newlines stripped (matching
/// `insert_newline` being a no-op in modals) and skips the chip / attachment
/// machinery entirely. Other modals drop the paste silently.
pub(super) fn apply_clipboard_paste(app: &mut App, read: ClipboardRead) {
    match app.active_modal {
        Modal::None => apply_composer_paste(app, read),
        Modal::HistorySearch | Modal::Provider | Modal::ModelEditor => {
            apply_modal_field_paste(app, read)
        }
        _ => {}
    }
}

/// Main-prompt paste: chips for images and large text blocks, inline insert
/// with a toast for short snippets. See [`apply_clipboard_paste`].
fn apply_composer_paste(app: &mut App, read: ClipboardRead) {
    match read {
        ClipboardRead::Image { data, mime } => {
            // If the current model doesn't support vision, reject the image
            // paste with a toast rather than silently dropping it — the user
            // should know why their paste didn't take.
            if !resolve_model(&app.current_model).vision {
                app.copy_toast_message = format!(
                    "{} does not support images — paste ignored",
                    app.current_model,
                );
                app.copy_toast_failed = true;
                app.copy_toast_until =
                    Some(std::time::Instant::now() + Duration::from_millis(2000));
                return;
            }
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

/// Modal-field paste: splice text inline at the cursor, stripping newlines
/// to preserve single-line semantics (the provider editor's API-key and
/// model-id fields, the picker filter, and the history search query are all
/// single-line). Image pastes are dropped with a short toast since the
/// modal field has no attachment staging. See [`apply_clipboard_paste`].
fn apply_modal_field_paste(app: &mut App, read: ClipboardRead) {
    match read {
        ClipboardRead::Text(text) => {
            // Collapse any newlines (and trailing carriage returns) so a
            // copied multi-line block pastes as one continuous line, matching
            // the single-line editing the modal fields already enforce.
            let stripped: String = text.chars().filter(|&c| c != '\n' && c != '\r').collect();
            let chars_to_insert = stripped.chars().count();
            if chars_to_insert == 0 {
                return;
            }
            let byte_pos = app
                .input
                .char_indices()
                .map(|(i, _)| i)
                .nth(app.cursor_position)
                .unwrap_or(app.input.len());
            app.input.insert_str(byte_pos, &stripped);
            app.cursor_position += chars_to_insert;
            app.copy_toast_message = format!(
                "pasted {chars_to_insert} char{}",
                if chars_to_insert == 1 { "" } else { "s" }
            );
            app.copy_toast_failed = false;
            app.copy_toast_until = Some(std::time::Instant::now() + Duration::from_millis(1200));
        }
        ClipboardRead::Image { .. } => {
            // Modal fields are single-line text; images are not attachable
            // here. Surface a brief toast so the paste is not silently lost.
            app.copy_toast_message = "can't paste image into this field".to_string();
            app.copy_toast_failed = true;
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

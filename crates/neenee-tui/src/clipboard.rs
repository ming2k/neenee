//! Clipboard integration: OSC52 terminal sequences + system clipboard.
//!
//! This follows opencode's approach: the TUI framework manages copying,
//! not the terminal emulator.  When the user copies selected text, we
//! write it through both OSC52 (for remote/TTY sessions) and the
//! native system clipboard (arboard).

use std::io::{self, Write};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    Native,
    Osc52,
}

/// Copy text through a native clipboard owner when possible, then fall back to
/// OSC52. Wayland needs a living owner for the selection, so `wl-copy` is
/// preferred over creating and immediately dropping an arboard clipboard.
pub async fn copy(text: &str) -> Result<CopyOutcome, String> {
    let mut errors = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match copy_with_command("wl-copy", &[], text).await {
            Ok(()) => return Ok(CopyOutcome::Native),
            Err(error) => errors.push(error),
        }
    }
    match copy_system(text).await {
        Ok(()) => return Ok(CopyOutcome::Native),
        Err(error) => errors.push(error.to_string()),
    }

    write_osc52(text)
        .map(|_| CopyOutcome::Osc52)
        .map_err(|osc_error| {
            format!(
                "native clipboard failed: {}; OSC52 failed: {}",
                errors.join("; "),
                osc_error
            )
        })
}

/// Write an OSC52 "copy to clipboard" escape sequence to stdout.
///
/// Sequence: `ESC ] 52 ; c ; <base64> BEL`
/// In tmux: wrapped with `ESC P tmux ; ESC ... ESC \\`
/// In screen: wrapped with `ESC P ... ESC \\`
fn write_osc52(text: &str) -> io::Result<()> {
    let encoded = base64_encode(text);
    let sequence = format!("\x1b]52;c;{}\x07", encoded);

    let output = if std::env::var("TMUX").is_ok() {
        format!("\x1bPtmux;\x1b{}\x1b\\", sequence)
    } else if std::env::var("STY").is_ok() {
        // GNU screen
        format!("\x1bP{}\x1b\\", sequence)
    } else {
        sequence
    };

    let mut stdout = io::stdout();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()
}

async fn copy_with_command(command: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = tokio::process::Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        // Redirect stderr to /dev/null instead of piping it: helpers like
        // `wl-copy` fork a long-lived background daemon to hold the selection,
        // and that daemon inherits the stderr pipe. With a piped stderr,
        // `wait_with_output` would block until that daemon exits (i.e. until
        // the selection is replaced), making every copy appear to hang.
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("failed to start {}: {}", command, error))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("{} stdin was unavailable", command))?;
    stdin
        .write_all(text.as_bytes())
        .await
        .map_err(|error| format!("failed to write to {}: {}", command, error))?;
    drop(stdin);
    // Wait only for the foreground process to exit. `wl-copy` daemonizes
    // after reading stdin and setting the selection, so this returns within
    // milliseconds; it must not wait for the background daemon (which would
    // block until the selection is replaced).
    let status = child
        .wait()
        .await
        .map_err(|error| format!("{} failed: {}", command, error))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with {}", command, status))
    }
}

/// Copy text to the system clipboard using arboard.
async fn copy_system(text: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // arboard's Clipboard is not Send, so we do the work in a blocking task.
    let text = text.to_string();
    tokio::task::spawn_blocking(move || {
        let mut clipboard = arboard::Clipboard::new()?;
        clipboard.set_text(text)?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await??;
    Ok(())
}

/// Simple base64 encoder (no external crate needed).
fn base64_encode(input: &str) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b = match chunk.len() {
            1 => [chunk[0], 0, 0],
            2 => [chunk[0], chunk[1], 0],
            3 => [chunk[0], chunk[1], chunk[2]],
            _ => unreachable!(),
        };
        let n = (b[0] as usize) << 16 | (b[1] as usize) << 8 | (b[2] as usize);
        out.push(TABLE[(n >> 18) & 0x3F] as char);
        out.push(TABLE[(n >> 12) & 0x3F] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) & 0x3F]
        } else {
            b'='
        } as char);
        out.push(if chunk.len() > 2 {
            TABLE[n & 0x3F]
        } else {
            b'='
        } as char);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64() {
        assert_eq!(base64_encode("hello"), "aGVsbG8=");
        assert_eq!(base64_encode("hello world"), "aGVsbG8gd29ybGQ=");
        assert_eq!(base64_encode(""), "");
    }

    #[tokio::test]
    async fn command_clipboard_receives_utf8_input() {
        copy_with_command("cat", &[], "复制内容").await.unwrap();
    }
}

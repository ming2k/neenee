//! Server-Sent Events byte-stream decoder shared by every streaming provider.
//!
//! Streaming chat-completion endpoints deliver the response as an opaque run
//! of byte chunks whose boundaries are dictated by TLS/TCP framing — *not* by
//! SSE frames or UTF-8 character boundaries. A single CJK character occupies
//! 3 bytes in UTF-8; if a chunk boundary lands inside those bytes, decoding
//! each chunk on its own (e.g. with [`String::from_utf8_lossy`]) permanently
//! replaces the split bytes with `U+FFFD` (`�`) — the `���` artefact seen in
//! CJK output.
//!
//! The decoder here accumulates raw bytes and only performs UTF-8 decoding at
//! `\n` line boundaries, so a multi-byte sequence (or a partial SSE frame)
//! split across chunks is reassembled before any provider ever observes it.

use futures::StreamExt;
use futures::stream::BoxStream;

use crate::transport_error;

/// Decode a streaming SSE response into a flat stream of `data:` payload
/// strings (the `data:` prefix and surrounding whitespace stripped; the
/// `[DONE]` sentinel filtered out).
///
/// Byte reassembly happens internally, so callers never observe a character or
/// frame split across network chunks. Each yielded item is one SSE `data:`
/// event's payload — finer-grained and more responsive than batching by
/// network chunk, and the standard shape expected of an SSE reader.
pub(crate) fn data_payloads(
    response: reqwest::Response,
    provider: &'static str,
) -> BoxStream<'static, Result<String, String>> {
    let mut buffer: Vec<u8> = Vec::new();
    response
        .bytes_stream()
        .map(move |item| {
            let bytes = match item {
                Ok(bytes) => bytes,
                Err(error) => return vec![Err(transport_error(provider, error))],
            };
            buffer.extend_from_slice(&bytes);
            let mut payloads: Vec<Result<String, String>> = Vec::new();
            for line in drain_complete_lines(&mut buffer) {
                if let Some(data) = data_payload_from_line(&line) {
                    payloads.push(Ok(data.to_string()));
                }
            }
            payloads
        })
        .flat_map(futures::stream::iter)
        .boxed()
}

/// Extract the `data:` payload from a single (already complete) SSE line.
///
/// Returns `None` for non-data lines (event/id/retry/comments) and for the
/// `[DONE]` sentinel. Accepts both `data:` and `data: ` prefixes.
fn data_payload_from_line(line: &str) -> Option<&str> {
    line.strip_prefix("data:")
        .map(str::trim_start)
        .filter(|data| *data != "[DONE]")
}

/// Drain complete (`\n`-terminated) lines from a raw byte buffer, decoding
/// each as UTF-8. Trailing bytes after the final newline are retained so a
/// partial multi-byte sequence or SSE frame is completed on the next read.
fn drain_complete_lines(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(&buffer[..pos]).trim().to_string();
        buffer.drain(..pos + 1);
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_data_payload_and_strips_prefix() {
        assert_eq!(data_payload_from_line("data: hello"), Some("hello"));
        assert_eq!(data_payload_from_line("data:hello"), Some("hello"));
        assert_eq!(data_payload_from_line("data:  spaced"), Some("spaced"));
    }

    #[test]
    fn ignores_non_data_and_done_sentinel() {
        assert_eq!(data_payload_from_line(": keep-alive comment"), None);
        assert_eq!(data_payload_from_line("event: ping"), None);
        assert_eq!(data_payload_from_line("id: 42"), None);
        assert_eq!(data_payload_from_line("data: [DONE]"), None);
        assert_eq!(data_payload_from_line(""), None);
    }

    #[test]
    fn drain_reassembles_split_utf8_across_chunks() {
        // "😀😁" is two wide chars (8 bytes). Split the second char (4 bytes)
        // across two network chunks the way a TLS read would: the first chunk
        // ends with an incomplete leading byte sequence, the second completes
        // it. Decoding per-chunk would yield U+FFFD (`�`); buffering bytes and
        // decoding at the `\n` boundary must preserve the original text.
        let frame = "data: {\"text\":\"😀😁\"}\n".as_bytes().to_vec();
        let split = frame.len() - 5; // split inside the second 4-byte emoji
        let mut buffer: Vec<u8> = Vec::new();

        buffer.extend_from_slice(&frame[..split]);
        assert!(
            drain_complete_lines(&mut buffer).is_empty(),
            "no newline yet -> nothing decoded, partial bytes retained"
        );

        buffer.extend_from_slice(&frame[split..]);
        let lines = drain_complete_lines(&mut buffer);
        assert_eq!(lines, vec!["data: {\"text\":\"😀😁\"}".to_string()]);
        assert!(buffer.is_empty(), "buffer must be fully drained");
    }

    #[test]
    fn drain_handles_crlf_and_retains_partial_tail() {
        let mut buffer = b"data: one\r\ndata: two\npartial".to_vec();
        let lines = drain_complete_lines(&mut buffer);
        assert_eq!(lines, vec!["data: one", "data: two"]);
        assert_eq!(buffer, b"partial");
    }
}

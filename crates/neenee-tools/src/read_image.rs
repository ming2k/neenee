//! `read_image` — read an image file and return it as a base64 data payload so
//! a vision-capable model can see it. Mirrors codex's `view_image` (resize to
//! cap size) and opencode's `read` (MIME-gated), localised to neenee's
//! `ToolOutput::Image` channel.
//!
//! The harness peels the image out of the tool result into a follow-up
//! user-role message (see `agent.rs`), since neenee's providers target the
//! OpenAI Chat Completions shape where tool messages only accept string
//! content — exactly the lowering opencode performs for OpenAI Chat.

use async_trait::async_trait;
use neenee_core::{Tool, ToolOutput};
use serde_json::json;

/// Read an image file so the model can see it.
pub struct ReadImageTool;

/// Images are downscaled so the longest edge is at most this many pixels
/// before encoding. The Chat Completions / Responses API treats very large
/// images as a lot of tokens (OpenAI's detail tiers), and kimi/GLM code
/// providers are even less tolerant — capping the edge keeps a screenshot or
/// photo from blowing the context budget while staying legible. Matches the
/// order of magnitude codex/opencode use (~1568px is OpenAI's "high" tier).
const MAX_EDGE_PX: u32 = 1568;

#[async_trait]
impl Tool for ReadImageTool {
    fn name(&self) -> &str {
        "read_image"
    }
    fn description(&self) -> &str {
        "Read an **image** file so you can see it. Returns the image inline \
         for your vision; no text is extracted. Use this for screenshots, \
         diagrams, photos, UI mockups, charts — anything you need to actually \
         *look at* rather than read as text.\n\
         \n\
         Supported formats: PNG, JPEG, GIF, WebP. For plain-text files \
         (source code, config, docs) use `read_file` instead — this tool is \
         for images only. Large images are auto-resized to a sensible \
         resolution before being shown to you."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Absolute or relative path to the image file" }
            },
            "required": ["path"]
        })
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        self.call_structured(arguments).await.map(|o| o.to_text())
    }

    async fn call_structured(&self, arguments: &str) -> Result<ToolOutput, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let path = args["path"].as_str().ok_or("Missing 'path'")?;

        // Validate the extension up front so a non-image path fails fast
        // without touching the filesystem, and a missing image fails with a
        // clear "not an image" message rather than a read error.
        let mime =
            mime_for_path(path).ok_or_else(|| format!("Unsupported image format: {}", path))?;

        let bytes = std::fs::read(path).map_err(|e| format!("Failed to read '{}': {}", path, e))?;

        // Resize if the image decodes and is larger than the cap; otherwise
        // pass the original bytes through untouched (fast path for already-
        // small images and for formats we can't cheaply re-encode, e.g. the
        // source may already be optimal).
        let encoded = encode_image(&bytes, &mime, MAX_EDGE_PX)
            .map_err(|e| format!("Failed to process image '{}': {}", path, e))?;

        Ok(ToolOutput::Image {
            mime,
            data: encoded,
        })
    }
}

neenee_core::register_tool!(ReadImageFactory => ReadImageTool);

/// Map a file extension to the MIME type neenee can encode. We accept the
/// formats the `image` crate is built with (see Cargo features) and that the
/// providers' vision endpoints ingest.
fn mime_for_path(path: &str) -> Option<String> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())?;
    match ext.as_str() {
        "png" => Some("image/png".to_string()),
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "gif" => Some("image/gif".to_string()),
        "webp" => Some("image/webp".to_string()),
        _ => None,
    }
}

/// Decode, downscale to `max_edge`, and re-encode as base64 in the original
/// MIME. GIFs are passed through verbatim: the `image` crate's GIF decoder is
/// frame-based and re-encoding would drop animation, so we hand the original
/// bytes to the model untouched (providers accept animated GIFs inline).
fn encode_image(bytes: &[u8], mime: &str, max_edge: u32) -> Result<String, String> {
    if mime == "image/gif" {
        return base64_or_err(bytes);
    }
    // Decode from memory. `image::load_from_memory` picks the decoder from the
    // bytes, but we already validated the MIME, so constrain by format.
    let format = match mime {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/webp" => image::ImageFormat::WebP,
        "image/gif" => image::ImageFormat::Gif,
        other => return Err(format!("unsupported mime: {other}")),
    };
    let img = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| format!("decode failed: {e}"))?;

    // Only downscale when an edge exceeds the cap; preserves small icons /
    // thumbnails losslessly.
    let (w, h) = (img.width(), img.height());
    let scaled = if w > max_edge || h > max_edge {
        img.resize(max_edge, max_edge, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    let mut buf = std::io::Cursor::new(Vec::new());
    scaled
        .write_to(&mut buf, format)
        .map_err(|e| format!("re-encode failed: {e}"))?;
    base64_or_err(&buf.into_inner())
}

fn base64_or_err(bytes: &[u8]) -> Result<String, String> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    Ok(STANDARD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_detection() {
        assert_eq!(mime_for_path("a.png"), Some("image/png".to_string()));
        assert_eq!(mime_for_path("a.JPG"), Some("image/jpeg".to_string()));
        assert_eq!(mime_for_path("a.webp"), Some("image/webp".to_string()));
        assert_eq!(mime_for_path("a.txt"), None);
        assert_eq!(mime_for_path("noext"), None);
    }

    #[tokio::test]
    async fn rejects_non_image_extension() {
        let err = ReadImageTool
            .call_structured(r#"{"path":"missing.txt"}"#)
            .await
            .unwrap_err();
        assert!(err.contains("Unsupported image format"));
    }

    #[tokio::test]
    async fn reports_missing_file() {
        let err = ReadImageTool
            .call_structured(r#"{"path":"nope.png"}"#)
            .await
            .unwrap_err();
        assert!(err.contains("Failed to read"));
    }

    #[tokio::test]
    async fn reads_small_png_as_image_output() {
        // Synthesize a 4x4 red PNG in memory.
        let mut buf = std::io::Cursor::new(Vec::new());
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let path = std::env::temp_dir().join("neenee_read_image_test.png");
        std::fs::write(&path, buf.into_inner()).unwrap();

        let out = ReadImageTool
            .call_structured(&format!(r#"{{"path":"{}"}}"#, path.display()))
            .await
            .unwrap();
        match out {
            ToolOutput::Image { mime, data } => {
                assert_eq!(mime, "image/png");
                assert!(!data.is_empty());
                // Decodes back to valid base64 bytes.
                use base64::{Engine, engine::general_purpose::STANDARD};
                let raw = STANDARD.decode(data).unwrap();
                assert!(raw.starts_with(&[0x89, b'P', b'N', b'G']));
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }
}

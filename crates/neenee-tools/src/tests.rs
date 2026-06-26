#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use crate::*;
    use neenee_core::{Tool, WebSearchConfig, truncate_utf8};

    #[test]
    fn html_to_text_handles_multibyte_before_script_tags() {
        let html = "αβ<script>hidden</script>γδ<style>.x{}</style>εζ";

        assert_eq!(html_to_text(html), "αβγδεζ");
    }

    #[test]
    fn truncate_utf8_does_not_split_multibyte_chars() {
        let text = "prefix ’ suffix";
        let inside_curly_quote = text.find('’').unwrap() + 1;

        assert_eq!(truncate_utf8(text, inside_curly_quote), "prefix ");
    }

    #[test]
    fn websearch_config_defaults_to_exa_with_parallel_fallback() {
        let cfg = WebSearchConfig::default();
        assert_eq!(cfg.provider, "exa");
        assert_eq!(cfg.fallback, "parallel");
        assert!(cfg.proxy.is_none());
        assert_eq!(cfg.timeout_secs, 20);
    }

    #[test]
    fn websearch_config_round_trips_through_toml() {
        let toml = r#"
            provider = "searxng"
            fallback = ""
            proxy = "socks5h://127.0.0.1:1080"
            timeout_secs = 8
            searxng_url = "http://localhost:8080/search"
        "#;
        let cfg: WebSearchConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.provider, "searxng");
        assert_eq!(cfg.fallback, "");
        assert_eq!(cfg.proxy.as_deref(), Some("socks5h://127.0.0.1:1080"));
        assert_eq!(cfg.timeout_secs, 8);
        assert_eq!(
            cfg.searxng_url.as_deref(),
            Some("http://localhost:8080/search")
        );
    }

    #[test]
    fn write_and_edit_tools_allow_plan_paths_in_plan_mode() {
        // Plan-mode path exemption was removed (ADR-0027/0028): scoped writes
        // are now expressed per-agent via `WriteScope`, not via an
        // `allowed_in_plan_mode` override on the write tools. This test is
        // kept as a placeholder guard that the write tools still build; the
        // scoping behavior is covered by neenee-core's WriteScope tests.
        let _write = WriteFileTool;
        let _edit = EditFileTool;
    }

    #[tokio::test]
    async fn read_file_carries_offset_as_start_line() {
        // The structured `Code::start_line` is the contract the renderer relies
        // on to number an offset snippet from its true file line. A read with
        // `offset: 3` must surface `start_line: 3` (and only the post-offset
        // content), while a plain read reports `start_line: 1`.
        let dir =
            std::env::temp_dir().join(format!("neenee-read-start-line-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lines.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();

        let tool = ReadFileTool;

        let full = tool
            .call_structured(&r#"{"path":"PATH"}"#.replace("PATH", &path.to_string_lossy()))
            .await
            .unwrap();
        match full {
            neenee_core::ToolOutput::Code {
                start_line, text, ..
            } => {
                assert_eq!(start_line, 1);
                assert!(text.starts_with("one"));
            }
            _ => panic!("expected Code"),
        }

        let offset = tool
            .call_structured(
                &r#"{"path":"PATH","offset":3}"#.replace("PATH", &path.to_string_lossy()),
            )
            .await
            .unwrap();
        match offset {
            neenee_core::ToolOutput::Code {
                start_line, text, ..
            } => {
                assert_eq!(start_line, 3);
                assert_eq!(text, "three\nfour\nfive");
            }
            _ => panic!("expected Code"),
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Pull `(text, prefix, suffix)` out of a `Code` output for assertions.
    fn code_parts(out: neenee_core::ToolOutput) -> (String, Option<String>, Option<String>) {
        match out {
            neenee_core::ToolOutput::Code {
                text,
                prefix,
                suffix,
                ..
            } => (text, prefix, suffix),
            _ => panic!("expected Code output"),
        }
    }

    /// A file whose every line is exactly `line_width` chars so the byte-budget
    /// math is predictable in the pagination tests below.
    fn make_fixed_width_file(line_count: usize) -> (std::path::PathBuf, Vec<String>) {
        let dir =
            std::env::temp_dir().join(format!("neenee-read-paginate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.txt");
        let lines: Vec<String> = (1..=line_count).map(|n| format!("line{n:05}")).collect();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
        (path, lines)
    }

    #[tokio::test]
    async fn plain_small_read_has_no_framing() {
        // The common case stays byte-identical to the legacy model output:
        // no prefix/suffix, so we don't tax every small read.
        let dir = std::env::temp_dir().join(format!("neenee-read-plain-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("small.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();

        let out = ReadFileTool
            .call_structured(&r#"{"path":"PATH"}"#.replace("PATH", &path.to_string_lossy()))
            .await
            .unwrap();
        let (text, prefix, suffix) = code_parts(out);
        assert_eq!(text, "a\nb\nc");
        assert!(prefix.is_none());
        assert!(suffix.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn large_read_paginates_with_concrete_non_overlapping_continuation() {
        // 6000 lines × 10 bytes ("lineNNNNN\n") = 60KB. The 50 000-byte budget
        // holds ~5000 lines per page. The tool MUST return whole lines, declare
        // the range, and give an exact next offset — and following that offset
        // must continue without overlap or gap (the loop-safety contract).
        const LINES: usize = 6000;
        const PAGE: usize = 5000; // 50_000 / (9 + 1)
        let (path, _lines) = make_fixed_width_file(LINES);
        let tool = ReadFileTool;
        let arg = |offset: usize| {
            format!(
                r#"{{"path":"{}","offset":{}}}"#,
                path.to_string_lossy(),
                offset
            )
        };

        // Page 1: lines 1..=5000, continuation offset = 5001.
        let (text1, pre1, suf1) = code_parts(tool.call_structured(&arg(1)).await.unwrap());
        assert_eq!(
            pre1,
            Some(format!(
                "[{}: lines 1-{} of {}]",
                path.to_string_lossy(),
                PAGE,
                LINES
            ))
        );
        let suf1 = suf1.expect("page 1 has a continuation suffix");
        assert!(
            suf1.contains("offset=5001"),
            "suffix must name the exact next offset, got: {suf1}"
        );
        assert_eq!(text1.lines().count(), PAGE);
        assert_eq!(text1.lines().next().unwrap(), "line00001");
        assert_eq!(text1.lines().last().unwrap(), &format!("line{:05}", PAGE));

        // Page 2 from the advertised offset: must start exactly at 5001 (no gap)
        // and not repeat line 5000 (no overlap) — this is what breaks the loop.
        let (text2, _pre2, suf2) = code_parts(tool.call_structured(&arg(5001)).await.unwrap());
        assert_eq!(text2.lines().next().unwrap(), "line05001", "no gap");
        assert!(
            !text2.lines().any(|l| l == "line05000"),
            "no overlap with previous page"
        );
        // Page 2 is the final page (1000 lines remaining).
        assert!(suf2.is_none(), "page 2 reaches EOF, no suffix");

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn oversized_limit_is_line_bounded_not_re_truncated() {
        // Regression for the real infinite-loop trap: requesting a huge `limit`
        // on a big file used to keep the slice over budget, re-truncate the
        // same window, and emit a generic "use offset/limit" with no number.
        // Now the window is line-bounded and the continuation is concrete, so
        // the model advances instead of looping.
        const LINES: usize = 6000;
        let (path, _lines) = make_fixed_width_file(LINES);
        let arg = format!(
            r#"{{"path":"{}","limit":{}}}"#,
            path.to_string_lossy(),
            LINES
        );
        let (text, _pre, suf) = code_parts(ReadFileTool.call_structured(&arg).await.unwrap());
        // Far fewer than the requested 6000 lines — bounded by the budget.
        assert!(text.lines().count() < LINES);
        assert!(
            suf.expect("oversized limit still paginates")
                .contains("offset="),
            "gives a concrete next offset rather than a generic hint"
        );

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn empty_and_past_eof_reads_explain_themselves() {
        // Both cases used to return a bare empty string, which a model can
        // mistake for a failure and re-read in a loop. They now carry an
        // explicit note via the model-facing prefix.
        let dir = std::env::temp_dir().join(format!("neenee-read-edge-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let empty = dir.join("empty.txt");
        std::fs::write(&empty, "").unwrap();
        let (text, pre, suf) = code_parts(
            ReadFileTool
                .call_structured(&r#"{"path":"PATH"}"#.replace("PATH", &empty.to_string_lossy()))
                .await
                .unwrap(),
        );
        assert!(text.is_empty());
        assert!(
            pre.as_ref().is_some_and(|p| p.contains("empty file")),
            "pre={pre:?}"
        );
        assert!(suf.is_none());

        let small = dir.join("small.txt");
        std::fs::write(&small, "a\nb\n").unwrap();
        let (text, pre, suf) = code_parts(
            ReadFileTool
                .call_structured(
                    &r#"{"path":"PATH","offset":99}"#.replace("PATH", &small.to_string_lossy()),
                )
                .await
                .unwrap(),
        );
        assert!(text.is_empty());
        assert!(
            pre.as_ref().is_some_and(|p| p.contains("past end of file")),
            "pre={pre:?}"
        );
        assert!(suf.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn reading_a_directory_suggests_list_dir() {
        // A directory read used to surface the raw OS error ("Is a directory
        // (os error 21)"), which gives the model no hint about what to do.
        // Now it gets an explicit, actionable message naming `list_dir`, which
        // breaks any retry loop.
        let dir = std::env::temp_dir().join(format!("neenee-read-isdir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let err = ReadFileTool
            .call(&r#"{"path":"PATH"}"#.replace("PATH", &dir.to_string_lossy()))
            .await
            .unwrap_err();
        assert!(
            err.contains("list_dir"),
            "should point to list_dir, got: {err}"
        );
        assert!(
            !err.contains("os error"),
            "should not leak the raw OS error, got: {err}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

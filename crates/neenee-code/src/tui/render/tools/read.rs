//! Presenter for `read_text`.

use super::{ToolPresenter, ToolView};

pub struct ReadPresenter;

impl ToolPresenter for ReadPresenter {
    fn summary(&self, view: &ToolView) -> String {
        let base = view
            .str("path")
            .map(|path| format!("Read {}", path))
            .unwrap_or_else(|| "Read file".to_string());

        // Annotate the window when an `offset`/`limit` narrows the read, using
        // Vim ex-command range syntax (`:start,end` / `:start,$`). `offset` is
        // the 1-based start line; `limit` is a line *count*, so the inclusive
        // end is `start + limit - 1`. A read with no limit runs to EOF, which
        // maps to Vim's `$` address. Defaults (`offset == 1`, `limit == 0`) are
        // omitted, so a plain full-file read stays annotation-free. The body's
        // line gutter already reflects `offset`; this makes the header
        // self-describing too.
        let offset = view.u64("offset").filter(|&o| o > 1);
        let limit = view.u64("limit").filter(|&l| l > 0);
        let range = match (offset, limit) {
            // count is >= 1 (filtered above), so end = start + count - 1.
            (Some(start), Some(count)) => Some(format!(":{},{}", start, start + count - 1)),
            // No limit → reads to EOF, Vim's `$` address.
            (Some(start), None) => Some(format!(":{},$", start)),
            // No offset → starts at line 1; end = count.
            (None, Some(count)) => Some(format!(":1,{}", count)),
            (None, None) => None,
        };
        match range {
            Some(r) => format!("{} {}", base, r),
            None => base,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReadPresenter, ToolPresenter, ToolView};
    use serde_json::{Map, Value, json};

    fn view(args: Value) -> ToolView<'static> {
        // Leak is acceptable for a unit test's static lifetime.
        let owned: Map<String, Value> = match args {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        let args = Box::leak(owned.into());
        ToolView {
            name: "read_text",
            args,
            profile: None,
        }
    }

    #[test]
    fn summary_plain_read_has_no_window_annotation() {
        let v = view(json!({"path": "src/lib.rs"}));
        assert_eq!(ReadPresenter.summary(&v), "Read src/lib.rs");
        // offset 1 is the default, so it must not appear.
        let v = view(json!({"path": "src/lib.rs", "offset": 1}));
        assert_eq!(ReadPresenter.summary(&v), "Read src/lib.rs");
        // limit 0 is the default (to EOF), so it must not appear.
        let v = view(json!({"path": "src/lib.rs", "limit": 0}));
        assert_eq!(ReadPresenter.summary(&v), "Read src/lib.rs");
    }

    #[test]
    fn summary_offset_annotates_start_line() {
        // offset with no limit reads to EOF → Vim `$` address.
        let v = view(json!({"path": "src/lib.rs", "offset": 100}));
        assert_eq!(ReadPresenter.summary(&v), "Read src/lib.rs :100,$");
    }

    #[test]
    fn summary_limit_annotates_window_size() {
        // limit alone starts at line 1 → `:1,end` where end = limit.
        let v = view(json!({"path": "src/lib.rs", "limit": 50}));
        assert_eq!(ReadPresenter.summary(&v), "Read src/lib.rs :1,50");
    }

    #[test]
    fn summary_offset_and_limit_annotate_both() {
        // offset 100 + limit 50 → inclusive range :100,149.
        let v = view(json!({"path": "src/lib.rs", "offset": 100, "limit": 50}));
        assert_eq!(ReadPresenter.summary(&v), "Read src/lib.rs :100,149");
    }

    #[test]
    fn summary_offset_and_limit_of_one_collapses_to_single_line() {
        // limit 1 → end == start, range is a single line.
        let v = view(json!({"path": "src/lib.rs", "offset": 42, "limit": 1}));
        assert_eq!(ReadPresenter.summary(&v), "Read src/lib.rs :42,42");
    }

    #[test]
    fn summary_falls_back_without_path() {
        let v = view(json!({"offset": 100}));
        assert_eq!(ReadPresenter.summary(&v), "Read file :100,$");
    }
}

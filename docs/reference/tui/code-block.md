# Code block

Fenced code in the transcript.

## Appearance

```text
  ┃ rust
  ┃ 1  pub fn main() {
  ┃ 2      println!("hi");
  ┃ 3  }
```

| Attribute | Value |
|-----------|-------|
| Accent bar | `┃` in `accent` at column 2 (2-char indent) |
| Background | `code_bg` (22, 24, 35) full-width band |
| Text color | `code_fg` (148, 226, 213) |
| Line-number gutter | Right-aligned, min width 2, `dim_fg` |
| Language label | Own dim line under the `┃` bar |

## Layout

- No borders, no `╭─ ╰─` frame (borderless, opencode-style).
- Line numbers appear on the first visual line of each logical line.
  Continuation lines (from wrapping) show a blank gutter.
- Wrapping respects CJK kinsoku rules.
- Character-level semantic selection via `code_gutter_line`.

## Language label

If the fenced code specifies a language (e.g., ` ```rust `), a dim language tag
line appears as the first line of the block, under the `┃` bar. If no language
is specified, no label is shown.

## Source

`draw_message_body` → `Block::Code` in `render/message_body.rs`. Uses
`code_gutter_line` for per-line rendering with optional left bar.

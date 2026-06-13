//! Semantic document model for the TUI.
//!
//! Unlike storing raw strings, this model preserves the structure of messages
//! so that selection and copy operate on semantic units (blocks) rather than
//! terminal grid characters.

use neenee_core::Role;

#[derive(Debug, Clone, PartialEq)]
pub enum MessageKind {
    Chat,
    ToolStep {
        name: String,
        arguments: String,
        output: Option<String>,
        expanded: bool,
    },
}

/// A single semantic block within a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Plain text paragraph.
    Text { content: String },
    /// Inline or fenced code.
    Code {
        language: Option<String>,
        content: String,
    },
    /// A heading.
    Heading { level: u8, content: String },
    /// A list item, preserving its marker and nesting level.
    ListItem {
        content: String,
        ordered: Option<u64>,
        depth: usize,
        checked: Option<bool>,
    },
    /// A blockquote.
    Quote { content: String },
    /// A horizontal rule.
    Rule,
    /// Soft / hard line break marker.
    Break,
}

impl Block {
    /// Returns the raw text content of this block (without formatting).
    pub fn raw_text(&self) -> &str {
        match self {
            Block::Text { content } => content,
            Block::Code { content, .. } => content,
            Block::Heading { content, .. } => content,
            Block::ListItem { content, .. } => content,
            Block::Quote { content } => content,
            Block::Rule => "",
            Block::Break => "\n",
        }
    }

    /// Returns true if this block is empty.
    pub fn is_empty(&self) -> bool {
        self.raw_text().is_empty()
    }
}

/// A structured chat message.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub role: Role,
    pub blocks: Vec<Block>,
    /// The original raw markdown/text, preserved for exact copy.
    pub raw: String,
    pub kind: MessageKind,
}

impl ChatMessage {
    pub fn new(role: Role, raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let blocks = parse_blocks(&raw);
        Self {
            role,
            blocks,
            raw,
            kind: MessageKind::Chat,
        }
    }

    pub fn tool_step(name: impl Into<String>, arguments: impl Into<String>) -> Self {
        let mut message = Self {
            role: Role::Tool,
            blocks: Vec::new(),
            raw: String::new(),
            kind: MessageKind::ToolStep {
                name: name.into(),
                arguments: arguments.into(),
                output: None,
                expanded: false,
            },
        };
        message.refresh_tool_step();
        message
    }

    pub fn finish_tool_step(&mut self, name: &str, output: impl Into<String>) -> bool {
        let MessageKind::ToolStep {
            name: step_name,
            output: step_output,
            ..
        } = &mut self.kind
        else {
            return false;
        };
        if step_name != name || step_output.is_some() {
            return false;
        }
        *step_output = Some(output.into());
        self.refresh_tool_step();
        true
    }

    pub fn is_tool_step(&self) -> bool {
        matches!(self.kind, MessageKind::ToolStep { .. })
    }

    pub fn tool_step_expanded(&self) -> Option<bool> {
        match &self.kind {
            MessageKind::ToolStep { expanded, .. } => Some(*expanded),
            MessageKind::Chat => None,
        }
    }

    pub fn set_tool_step_expanded(&mut self, expanded: bool) {
        if let MessageKind::ToolStep {
            expanded: current, ..
        } = &mut self.kind
        {
            *current = expanded;
            self.refresh_tool_step();
        }
    }

    fn refresh_tool_step(&mut self) {
        let MessageKind::ToolStep {
            name,
            arguments,
            output,
            expanded,
        } = &self.kind
        else {
            return;
        };
        self.raw = if *expanded {
            let arguments = pretty_json(arguments);
            match output {
                Some(output) => format!(
                    "Calling `{}`\n\n```json\n{}\n```\n\nResult\n\n{}",
                    name, arguments, output
                ),
                None => format!("Calling `{}`\n\n```json\n{}\n```", name, arguments),
            }
        } else {
            let status = match output {
                Some(output) if output.starts_with("Error") => "failed",
                Some(_) => "completed",
                None => "running",
            };
            format!("⚒ {} · {} · {}", name, status, argument_summary(arguments))
        };
        self.blocks = parse_blocks(&self.raw);
    }

    /// Re-parse blocks from raw text (e.g. after streaming append).
    pub fn reparse(&mut self) {
        self.blocks = parse_blocks(&self.raw);
    }

    /// Append streaming text and re-parse.
    ///
    /// Parsing every accumulated chunk keeps the live layout structurally
    /// consistent with the final layout. The previous append-only Text block
    /// path delayed all Markdown structure until StreamEnd, causing the whole
    /// response to jump when headings, lists, and code fences were discovered.
    pub fn push_stream(&mut self, delta: &str) {
        self.raw.push_str(delta);
        self.reparse();
    }

    /// Extract text from a byte range within this message's raw content.
    pub fn raw_slice(&self, start: usize, end: usize) -> &str {
        let len = self.raw.len();
        let start = start.min(len);
        let end = end.min(len);
        &self.raw[start..end]
    }
}

fn pretty_json(arguments: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| arguments.to_string())
}

fn argument_summary(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return truncate(arguments, 72);
    };
    let Some(object) = value.as_object() else {
        return truncate(arguments, 72);
    };
    let summary = ["path", "pattern", "command", "name"]
        .iter()
        .find_map(|key| {
            object
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .map(|value| format!("{}={}", key, value))
        })
        .unwrap_or_else(|| arguments.to_string());
    truncate(&summary, 72)
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{}...", prefix)
    } else {
        prefix
    }
}

/// Parse raw markdown-like text into semantic blocks.
///
/// This is intentionally lightweight — it splits on major block boundaries
/// (code fences, headings, rules, blockquotes) while preserving the original
/// text so copying yields exact source.
pub fn parse_blocks(text: &str) -> Vec<Block> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let mut blocks = Vec::new();
    let mut paragraph = String::new();
    let mut heading: Option<(u8, String)> = None;
    let mut code_lang: Option<String> = None;
    let mut code_content = String::new();
    let mut in_code = false;
    let mut quotes = Vec::<String>::new();
    let mut lists = Vec::<ListState>::new();
    let mut items = Vec::<ListAccumulator>::new();
    let mut table = None::<TableAccumulator>;

    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => paragraph.clear(),
                Tag::Heading { level, .. } => {
                    heading = Some((heading_level(level), String::new()));
                }
                Tag::CodeBlock(lang) => {
                    in_code = true;
                    code_lang = match &lang {
                        pulldown_cmark::CodeBlockKind::Fenced(l) => Some(l.to_string()),
                        _ => None,
                    };
                    code_content.clear();
                }
                Tag::BlockQuote => {
                    quotes.push(String::new());
                }
                Tag::List(start) => lists.push(ListState { next: start }),
                Tag::Item => {
                    let ordered = lists.last_mut().and_then(|list| {
                        let current = list.next?;
                        list.next = Some(current + 1);
                        Some(current)
                    });
                    items.push(ListAccumulator {
                        content: String::new(),
                        ordered,
                        depth: lists.len().saturating_sub(1),
                        checked: None,
                    });
                }
                Tag::Table(_) => table = Some(TableAccumulator::default()),
                Tag::TableHead | Tag::TableRow => {
                    if let Some(table) = &mut table {
                        table.start_row();
                    }
                }
                Tag::TableCell => {
                    if let Some(table) = &mut table {
                        table.start_cell();
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if items.is_empty() && quotes.is_empty() && table.is_none() {
                        push_block(
                            &mut blocks,
                            Block::Text {
                                content: paragraph.trim_end().to_string(),
                            },
                        );
                    }
                    paragraph.clear();
                }
                TagEnd::Heading(_) => {
                    if let Some((level, content)) = heading.take() {
                        push_block(
                            &mut blocks,
                            Block::Heading {
                                level,
                                content: content.trim_end().to_string(),
                            },
                        );
                    }
                }
                TagEnd::CodeBlock => {
                    in_code = false;
                    let content = code_content
                        .strip_prefix('\n')
                        .unwrap_or(&code_content)
                        .trim_end_matches('\n');
                    push_block(
                        &mut blocks,
                        Block::Code {
                            language: code_lang.take(),
                            content: content.to_string(),
                        },
                    );
                }
                TagEnd::BlockQuote => {
                    if let Some(content) = quotes.pop() {
                        push_block(
                            &mut blocks,
                            Block::Quote {
                                content: content.trim_end().to_string(),
                            },
                        );
                    }
                }
                TagEnd::Item => {
                    if let Some(item) = items.pop() {
                        push_block(
                            &mut blocks,
                            Block::ListItem {
                                content: item.content.trim_end().to_string(),
                                ordered: item.ordered,
                                depth: item.depth,
                                checked: item.checked,
                            },
                        );
                    }
                }
                TagEnd::List(_) => {
                    lists.pop();
                }
                TagEnd::TableCell => {
                    if let Some(table) = &mut table {
                        table.end_cell();
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(table) = &mut table {
                        table.end_row();
                    }
                }
                TagEnd::Table => {
                    if let Some(table) = table.take() {
                        push_block(
                            &mut blocks,
                            Block::Text {
                                content: table.render(),
                            },
                        );
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_code {
                    code_content.push_str(&t);
                } else {
                    append_text(
                        &t,
                        &mut heading,
                        &mut items,
                        &mut quotes,
                        &mut table,
                        &mut paragraph,
                    );
                }
            }
            Event::Code(t) => {
                if in_code {
                    code_content.push('`');
                    code_content.push_str(&t);
                    code_content.push('`');
                } else {
                    append_text(
                        &t,
                        &mut heading,
                        &mut items,
                        &mut quotes,
                        &mut table,
                        &mut paragraph,
                    );
                }
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                if in_code {
                    code_content.push_str(&h);
                } else {
                    append_text(
                        &h,
                        &mut heading,
                        &mut items,
                        &mut quotes,
                        &mut table,
                        &mut paragraph,
                    );
                }
            }
            Event::SoftBreak => {
                if in_code {
                    code_content.push('\n');
                } else {
                    append_text(
                        " ",
                        &mut heading,
                        &mut items,
                        &mut quotes,
                        &mut table,
                        &mut paragraph,
                    );
                }
            }
            Event::HardBreak => {
                if in_code {
                    code_content.push('\n');
                } else {
                    append_text(
                        "\n",
                        &mut heading,
                        &mut items,
                        &mut quotes,
                        &mut table,
                        &mut paragraph,
                    );
                }
            }
            Event::Rule => {
                push_block(&mut blocks, Block::Rule);
            }
            Event::TaskListMarker(checked) => {
                if let Some(item) = items.last_mut() {
                    item.checked = Some(checked);
                }
            }
            _ => {}
        }
    }

    if !paragraph.trim().is_empty() {
        push_block(
            &mut blocks,
            Block::Text {
                content: paragraph.trim_end().to_string(),
            },
        );
    }
    while matches!(blocks.last(), Some(Block::Break)) {
        blocks.pop();
    }
    blocks
}

#[derive(Default)]
struct TableAccumulator {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
}

impl TableAccumulator {
    fn start_row(&mut self) {
        self.row.clear();
    }

    fn end_row(&mut self) {
        if !self.cell.is_empty() {
            self.end_cell();
        }
        if !self.row.is_empty() {
            self.rows.push(std::mem::take(&mut self.row));
        }
    }

    fn start_cell(&mut self) {
        self.cell.clear();
    }

    fn end_cell(&mut self) {
        self.row.push(std::mem::take(&mut self.cell));
    }

    fn render(self) -> String {
        self.rows
            .into_iter()
            .map(|row| format!("│ {} │", row.join(" │ ")))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

struct ListAccumulator {
    content: String,
    ordered: Option<u64>,
    depth: usize,
    checked: Option<bool>,
}

struct ListState {
    next: Option<u64>,
}

fn append_text(
    text: &str,
    heading: &mut Option<(u8, String)>,
    items: &mut [ListAccumulator],
    quotes: &mut [String],
    table: &mut Option<TableAccumulator>,
    paragraph: &mut String,
) {
    if let Some(table) = table {
        table.cell.push_str(text);
    } else if let Some((_, content)) = heading {
        content.push_str(text);
    } else if let Some(item) = items.last_mut() {
        item.content.push_str(text);
    } else if let Some(quote) = quotes.last_mut() {
        quote.push_str(text);
    } else {
        paragraph.push_str(text);
    }
}

fn push_block(blocks: &mut Vec<Block>, block: Block) {
    if block.is_empty() && !matches!(block, Block::Rule | Block::Break) {
        return;
    }
    let needs_gap = blocks.last().is_some_and(|previous| {
        !matches!(
            (previous, &block),
            (Block::Break, _)
                | (Block::Heading { .. }, Block::Text { .. })
                | (Block::ListItem { .. }, Block::ListItem { .. })
        )
    });
    if needs_gap {
        blocks.push(Block::Break);
    }
    blocks.push(block);
}

fn heading_level(level: pulldown_cmark::HeadingLevel) -> u8 {
    match level {
        pulldown_cmark::HeadingLevel::H1 => 1,
        pulldown_cmark::HeadingLevel::H2 => 2,
        pulldown_cmark::HeadingLevel::H3 => 3,
        pulldown_cmark::HeadingLevel::H4 => 4,
        pulldown_cmark::HeadingLevel::H5 => 5,
        pulldown_cmark::HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_text() {
        let blocks = parse_blocks("Hello world");
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], Block::Text { content } if content == "Hello world"));
    }

    #[test]
    fn test_parse_code_block() {
        let text = "Some text\n\n```rust\nfn main() {}\n```\n\nMore text";
        let blocks = parse_blocks(text);
        assert_eq!(blocks.len(), 5);
        assert!(matches!(&blocks[0], Block::Text { content } if content == "Some text"));
        assert!(
            matches!(&blocks[2], Block::Code { language, content } if language.as_deref() == Some("rust") && content == "fn main() {}")
        );
        assert!(matches!(&blocks[4], Block::Text { content } if content == "More text"));
    }

    #[test]
    fn test_push_stream() {
        let mut streamed = ChatMessage::new(Role::Assistant, "");
        for chunk in [
            "# Result\n\n",
            "First paragraph.\n\n",
            "- one\n",
            "- two\n\n",
            "```rust\nfn main() {}\n```",
        ] {
            streamed.push_stream(chunk);
        }

        let completed = ChatMessage::new(Role::Assistant, streamed.raw.clone());
        assert_eq!(streamed.blocks, completed.blocks);
    }

    #[test]
    fn parses_block_boundaries_without_collapsing_the_document() {
        let blocks = parse_blocks(
            "# Result\n\nFirst paragraph.\n\nSecond paragraph.\n\n1. one\n2. two\n\n> quoted",
        );

        assert!(matches!(
            &blocks[0],
            Block::Heading { level: 1, content } if content == "Result"
        ));
        assert!(blocks.iter().any(|block| matches!(block, Block::Break)));
        assert!(blocks.iter().any(
            |block| matches!(block, Block::Text { content } if content == "First paragraph.")
        ));
        assert!(blocks.iter().any(
            |block| matches!(block, Block::Text { content } if content == "Second paragraph.")
        ));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::ListItem {
                content,
                ordered: Some(1),
                ..
            } if content == "one"
        )));
        assert!(blocks
            .iter()
            .any(|block| matches!(block, Block::Quote { content } if content == "quoted")));
    }

    #[test]
    fn markdown_soft_breaks_flow_but_hard_breaks_are_preserved() {
        let soft = parse_blocks("第一行\n第二行");
        assert!(matches!(
            &soft[0],
            Block::Text { content } if content == "第一行 第二行"
        ));

        let hard = parse_blocks("第一行  \n第二行");
        assert!(matches!(
            &hard[0],
            Block::Text { content } if content == "第一行\n第二行"
        ));
    }

    #[test]
    fn parses_task_lists_and_tables() {
        let blocks = parse_blocks(
            "- [x] done\n- [ ] next\n\n| Name | State |\n| --- | --- |\n| neenee | ready |",
        );

        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::ListItem {
                checked: Some(true),
                content,
                ..
            } if content == "done"
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::ListItem {
                checked: Some(false),
                content,
                ..
            } if content == "next"
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Text { content } if content.contains("│ Name │ State │")
                && content.contains("│ neenee │ ready │")
        )));
    }

    #[test]
    fn tool_step_collapses_and_restores_full_semantic_detail() {
        let mut message = ChatMessage::tool_step("read_file", r#"{"path":"README.md"}"#);
        assert!(message.raw.contains("read_file · running"));
        assert!(!message.raw.contains("```json"));

        assert!(message.finish_tool_step("read_file", "contents"));
        assert!(message.raw.contains("completed"));
        message.set_tool_step_expanded(true);

        assert!(message.raw.contains("```json"));
        assert!(message.raw.contains("contents"));
    }
}

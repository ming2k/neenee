//! Semantic document model for the TUI.
//!
//! Unlike storing raw strings, this model preserves the structure of messages
//! so that selection and copy operate on semantic units (blocks) rather than
//! terminal grid characters.

use neenee_core::{Role, SubTaskEvent};

#[derive(Debug, Clone, PartialEq)]
pub enum MessageKind {
    Chat,
    ToolStep {
        id: String,
        name: String,
        arguments: String,
        output: Option<String>,
        expanded: bool,
        duration_ms: Option<u64>,
        /// Child events emitted by a sub-agent spawned from this tool step.
        children: Vec<ChatMessage>,
    },
    Thinking {
        content: String,
        duration_ms: Option<u64>,
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

    pub fn tool_step(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        let mut message = Self {
            role: Role::Tool,
            blocks: Vec::new(),
            raw: String::new(),
            kind: MessageKind::ToolStep {
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
                output: None,
                expanded: false,
                duration_ms: None,
                children: Vec::new(),
            },
        };
        message.refresh_tool_step();
        message
    }

    pub fn finish_tool_step(
        &mut self,
        id: &str,
        output: impl Into<String>,
        duration_ms: u64,
    ) -> bool {
        let MessageKind::ToolStep {
            id: step_id,
            output: step_output,
            duration_ms: step_duration,
            ..
        } = &mut self.kind
        else {
            return false;
        };
        if step_id != id || step_output.is_some() {
            return false;
        }
        *step_output = Some(output.into());
        *step_duration = Some(duration_ms);
        self.refresh_tool_step();
        true
    }

    /// Append a sub-agent event as a nested child of this tool step.
    ///
    /// Returns `true` if this message is a tool step and the event was stored.
    pub fn push_subtask_event(&mut self, event: &SubTaskEvent) -> bool {
        let MessageKind::ToolStep { children, .. } = &mut self.kind else {
            return false;
        };
        match event {
            SubTaskEvent::StreamStart => {
                children.push(ChatMessage::new(Role::Assistant, ""));
            }
            SubTaskEvent::StreamDelta(delta) => {
                if let Some(last) = children.last_mut().filter(|m| {
                    m.role == Role::Assistant && matches!(m.kind, MessageKind::Chat)
                }) {
                    last.push_stream(delta);
                } else {
                    let mut msg = ChatMessage::new(Role::Assistant, "");
                    msg.push_stream(delta);
                    children.push(msg);
                }
            }
            SubTaskEvent::StreamEnd(content) => {
                if let Some(last) = children.last_mut().filter(|m| m.role == Role::Assistant) {
                    last.raw = content.clone();
                    last.reparse();
                } else {
                    children.push(ChatMessage::new(Role::Assistant, content.clone()));
                }
            }
            SubTaskEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                children.push(ChatMessage::tool_step(
                    id.clone(),
                    name.clone(),
                    arguments.clone(),
                ));
            }
            SubTaskEvent::ToolResult {
                id,
                output,
                duration_ms,
                ..
            } => {
                if let Some(child) = children.iter_mut().find(|m| {
                    m.is_tool_step()
                        && if let MessageKind::ToolStep {
                            id: step_id,
                            output: None,
                            ..
                        } = &m.kind
                        {
                            step_id == id
                        } else {
                            false
                        }
                }) {
                    child.finish_tool_step(id, output.clone(), *duration_ms);
                } else {
                    let mut msg = ChatMessage::tool_step(id.clone(), "tool", "{}");
                    msg.finish_tool_step(id, output.clone(), *duration_ms);
                    children.push(msg);
                }
            }
            SubTaskEvent::Activity(_) => {}
        }
        true
    }

    pub fn is_tool_step(&self) -> bool {
        matches!(self.kind, MessageKind::ToolStep { .. })
    }

    pub fn tool_step_expanded(&self) -> Option<bool> {
        match &self.kind {
            MessageKind::ToolStep { expanded, .. } => Some(*expanded),
            _ => None,
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

    pub fn thinking(content: impl Into<String>) -> Self {
        let content = content.into();
        let mut message = Self {
            role: Role::Assistant,
            blocks: Vec::new(),
            raw: String::new(),
            kind: MessageKind::Thinking {
                content: content.clone(),
                duration_ms: None,
                expanded: false,
            },
        };
        message.raw = content;
        message.blocks = parse_blocks(&message.raw);
        message
    }

    pub fn is_thinking(&self) -> bool {
        matches!(self.kind, MessageKind::Thinking { .. })
    }

    pub fn thinking_expanded(&self) -> Option<bool> {
        match &self.kind {
            MessageKind::Thinking { expanded, .. } => Some(*expanded),
            _ => None,
        }
    }

    pub fn set_thinking_expanded(&mut self, expanded: bool) {
        if let MessageKind::Thinking {
            expanded: current, ..
        } = &mut self.kind
        {
            *current = expanded;
        }
    }

    pub fn set_thinking_duration(&mut self, duration_ms: u64) {
        if let MessageKind::Thinking { duration_ms: d, .. } = &mut self.kind {
            *d = Some(duration_ms);
        }
    }

    /// Human-readable header for the thinking card (always one line).
    pub fn thinking_header(&self) -> Option<String> {
        let MessageKind::Thinking {
            content,
            duration_ms,
            ..
        } = &self.kind
        else {
            return None;
        };
        let chars = content.chars().count();
        Some(format!(
            "Thinking · {} · {} chars",
            duration_text(*duration_ms),
            chars
        ))
    }

    /// Human-readable header for the tool-step card (always one line).
    pub fn tool_step_header(&self) -> Option<String> {
        let MessageKind::ToolStep {
            name,
            arguments,
            output,
            duration_ms,
            ..
        } = &self.kind
        else {
            return None;
        };
        let status = match output {
            Some(output) if output.starts_with("Error") => "failed",
            Some(_) => "completed",
            None => "running",
        };
        Some(format!(
            "{} · {} · {} · {}",
            name,
            status,
            duration_text(*duration_ms),
            argument_summary(name, arguments)
        ))
    }

    fn refresh_tool_step(&mut self) {
        let MessageKind::ToolStep {
            id: _,
            name,
            arguments,
            output,
            expanded,
            duration_ms,
            children: _,
        } = &self.kind
        else {
            return;
        };
        self.raw = if *expanded {
            let arguments = pretty_json(arguments);
            match output {
                Some(output) => format!("```json\n{}\n```\n\nResult\n\n{}", arguments, output),
                None => format!("```json\n{}\n```", arguments),
            }
        } else {
            let status = match output {
                Some(output) if output.starts_with("Error") => "failed",
                Some(_) => "completed",
                None => "running",
            };
            format!(
                "⚒ {} · {} · {} · {}",
                name,
                status,
                duration_text(*duration_ms),
                argument_summary(name, arguments)
            )
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

fn duration_text(duration_ms: Option<u64>) -> String {
    match duration_ms {
        None => "...".to_string(),
        Some(ms) if ms < 1000 => format!("{}ms", ms),
        Some(ms) => format!("{:.1}s", ms as f64 / 1000.0),
    }
}

fn argument_summary(name: &str, arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return truncate(arguments, 72);
    };
    let Some(object) = value.as_object() else {
        return truncate(arguments, 72);
    };

    let get = |key: &str| object.get(key).and_then(serde_json::Value::as_str);

    let summary = match name {
        "read_file" => {
            if let Some(path) = get("path") {
                let mut s = format!("Read {}", path);
                if let Some(limit) = object.get("limit").and_then(|v| v.as_u64()) {
                    s.push_str(&format!(" [limit={}]", limit));
                }
                if let Some(offset) = object.get("offset").and_then(|v| v.as_u64()) {
                    s.push_str(&format!(" [offset={}]", offset));
                }
                s
            } else {
                "Read file".to_string()
            }
        }
        "write_file" => get("path")
            .map(|path| format!("Write {}", path))
            .unwrap_or_else(|| "Write file".to_string()),
        "edit_file" => get("path")
            .map(|path| format!("Edit {}", path))
            .unwrap_or_else(|| "Edit file".to_string()),
        "bash" => get("command")
            .map(|cmd| {
                let first = cmd.lines().next().unwrap_or(cmd);
                format!("Run {}", truncate(first, 64))
            })
            .unwrap_or_else(|| "Run command".to_string()),
        "grep" => {
            let pattern = get("pattern").unwrap_or("...");
            let path = get("path").unwrap_or(".");
            format!("Grep \"{}\" in {}", truncate(pattern, 48), path)
        }
        "glob" => get("pattern")
            .map(|pattern| format!("Glob {}", pattern))
            .unwrap_or_else(|| "Glob files".to_string()),
        "list_dir" => get("path")
            .map(|path| format!("List {}", path))
            .unwrap_or_else(|| "List directory".to_string()),
        "webfetch" => get("url")
            .map(|url| format!("Fetch {}", url))
            .unwrap_or_else(|| "Fetch URL".to_string()),
        "websearch" => get("query")
            .map(|query| format!("Search \"{}\"", truncate(query, 56)))
            .unwrap_or_else(|| "Web search".to_string()),
        "todo" => "Update todo list".to_string(),
        "task" => get("description")
            .map(|desc| format!("Task: {}", truncate(desc, 56)))
            .unwrap_or_else(|| "Run sub-task".to_string()),
        "create_project" => get("name")
            .map(|name| format!("Create project {}", name))
            .unwrap_or_else(|| "Create project".to_string()),
        "use_skill" => get("name")
            .map(|name| format!("Use skill {}", name))
            .unwrap_or_else(|| "Use skill".to_string()),
        "goal_checklist" => "Update goal checklist".to_string(),
        _ => ["path", "pattern", "command", "name", "url", "query"]
            .iter()
            .find_map(|key| get(key).map(|value| format!("{}={}", key, value)))
            .unwrap_or_else(|| arguments.to_string()),
    };
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
        let mut message = ChatMessage::tool_step("call_1", "read_file", r#"{"path":"README.md"}"#);
        assert!(message.raw.contains("read_file · running"));
        assert!(!message.raw.contains("```json"));

        assert!(message.finish_tool_step("call_1", "contents", 1234));
        assert!(message.raw.contains("completed"));
        message.set_tool_step_expanded(true);

        assert!(message.raw.contains("```json"));
        assert!(message.raw.contains("contents"));
    }
}

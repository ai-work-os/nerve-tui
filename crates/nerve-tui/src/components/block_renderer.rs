//! Block renderers — each ContentBlock type has its own render function.
//! Input: ContentBlock + terminal width → Output: Vec<Line<'static>>

use crate::theme;
use nerve_tui_protocol::{ContentBlock, ToolStatus};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, CodeBlockKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::LazyLock;
use regex::Regex;
use syntect::highlighting::{ThemeSet, Theme, Style as SynStyle};
use syntect::parsing::SyntaxSet;
use syntect::easy::HighlightLines;
use tracing::debug;
use unicode_width::UnicodeWidthStr;

/// Global syntax set (loaded once, reused across all renders).
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Global theme for code highlighting.
/// Auto-detect light/dark terminal via COLORFGBG env var.
static CODE_THEME: LazyLock<Theme> = LazyLock::new(|| {
    let ts = ThemeSet::load_defaults();
    let is_dark = std::env::var("COLORFGBG")
        .map(|v| {
            // Format: "fg;bg" — bg < 8 means dark background
            v.rsplit(';').next()
                .and_then(|bg| bg.parse::<u32>().ok())
                .map(|bg| bg < 8)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let theme_name = if is_dark { "base16-ocean.dark" } else { "base16-ocean.light" };
    ts.themes.get(theme_name)
        .cloned()
        .unwrap_or_else(|| ts.themes["base16-ocean.dark"].clone())
});

/// Maximum lines to show in collapsed tool results.
const TOOL_RESULT_MAX_LINES: usize = 10;
/// Maximum lines to show in expanded tool results.
const TOOL_RESULT_EXPANDED_MAX: usize = 50;

/// Strip known system XML tags and their content from text.
/// Two passes: first remove paired tags with content, then orphan opening/closing tags.
fn sanitize_content(text: &str) -> String {
    static RE_PAIRED: LazyLock<Regex> = LazyLock::new(|| {
        // The `regex` crate does not support backreferences — enumerate each tag pair explicitly.
        Regex::new(
            r"(?s)(?:<system-reminder>.*?</system-reminder>|<persisted-output>.*?</persisted-output>|<thinking>.*?</thinking>|<command-name>.*?</command-name>|<artifact>.*?</artifact>|<EXTREMELY_IMPORTANT>.*?</EXTREMELY_IMPORTANT>|<SUBAGENT-STOP>.*?</SUBAGENT-STOP>|<EXTREMELY-IMPORTANT>.*?</EXTREMELY-IMPORTANT>)"
        ).unwrap()
    });
    let cleaned = RE_PAIRED.replace_all(text, "");
    static RE_ORPHAN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"</?(?:system-reminder|persisted-output|antml:thinking|command-name|artifact|EXTREMELY_IMPORTANT|SUBAGENT-STOP|EXTREMELY-IMPORTANT)>").unwrap()
    });
    RE_ORPHAN.replace_all(&cleaned, "").into_owned()
}

/// Extract a one-line summary from tool input based on tool name.
/// Parses input as JSON and picks the most relevant field for display.
fn extract_tool_summary(name: &str, input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }

    let val: serde_json::Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => {
            // Not JSON — return raw input, truncated
            return truncate_str(input, 40);
        }
    };

    let obj = match val.as_object() {
        Some(o) => o,
        None => return truncate_str(input, 40),
    };

    let get_str = |key: &str| -> Option<String> {
        obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
    };

    match name {
        "Bash" => {
            if let Some(cmd) = get_str("command") {
                truncate_str(&cmd, 60)
            } else {
                truncate_str(input, 40)
            }
        }
        "Read" => {
            if let Some(path) = get_str("file_path") {
                let offset = obj.get("offset").and_then(|v| v.as_u64());
                let limit = obj.get("limit").and_then(|v| v.as_u64());
                match (offset, limit) {
                    (Some(o), Some(l)) => format!("{} ({}-{})", path, o, o + l),
                    (Some(o), None) => format!("{} ({}-)", path, o),
                    _ => path,
                }
            } else {
                truncate_str(input, 40)
            }
        }
        "Edit" | "Write" => {
            get_str("file_path").unwrap_or_else(|| truncate_str(input, 40))
        }
        "WebSearch" => {
            get_str("query").unwrap_or_else(|| truncate_str(input, 40))
        }
        "WebFetch" => {
            get_str("url").unwrap_or_else(|| truncate_str(input, 40))
        }
        "Agent" => {
            get_str("description").unwrap_or_else(|| truncate_str(input, 40))
        }
        _ => {
            truncate_str(input, 40)
        }
    }
}

/// Truncate a string to `max_len` characters, appending "..." if truncated.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

/// Render any ContentBlock to styled lines (full expanded view).
pub fn render_block(block: &ContentBlock, width: u16) -> Vec<Line<'static>> {
    render_block_inner(block, width, false)
}

/// Render any ContentBlock in collapsed mode (one-line summary for thinking/tool_call/tool_result).
pub fn render_block_collapsed(block: &ContentBlock, width: u16) -> Vec<Line<'static>> {
    render_block_inner(block, width, true)
}

fn render_block_inner(block: &ContentBlock, width: u16, collapsed: bool) -> Vec<Line<'static>> {
    debug!(block_type = block.kind(), width, collapsed, "rendering content block");
    match block {
        ContentBlock::Text { text } => render_text(text, width),
        ContentBlock::Thinking { text, started_at, finished_at } => {
            let elapsed = match (started_at, finished_at) {
                (Some(s), Some(f)) => Some(f.duration_since(*s)),
                (Some(s), None) => Some(s.elapsed()),
                _ => None,
            };
            render_thinking(text, elapsed, collapsed)
        }
        ContentBlock::ToolCall { id: _, name, input, status } => {
            render_tool_call(name, input, status, collapsed)
        }
        ContentBlock::ToolResult { tool_call_id: _, content, is_error } => {
            render_tool_result(content, *is_error, collapsed)
        }
        ContentBlock::Error { message } => render_error(message),
    }
}

/// Render a ContentBlock in summary mode.
/// Text: full render (including code blocks). ToolCall: one-line summary.
/// Thinking & ToolResult: hidden. Error: shown.
pub fn render_block_summary(block: &ContentBlock, width: u16) -> Vec<Line<'static>> {
    match block {
        ContentBlock::Text { text } => render_text(text, width),
        ContentBlock::ToolCall { name, input, status, .. } => {
            render_tool_call_summary(name, input, status)
        }
        ContentBlock::Thinking { .. } => vec![],
        ContentBlock::ToolResult { .. } => vec![],
        ContentBlock::Error { message } => render_error(message),
    }
}

// ---------------------------------------------------------------------------
// Text block: pulldown-cmark + syntect for code highlighting
// ---------------------------------------------------------------------------

fn render_text(content: &str, _width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let cleaned = sanitize_content(content);
    render_markdown(&cleaned, &mut out, false);
    out
}


fn render_markdown(content: &str, out: &mut Vec<Line<'static>>, skip_code_blocks: bool) {
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(content, opts);

    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();
    let mut is_heading = false;
    let mut bold = false;
    let mut italic = false;
    let mut list_item_pending = false;
    // Table state
    let mut in_table = false;
    let mut table_row: Vec<String> = Vec::new();
    let mut table_cell_buf = String::new();
    let mut in_table_head = false;
    let mut table_rows: Vec<(Vec<String>, bool)> = Vec::new(); // (cells, is_header)
    let mut in_skipped_code = false;

    for event in parser {
        // In summary mode, skip code block events entirely
        if in_skipped_code {
            if matches!(event, Event::End(TagEnd::CodeBlock)) {
                in_skipped_code = false;
            }
            continue;
        }

        match event {
            Event::Start(Tag::CodeBlock(kind)) if skip_code_blocks => {
                // Flush current line before skipping
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
                in_skipped_code = true;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                // Flush current line
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    _ => String::new(),
                };
                code_buf.clear();
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                // Render accumulated code with syntect
                let highlighted = highlight_code(&code_buf, &code_lang);
                out.extend(highlighted);
                in_code_block = false;
                code_buf.clear();
                code_lang.clear();
            }
            Event::Start(Tag::Heading { .. }) => {
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
                is_heading = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                let text: String = current_spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect();
                current_spans.clear();
                out.push(Line::from(Span::styled(
                    text,
                    Style::default()
                        .fg(theme::PRIMARY)
                        .add_modifier(Modifier::BOLD),
                )));
                is_heading = false;
            }
            Event::Start(Tag::Paragraph) => {
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
                if !out.is_empty() && !in_code_block {
                    out.push(Line::from(""));
                }
            }
            Event::End(TagEnd::Paragraph) => {
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
            }
            Event::Start(Tag::List(_)) => {}
            Event::End(TagEnd::List(_)) => {}
            Event::Start(Tag::Item) => {
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
                list_item_pending = true;
            }
            Event::End(TagEnd::Item) => {
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
            }
            Event::Start(Tag::Strong) => bold = true,
            Event::End(TagEnd::Strong) => bold = false,
            Event::Start(Tag::Emphasis) => italic = true,
            Event::End(TagEnd::Emphasis) => italic = false,
            Event::Code(text) => {
                if in_table {
                    table_cell_buf.push_str(&text);
                } else {
                    current_spans.push(Span::styled(
                        text.to_string(),
                        Style::default().fg(theme::WARNING),
                    ));
                }
            }
            Event::Text(text) => {
                if in_code_block {
                    code_buf.push_str(&text);
                } else if in_table {
                    table_cell_buf.push_str(&text);
                } else if is_heading {
                    current_spans.push(Span::raw(text.to_string()));
                } else {
                    if list_item_pending {
                        current_spans.push(Span::raw("  • ".to_string()));
                        list_item_pending = false;
                    }
                    let style = match (bold, italic) {
                        (true, true) => Style::default()
                            .add_modifier(Modifier::BOLD)
                            .add_modifier(Modifier::ITALIC),
                        (true, false) => Style::default().add_modifier(Modifier::BOLD),
                        (false, true) => Style::default().add_modifier(Modifier::ITALIC),
                        _ => Style::default(),
                    };
                    // Highlight @mentions in unstyled text
                    if text.contains('@') && !bold && !italic {
                        current_spans.extend(highlight_mentions(&text));
                    } else {
                        current_spans.push(Span::styled(text.to_string(), style));
                    }
                }
            }
            Event::SoftBreak => {
                if in_code_block {
                    code_buf.push('\n');
                } else {
                    current_spans.push(Span::raw(" ".to_string()));
                }
            }
            Event::HardBreak => {
                if in_code_block {
                    code_buf.push('\n');
                } else {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
            }
            Event::Start(Tag::Table(_alignments)) => {
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
                in_table = true;
                table_rows.clear();
            }
            Event::End(TagEnd::Table) => {
                in_table = false;
                // Render collected table rows
                if !table_rows.is_empty() {
                    let rendered = render_table(&table_rows);
                    out.extend(rendered);
                }
                table_rows.clear();
            }
            Event::Start(Tag::TableHead) => {
                in_table_head = true;
                table_row.clear();
            }
            Event::End(TagEnd::TableHead) => {
                table_rows.push((std::mem::take(&mut table_row), true));
                in_table_head = false;
            }
            Event::Start(Tag::TableRow) => {
                table_row.clear();
            }
            Event::End(TagEnd::TableRow) => {
                if !in_table_head {
                    table_rows.push((std::mem::take(&mut table_row), false));
                }
            }
            Event::Start(Tag::TableCell) => {
                table_cell_buf.clear();
            }
            Event::End(TagEnd::TableCell) => {
                table_row.push(std::mem::take(&mut table_cell_buf));
            }
            _ => {}
        }
    }
    if !current_spans.is_empty() {
        out.push(Line::from(current_spans));
    }
}

/// Render a table from collected rows into styled lines.
///
/// Uses `UnicodeWidthStr::width()` for correct CJK alignment and manual padding.
fn render_table(table_rows: &[(Vec<String>, bool)]) -> Vec<Line<'static>> {
    let col_count = table_rows.iter().map(|(cells, _)| cells.len()).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }

    // Calculate column widths using display width (not byte length)
    let mut col_widths = vec![0usize; col_count];
    for (cells, _) in table_rows {
        for (i, cell) in cells.iter().enumerate() {
            if i < col_count {
                let w = UnicodeWidthStr::width(cell.as_str());
                col_widths[i] = col_widths[i].max(w);
            }
        }
    }

    debug!(?col_widths, col_count, row_count = table_rows.len(), "rendering table");

    let mut out = Vec::new();
    let border_style = Style::default().fg(Color::DarkGray);

    for (row_idx, (cells, is_header)) in table_rows.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled("│ ", border_style));
        for (i, cell) in cells.iter().enumerate() {
            let target_w = if i < col_widths.len() { col_widths[i] } else { UnicodeWidthStr::width(cell.as_str()) };
            let display_w = UnicodeWidthStr::width(cell.as_str());
            let pad = target_w.saturating_sub(display_w);
            let padded = format!("{}{}", cell, " ".repeat(pad));
            let style = if *is_header {
                Style::default().fg(theme::PRIMARY).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            spans.push(Span::styled(padded, style));
            spans.push(Span::styled(" │ ", border_style));
        }
        out.push(Line::from(spans));

        // Separator after header
        if *is_header && row_idx == 0 {
            let mut sep_spans: Vec<Span<'static>> = Vec::new();
            sep_spans.push(Span::styled("├─", border_style));
            for (i, w) in col_widths.iter().enumerate() {
                sep_spans.push(Span::styled("─".repeat(*w), border_style));
                if i < col_widths.len() - 1 {
                    sep_spans.push(Span::styled("─┼─", border_style));
                }
            }
            sep_spans.push(Span::styled("─┤", border_style));
            out.push(Line::from(sep_spans));
        }
    }

    out
}

/// Highlight code using syntect. Falls back to plain muted text on unknown language.
fn highlight_code(code: &str, lang: &str) -> Vec<Line<'static>> {
    let ss = &*SYNTAX_SET;
    let theme = &*CODE_THEME;

    let syntax = if lang.is_empty() {
        ss.find_syntax_plain_text()
    } else {
        ss.find_syntax_by_token(lang)
            .unwrap_or_else(|| ss.find_syntax_plain_text())
    };

    let mut h = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();

    // Language label (small, muted) — no separator lines
    if !lang.is_empty() {
        lines.push(Line::from(Span::styled(
            lang.to_string(),
            Style::default().fg(theme::TEXT_MUTED).bg(theme::BG_L1),
        )));
    }

    for line in code.lines() {
        match h.highlight_line(line, &ss) {
            Ok(ranges) => {
                let spans: Vec<Span<'static>> = ranges
                    .into_iter()
                    .map(|(syn_style, text)| {
                        Span::styled(
                            text.to_string(),
                            syntect_style_to_ratatui(syn_style),
                        )
                    })
                    .collect();
                lines.push(Line::from(spans));
            }
            Err(_) => {
                lines.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(theme::TEXT_MUTED).bg(theme::BG_L1),
                )));
            }
        }
    }

    lines
}

fn syntect_style_to_ratatui(syn: SynStyle) -> Style {
    let fg = Color::Rgb(syn.foreground.r, syn.foreground.g, syn.foreground.b);
    Style::default().fg(fg).bg(theme::BG_L1)
}

// ---------------------------------------------------------------------------
// Thinking block: collapsed by default, shows timer
// ---------------------------------------------------------------------------

fn render_thinking(text: &str, elapsed: Option<std::time::Duration>, collapsed: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let timer = elapsed
        .map(|d| format!("{:.1}s", d.as_secs_f64()))
        .unwrap_or_else(|| "…".to_string());

    let header = format!("  💭 思考中 ({})", timer);
    lines.push(Line::from(Span::styled(
        header,
        Style::default()
            .fg(theme::BORDER_ACTIVE)
            .add_modifier(Modifier::ITALIC),
    )));

    if !collapsed {
        for line in text.lines() {
            lines.push(Line::from(Span::styled(
                format!("  │ {}", line),
                Style::default().fg(theme::BORDER_ACTIVE),
            )));
        }
    }

    lines
}

// ---------------------------------------------------------------------------
// ToolCall block: tool name + status icon + collapsible args
// ---------------------------------------------------------------------------

fn render_tool_call(name: &str, input: &str, status: &ToolStatus, collapsed: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let (icon, icon_color) = match status {
        ToolStatus::Pending => ("⏳", theme::WARNING),
        ToolStatus::Running => ("⏳", theme::SUCCESS),
        ToolStatus::Completed => ("✓", theme::SUCCESS),
        ToolStatus::Failed => ("✗", theme::ERROR),
    };

    debug!(tool_name = name, ?status, collapsed, "rendering tool_call block");

    if collapsed {
        // Compact one-line: icon + name + ": " + summary
        let summary = extract_tool_summary(name, input);
        let mut spans = vec![
            Span::styled(format!("  {} ", icon), Style::default().fg(icon_color)),
            Span::styled(
                name.to_string(),
                Style::default()
                    .fg(theme::TOOL_NAME)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if !summary.is_empty() {
            spans.push(Span::styled(
                format!(": {}", summary),
                Style::default().fg(theme::TOOL_VALUE),
            ));
        }
        lines.push(Line::from(spans));
    } else {
        // Header: icon + tool name
        lines.push(Line::from(vec![
            Span::styled(format!("  {} ", icon), Style::default().fg(icon_color)),
            Span::styled(
                name.to_string(),
                Style::default()
                    .fg(theme::TOOL_NAME)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        // Show input args when expanded
        if !input.is_empty() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(input) {
                if let Some(obj) = val.as_object() {
                    for (k, v) in obj {
                        let v_str = match v {
                            serde_json::Value::String(s) => s.clone(),
                            _ => v.to_string(),
                        };
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("    {}: ", k),
                                Style::default().fg(theme::TOOL_KEY),
                            ),
                            Span::styled(v_str, Style::default().fg(theme::TOOL_VALUE)),
                        ]));
                    }
                }
            } else {
                // Plain string input
                for line in input.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("    {}", line),
                        Style::default().fg(theme::TOOL_VALUE),
                    )));
                }
            }
        }
    }

    lines
}

/// Render a ToolCall as a single summary line (for summary_mode).
fn render_tool_call_summary(name: &str, input: &str, status: &ToolStatus) -> Vec<Line<'static>> {
    let (icon, icon_color) = match status {
        ToolStatus::Pending => ("⏳", theme::WARNING),
        ToolStatus::Running => ("⏳", theme::SUCCESS),
        ToolStatus::Completed => ("✓", theme::SUCCESS),
        ToolStatus::Failed => ("✗", theme::ERROR),
    };

    let summary = extract_tool_summary(name, input);
    let mut spans = vec![
        Span::styled(format!("  {} ", icon), Style::default().fg(icon_color)),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(theme::TOOL_NAME)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !summary.is_empty() {
        spans.push(Span::styled(
            format!(": {}", summary),
            Style::default().fg(theme::TOOL_VALUE),
        ));
    }

    vec![Line::from(spans)]
}

// ---------------------------------------------------------------------------
// ToolResult block: shows result content with error styling
// ---------------------------------------------------------------------------

fn render_tool_result(content: &str, is_error: bool, collapsed: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let content_lines: Vec<&str> = content.lines().collect();
    let line_count = content_lines.len();

    let (label, label_color) = if is_error {
        ("  ✗ 结果（错误）".to_string(), theme::ERROR)
    } else if line_count > TOOL_RESULT_MAX_LINES {
        (format!("  ✓ 结果 ({} 行)", line_count), theme::TEXT_MUTED)
    } else {
        ("  ✓ 结果".to_string(), theme::TEXT_MUTED)
    };

    lines.push(Line::from(Span::styled(
        label,
        Style::default().fg(label_color),
    )));

    let content_color = if is_error {
        theme::ERROR
    } else {
        theme::TEXT
    };

    // Error content is never truncated
    if is_error {
        for line in content.lines() {
            lines.push(Line::from(Span::styled(
                format!("    {}", line),
                Style::default().fg(content_color),
            )));
        }
        return lines;
    }

    let (max_lines, head_count, tail_count) = if collapsed {
        (TOOL_RESULT_MAX_LINES, 5usize, 2usize)
    } else {
        (TOOL_RESULT_EXPANDED_MAX, 25usize, 5usize)
    };

    if line_count <= max_lines {
        // Show all lines
        for line in &content_lines {
            lines.push(Line::from(Span::styled(
                format!("    {}", line),
                Style::default().fg(content_color),
            )));
        }
    } else {
        // Head
        for line in &content_lines[..head_count] {
            lines.push(Line::from(Span::styled(
                format!("    {}", line),
                Style::default().fg(content_color),
            )));
        }
        // Ellipsis
        let hidden = line_count - head_count - tail_count;
        lines.push(Line::from(Span::styled(
            format!("    \u{2026} +{} lines", hidden),
            Style::default().fg(theme::TEXT_MUTED),
        )));
        // Tail
        for line in &content_lines[line_count - tail_count..] {
            lines.push(Line::from(Span::styled(
                format!("    {}", line),
                Style::default().fg(content_color),
            )));
        }
    }

    lines
}

// ---------------------------------------------------------------------------
// Error block
// ---------------------------------------------------------------------------

fn render_error(message: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        format!("  ⚠ {}", message),
        Style::default().fg(theme::ERROR).add_modifier(Modifier::BOLD),
    ))]
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Highlight @mentions in text, returning styled spans.
/// Non-mention text gets default style, @name gets MENTION color + bold.
pub(crate) fn highlight_mentions(text: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;

    while let Some(idx) = rest.find('@') {
        if idx > 0 {
            spans.push(Span::raw(rest[..idx].to_string()));
        }
        let after_at = &rest[idx + 1..];
        let end = after_at
            .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.')
            .unwrap_or(after_at.len());
        if end > 0 {
            spans.push(Span::styled(
                rest[idx..idx + 1 + end].to_string(),
                Style::default()
                    .fg(theme::MENTION)
                    .add_modifier(Modifier::BOLD),
            ));
            rest = &rest[idx + 1 + end..];
        } else {
            spans.push(Span::raw("@".to_string()));
            rest = after_at;
        }
    }
    if !rest.is_empty() {
        spans.push(Span::raw(rest.to_string()));
    }
    if spans.is_empty() {
        spans.push(Span::raw(text.to_string()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_tui_protocol::{ContentBlock, ToolStatus};
    use std::time::{Duration, Instant};

    // --- Text block tests ---

    #[test]
    fn text_plain_renders() {
        let lines = render_text("Hello world", 80);
        assert!(!lines.is_empty());
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("Hello world"));
    }

    #[test]
    fn text_markdown_bold() {
        let lines = render_text("**bold text**", 80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("bold text"));
        // Check bold modifier on the span
        let bold_span = lines.iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "bold text");
        assert!(bold_span.is_some());
        assert!(bold_span.unwrap().style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn text_code_block_uses_syntect() {
        let md = "```rust\nfn main() {}\n```";
        let lines = render_text(md, 80);
        // Should have language label line
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("rust"), "should show language label");
        assert!(text.contains("fn"), "should contain code");
    }

    #[test]
    fn text_code_block_no_lang_fallback() {
        let md = "```\nsome code\n```";
        let lines = render_text(md, 80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("some code"));
    }

    #[test]
    fn text_unclosed_code_block_handled() {
        // CommonMark: EOF implicitly closes code block
        let md = "```python\nprint('hello')";
        let lines = render_text(md, 80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("print"));
        // Should not leak code styling into subsequent text
    }

    #[test]
    fn text_heading() {
        let lines = render_text("# Title", 80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("Title"));
        // Check heading style (Cyan + Bold)
        let heading_span = lines.iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "Title");
        assert!(heading_span.is_some());
        assert_eq!(heading_span.unwrap().style.fg, Some(theme::PRIMARY));
    }

    #[test]
    fn text_list_items() {
        let lines = render_text("- item one\n- item two", 80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("•"));
        assert!(text.contains("item one"));
        assert!(text.contains("item two"));
    }

    #[test]
    fn text_inline_code() {
        let lines = render_text("Use `foo()` here", 80);
        let code_span = lines.iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "foo()");
        assert!(code_span.is_some());
        assert_eq!(code_span.unwrap().style.fg, Some(theme::WARNING));
    }

    // --- Thinking block tests ---

    #[test]
    fn thinking_with_elapsed() {
        let elapsed = Some(Duration::from_secs_f64(2.5));
        let lines = render_thinking("I need to check the file...", elapsed, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("思考中"));
        assert!(text.contains("2.5s"));
        assert!(text.contains("check the file"));
    }

    #[test]
    fn thinking_no_elapsed() {
        let lines = render_thinking("step 1", None, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("…"));
    }

    #[test]
    fn thinking_shows_all_content() {
        let long = (0..10).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_thinking(&long, Some(Duration::from_secs(1)), false);
        // Header + 10 content lines = 11
        assert_eq!(lines.len(), 11);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("思考中"));
        assert!(text.contains("line 0"));
        assert!(text.contains("line 9"));
    }

    // --- ToolCall block tests ---

    #[test]
    fn tool_call_pending() {
        let lines = render_tool_call("Read", "{}", &ToolStatus::Pending, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("⏳"));
        assert!(text.contains("Read"));
    }

    #[test]
    fn tool_call_completed() {
        let lines = render_tool_call("Edit", "{}", &ToolStatus::Completed, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("✓"));
        assert!(text.contains("Edit"));
    }

    #[test]
    fn tool_call_failed() {
        let lines = render_tool_call("Bash", "{}", &ToolStatus::Failed, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("✗"));
    }

    #[test]
    fn tool_call_shows_all_args() {
        let input = r#"{"path": "/tmp/test.rs", "content": "fn main() {}"}"#;
        let lines = render_tool_call("Write", input, &ToolStatus::Running, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("Write"));
        assert!(text.contains("path"));
        assert!(text.contains("/tmp/test.rs"));
    }

    #[test]
    fn tool_call_many_args_all_shown() {
        let input = r#"{"a":"1","b":"2","c":"3","d":"4","e":"5"}"#;
        let lines = render_tool_call("Foo", input, &ToolStatus::Pending, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("a:"));
        assert!(text.contains("e:"));
        // Header + 5 args = 6
        assert_eq!(lines.len(), 6);
    }

    // --- ToolResult block tests ---

    #[test]
    fn tool_result_success_shows_content() {
        let lines = render_tool_result("file1.txt\nfile2.txt", false, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("✓ 结果"));
        assert!(text.contains("file1.txt"));
        // Header + 2 content lines = 3
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn tool_result_error_shows_content() {
        let lines = render_tool_result("command not found", true, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("✗ 结果（错误）"));
        assert!(text.contains("command not found"));
        // Check red color on content
        let error_span = lines.iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("command not found"));
        assert_eq!(error_span.unwrap().style.fg, Some(theme::ERROR));
    }

    #[test]
    fn tool_result_long_shows_all() {
        let content = (0..10).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, false, false);
        // Header + 10 content lines = 11
        assert_eq!(lines.len(), 11);
    }

    #[test]
    fn tool_result_always_shows_all() {
        let content = (0..10).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, false, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("line 0"));
        assert!(text.contains("line 9"));
    }

    #[test]
    fn render_block_shows_content() {
        let block = ContentBlock::Thinking {
            text: "long thought".into(),
            started_at: Some(Instant::now()),
            finished_at: Some(Instant::now()),
        };
        let lines = render_block(&block, 80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("long thought"));
    }

    // --- Error block tests ---

    #[test]
    fn error_block_renders() {
        let lines = render_error("something went wrong");
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("⚠"));
        assert!(text.contains("something went wrong"));
        assert_eq!(lines[0].spans[0].style.fg, Some(theme::ERROR));
    }

    // --- render_block dispatch tests ---

    #[test]
    fn render_block_dispatches_text() {
        let block = ContentBlock::Text { text: "hello".into() };
        let lines = render_block(&block, 80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn render_block_dispatches_thinking() {
        let now = Instant::now();
        let block = ContentBlock::Thinking {
            text: "thinking...".into(),
            started_at: Some(now),
            finished_at: None,
        };
        let lines = render_block(&block, 80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("思考中"));
    }

    #[test]
    fn render_block_dispatches_tool_call() {
        let block = ContentBlock::ToolCall {
            id: "tc1".into(),
            name: "Bash".into(),
            input: "{}".into(),
            status: ToolStatus::Completed,
        };
        let lines = render_block(&block, 80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("Bash"));
    }

    #[test]
    fn render_block_dispatches_error() {
        let block = ContentBlock::Error { message: "oops".into() };
        let lines = render_block(&block, 80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("oops"));
    }

    // --- Syntect integration tests ---

    #[test]
    fn highlight_code_rust() {
        let lines = highlight_code("fn main() {\n    println!(\"hello\");\n}", "rust");
        // lang label + 3 code lines = 4
        assert!(lines.len() >= 4);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("rust"));
        assert!(text.contains("fn"));
        assert!(text.contains("println"));
    }

    #[test]
    fn highlight_code_unknown_lang_fallback() {
        let lines = highlight_code("some stuff", "nonexistent_lang");
        // lang label + 1 line = 2
        assert!(lines.len() >= 2);
    }

    // --- Table rendering tests ---

    /// Helper: extract all text from rendered lines.
    fn lines_to_text(lines: &[Line<'_>]) -> String {
        lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect()
    }

    /// Helper: extract text of a single line (for alignment checks).
    fn line_to_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn table_english_content() {
        let md = "| Name | Age |\n|------|-----|\n| Alice | 30 |\n| Bob | 25 |";
        let lines = render_text(md, 80);
        let text = lines_to_text(&lines);
        assert!(text.contains("Name"), "header should contain Name");
        assert!(text.contains("Age"), "header should contain Age");
        assert!(text.contains("Alice"), "body should contain Alice");
        assert!(text.contains("30"), "body should contain 30");
        assert!(text.contains("Bob"), "body should contain Bob");
        assert!(text.contains("│"), "should have table borders");
        assert!(text.contains("├"), "should have separator");
        assert!(text.contains("─"), "should have horizontal lines");
    }

    #[test]
    fn table_english_header_style() {
        let md = "| Name | Age |\n|------|-----|\n| Alice | 30 |";
        let lines = render_text(md, 80);
        // Find span with "Name" — should be Cyan+Bold (header)
        let header_span = lines.iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("Name"));
        assert!(header_span.is_some(), "should find Name span");
        let span = header_span.unwrap();
        assert_eq!(span.style.fg, Some(theme::PRIMARY));
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn table_chinese_content() {
        let md = "| 字段 | 说明 |\n|------|------|\n| 名称 | 用户名 |\n| 年龄 | 用户年龄 |";
        let lines = render_text(md, 80);
        let text = lines_to_text(&lines);
        assert!(text.contains("字段"), "header should contain 字段");
        assert!(text.contains("说明"), "header should contain 说明");
        assert!(text.contains("名称"), "body should contain 名称");
        assert!(text.contains("用户名"), "body should contain 用户名");
        assert!(text.contains("用户年龄"), "body should contain 用户年龄");
    }

    #[test]
    fn table_chinese_alignment() {
        let md = "| 字段 | 说明 |\n|------|------|\n| 名称 | 用户名 |\n| 年龄 | 用户年龄 |";
        let lines = render_text(md, 80);
        // Collect data row lines (skip header and separator)
        // Header is line 0, separator is line 1, data rows are 2+
        // Each row should have the same display width for each column
        // The widest in col 0 is "字段"/"名称"/"年龄" = all 4 display width
        // The widest in col 1 is "用户年龄" = 8 display width
        // So "用户名" (6 display width) should be padded to 8
        let data_lines: Vec<&Line> = lines.iter()
            .filter(|l| {
                let t = line_to_text(l);
                t.contains("│") && !t.contains("├")
            })
            .collect();
        assert!(data_lines.len() >= 3, "should have header + 2 data rows");

        // All data row lines should have the same display width
        let widths: Vec<usize> = data_lines.iter()
            .map(|l| UnicodeWidthStr::width(line_to_text(l).as_str()))
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]),
            "all table rows should have same display width, got {:?}", widths);
    }

    #[test]
    fn table_mixed_content() {
        let md = "| Field | 说明 | Required |\n|-------|------|----------|\n| toolCallId | 调用 ID | Yes |";
        let lines = render_text(md, 80);
        let text = lines_to_text(&lines);
        assert!(text.contains("Field"), "header should contain Field");
        assert!(text.contains("说明"), "header should contain 说明");
        assert!(text.contains("Required"), "header should contain Required");
        assert!(text.contains("toolCallId"), "body should contain toolCallId");
        assert!(text.contains("调用 ID"), "body should contain 调用 ID");
        assert!(text.contains("Yes"), "body should contain Yes");
    }

    #[test]
    fn table_mixed_alignment() {
        let md = "| Field | 说明 | Required |\n|-------|------|----------|\n| toolCallId | 调用 ID | Yes |";
        let lines = render_text(md, 80);
        let data_lines: Vec<&Line> = lines.iter()
            .filter(|l| {
                let t = line_to_text(l);
                t.contains("│") && !t.contains("├")
            })
            .collect();
        assert!(data_lines.len() >= 2, "should have header + 1 data row");

        let widths: Vec<usize> = data_lines.iter()
            .map(|l| UnicodeWidthStr::width(line_to_text(l).as_str()))
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]),
            "all table rows should have same display width, got {:?}", widths);
    }

    #[test]
    fn table_separator_line() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let lines = render_text(md, 80);
        let sep_line = lines.iter()
            .find(|l| line_to_text(l).contains("├"));
        assert!(sep_line.is_some(), "should have a separator line");
        let sep_text = line_to_text(sep_line.unwrap());
        assert!(sep_text.contains("┼"), "separator should have cross joints");
        assert!(sep_text.contains("┤"), "separator should have right end");
    }

    #[test]
    fn render_table_unit_chinese_padding() {
        // Directly test render_table with known data
        let rows = vec![
            (vec!["字段".to_string(), "说明".to_string()], true),
            (vec!["名称".to_string(), "用户年龄".to_string()], false),
        ];
        let lines = render_table(&rows);
        // "字段" width=4, "名称" width=4 → col0 = 4
        // "说明" width=4, "用户年龄" width=8 → col1 = 8
        // Header "说明" should be padded to 8 display width: "说明" + 4 spaces
        let header_text = line_to_text(&lines[0]);
        let body_text = line_to_text(&lines[2]); // skip separator at [1]
        let header_w = UnicodeWidthStr::width(header_text.as_str());
        let body_w = UnicodeWidthStr::width(body_text.as_str());
        assert_eq!(header_w, body_w,
            "header and body should have same display width: header={}, body={}", header_w, body_w);
    }

    // --- Table with backtick code in cells ---

    #[test]
    fn table_backtick_code_in_cells() {
        let md = "| 文件 | 用途 |\n|------|------|\n| `app.rs` | 应用主循环 |\n| `lib.rs` | 库入口 |";
        let lines = render_text(md, 80);
        // Content must appear inside table row lines (lines containing │), not leaked outside
        let table_row_lines: Vec<String> = lines.iter()
            .map(|l| line_to_text(l))
            .filter(|t| t.contains("│") && !t.contains("├"))
            .collect();
        let table_text: String = table_row_lines.join("\n");
        assert!(table_text.contains("app.rs"), "backtick content 'app.rs' should be inside table rows, got:\n{}", table_text);
        assert!(table_text.contains("lib.rs"), "backtick content 'lib.rs' should be inside table rows, got:\n{}", table_text);
    }

    // --- @mention highlighting tests ---

    #[test]
    fn mention_highlight_single() {
        let spans = highlight_mentions("hello @alice world");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content.as_ref(), "hello ");
        assert_eq!(spans[1].content.as_ref(), "@alice");
        assert_eq!(spans[1].style.fg, Some(theme::MENTION));
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[2].content.as_ref(), " world");
    }

    #[test]
    fn mention_highlight_at_start() {
        let spans = highlight_mentions("@bob hi");
        assert_eq!(spans[0].content.as_ref(), "@bob");
        assert_eq!(spans[0].style.fg, Some(theme::MENTION));
    }

    #[test]
    fn mention_highlight_multiple() {
        let spans = highlight_mentions("@alice and @bob");
        let mention_count = spans.iter().filter(|s| s.style.fg == Some(theme::MENTION)).count();
        assert_eq!(mention_count, 2);
    }

    #[test]
    fn mention_highlight_bare_at() {
        let spans = highlight_mentions("email@ test");
        // bare @ with no following alphanumeric
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("@"));
    }

    #[test]
    fn mention_highlight_no_mention() {
        let spans = highlight_mentions("no mentions here");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "no mentions here");
    }

    #[test]
    fn mention_in_markdown_text() {
        // Verify @mention highlighting works through render_text
        let lines = render_text("Hello @alice, check this", 80);
        let all_spans: Vec<&Span> = lines.iter().flat_map(|l| l.spans.iter()).collect();
        let mention = all_spans.iter().find(|s| s.content.as_ref() == "@alice");
        assert!(mention.is_some(), "should find @alice span");
        assert_eq!(mention.unwrap().style.fg, Some(theme::MENTION));
    }

    // --- Collapsed rendering tests ---

    #[test]
    fn thinking_collapsed_shows_header_only() {
        let elapsed = Some(Duration::from_secs_f64(2.5));
        let lines = render_thinking("line 1\nline 2\nline 3", elapsed, true);
        // Collapsed: only header, no content lines
        assert_eq!(lines.len(), 1);
        let text = lines_to_text(&lines);
        assert!(text.contains("思考中"));
        assert!(text.contains("2.5s"));
        assert!(!text.contains("line 1"));
    }

    #[test]
    fn thinking_expanded_shows_all() {
        let elapsed = Some(Duration::from_secs_f64(1.0));
        let lines = render_thinking("line 1\nline 2", elapsed, false);
        // Expanded: header + 2 content lines = 3
        assert_eq!(lines.len(), 3);
        let text = lines_to_text(&lines);
        assert!(text.contains("line 1"));
        assert!(text.contains("line 2"));
    }

    #[test]
    fn tool_call_collapsed_shows_header_only() {
        let input = r#"{"file_path": "/tmp/test.rs", "content": "fn main() {}"}"#;
        let lines = render_tool_call("Write", input, &ToolStatus::Completed, true);
        // Collapsed: one line with summary (file_path)
        assert_eq!(lines.len(), 1);
        let text = lines_to_text(&lines);
        assert!(text.contains("Write"));
        assert!(text.contains("/tmp/test.rs"), "collapsed should show file path summary");
    }

    #[test]
    fn tool_call_expanded_shows_args() {
        let input = r#"{"path": "/tmp/test.rs"}"#;
        let lines = render_tool_call("Write", input, &ToolStatus::Completed, false);
        // Expanded: header + 1 arg = 2
        assert_eq!(lines.len(), 2);
        let text = lines_to_text(&lines);
        assert!(text.contains("path"));
    }

    #[test]
    fn tool_result_collapsed_shows_truncated() {
        let content = (0..10).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, false, true);
        // 10 lines == TOOL_RESULT_MAX_LINES, show all: header + 10 = 11
        assert_eq!(lines.len(), 11);
        let text = lines_to_text(&lines);
        assert!(text.contains("line 0"));
        assert!(text.contains("line 9"));
    }

    // --- ToolResult truncation tests ---

    #[test]
    fn tool_result_short_content_unchanged_collapsed() {
        let content = (0..5).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, false, true);
        let text = lines_to_text(&lines);
        assert!(text.contains("line 0"));
        assert!(text.contains("line 4"));
        assert!(!text.contains("\u{2026}"), "short content should not be truncated");
    }

    #[test]
    fn tool_result_long_content_truncated_collapsed() {
        let content = (0..20).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, false, true);
        let text = lines_to_text(&lines);
        assert!(text.contains("line 0"));
        assert!(text.contains("line 4"));
        assert!(!text.contains("line 5\n") && !text.contains("line 5 ") && !text.contains("    line 5"));
        assert!(text.contains("\u{2026}"));
        assert!(text.contains("line 18"));
        assert!(text.contains("line 19"));
    }

    #[test]
    fn tool_result_long_content_truncated_expanded() {
        let content = (0..100).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, false, false);
        let text = lines_to_text(&lines);
        assert!(text.contains("line 0"));
        assert!(text.contains("line 24"));
        assert!(text.contains("\u{2026}"));
        assert!(text.contains("line 95"));
        assert!(text.contains("line 99"));
    }

    #[test]
    fn tool_result_expanded_short_no_truncation() {
        let content = (0..30).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, false, false);
        let text = lines_to_text(&lines);
        assert!(text.contains("line 0"));
        assert!(text.contains("line 29"));
        assert!(!text.contains("\u{2026}"), "30 lines should not be truncated in expanded mode");
    }

    #[test]
    fn tool_result_error_not_truncated() {
        let content = (0..100).map(|i| format!("error line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, true, false);
        let text = lines_to_text(&lines);
        assert!(text.contains("error line 0"));
        assert!(text.contains("error line 99"));
        assert!(!text.contains("\u{2026}"), "error content should not be truncated");
    }

    #[test]
    fn tool_result_truncation_ellipsis_shows_count() {
        let content = (0..100).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, false, true);
        let text = lines_to_text(&lines);
        // Collapsed: 100 lines, show 5 head + 2 tail = 7 shown, 93 hidden
        assert!(text.contains("+93 lines"), "ellipsis should show hidden line count, got: {}", text);
    }

    #[test]
    fn tool_result_collapsed_short_shows_all() {
        let lines = render_tool_result("ok", false, true);
        // Short content: header + 1 line = 2
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn tool_result_expanded_shows_all() {
        let content = (0..10).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, false, false);
        // Expanded: header + 10 lines = 11
        assert_eq!(lines.len(), 11);
    }

    #[test]
    fn render_block_collapsed_dispatches() {
        let block = ContentBlock::Thinking {
            text: "long thought\nsecond line".into(),
            started_at: Some(Instant::now()),
            finished_at: Some(Instant::now()),
        };
        let lines = render_block_collapsed(&block, 80);
        // Should be collapsed: only header
        assert_eq!(lines.len(), 1);
        let text = lines_to_text(&lines);
        assert!(text.contains("思考中"));
        assert!(!text.contains("long thought"));
    }

    #[test]
    fn render_block_collapsed_text_unchanged() {
        // Text blocks are not affected by collapsed flag
        let block = ContentBlock::Text { text: "hello world".into() };
        let expanded = render_block(&block, 80);
        let collapsed = render_block_collapsed(&block, 80);
        assert_eq!(expanded.len(), collapsed.len());
    }

    // --- Task 4d: summary mode rendering tests ---

    #[test]
    fn summary_mode_hides_thinking_block() {
        let block = ContentBlock::Thinking {
            text: "deep thought about the problem".into(),
            started_at: Some(Instant::now()),
            finished_at: Some(Instant::now()),
        };
        let lines = render_block_summary(&block, 80);
        // In summary mode, thinking blocks should not render (empty)
        assert!(
            lines.is_empty(),
            "summary mode should hide thinking blocks, got {} lines",
            lines.len()
        );
    }

    #[test]
    fn summary_mode_shows_tool_call_block() {
        let block = ContentBlock::ToolCall {
            id: "tc1".into(),
            name: "Read".into(),
            input: r#"{"path": "/tmp/file.rs"}"#.into(),
            status: ToolStatus::Completed,
        };
        let lines = render_block_summary(&block, 80);
        assert_eq!(lines.len(), 1, "summary mode should show tool_call as 1-line summary");
    }

    #[test]
    fn summary_mode_shows_code_fence_in_text() {
        let text_with_code = "Here is some text\n```rust\nfn main() {}\n```\nMore text after";
        let block = ContentBlock::Text { text: text_with_code.into() };
        let lines = render_block_summary(&block, 80);
        let rendered: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        // Code fence content should now be VISIBLE in summary mode
        assert!(rendered.contains("fn main"), "summary mode should now show code fences");
        assert!(rendered.contains("Here is some text"), "summary mode should keep plain text");
        assert!(rendered.contains("More text after"), "summary mode should keep text after code fence");
    }

    #[test]
    fn summary_mode_renders_plain_text() {
        let block = ContentBlock::Text { text: "simple plain text".into() };
        let lines = render_block_summary(&block, 80);
        let rendered: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(rendered.contains("simple plain text"));
    }

    // --- Task 5 & 6: separator removal + warm colors ---

    #[test]
    fn code_block_no_separator_lines() {
        let lines = highlight_code("fn main() {}", "rust");
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(!text.contains("───"), "code block should not have separator lines");
        assert!(text.contains("rust"), "code block should show language name");
    }

    #[test]
    fn thinking_uses_warm_color() {
        let elapsed = Some(Duration::from_secs_f64(1.0));
        let lines = render_thinking("test", elapsed, true);
        let span = &lines[0].spans[0];
        assert_eq!(span.style.fg, Some(theme::BORDER_ACTIVE));
    }

    #[test]
    fn tool_call_completed_uses_success_color() {
        let lines = render_tool_call("Read", "{}", &ToolStatus::Completed, true);
        let icon_span = &lines[0].spans[0];
        assert_eq!(icon_span.style.fg, Some(theme::SUCCESS));
    }

    // --- sanitize_content tests ---

    #[test]
    fn sanitize_strips_system_reminder() {
        let input = "Hello<system-reminder>secret stuff</system-reminder> world";
        let result = sanitize_content(input);
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn sanitize_strips_persisted_output() {
        let input = "Before<persisted-output>hidden</persisted-output>After";
        let result = sanitize_content(input);
        assert_eq!(result, "BeforeAfter");
    }

    #[test]
    fn sanitize_strips_antml_thinking() {
        let input = "Start<thinking>internal thought</thinking>End";
        let result = sanitize_content(input);
        assert_eq!(result, "StartEnd");
    }

    #[test]
    fn sanitize_strips_command_name() {
        let input = "A<command-name>cmd</command-name>B";
        let result = sanitize_content(input);
        assert_eq!(result, "AB");
    }

    #[test]
    fn sanitize_strips_artifact() {
        let input = "X<artifact>content</artifact>Y";
        let result = sanitize_content(input);
        assert_eq!(result, "XY");
    }

    #[test]
    fn sanitize_strips_extremely_important_underscore() {
        let input = "A<EXTREMELY_IMPORTANT>do this</EXTREMELY_IMPORTANT>B";
        let result = sanitize_content(input);
        assert_eq!(result, "AB");
    }

    #[test]
    fn sanitize_strips_subagent_stop() {
        let input = "A<SUBAGENT-STOP>stop</SUBAGENT-STOP>B";
        let result = sanitize_content(input);
        assert_eq!(result, "AB");
    }

    #[test]
    fn sanitize_strips_extremely_important_hyphen() {
        let input = "A<EXTREMELY-IMPORTANT>urgent</EXTREMELY-IMPORTANT>B";
        let result = sanitize_content(input);
        assert_eq!(result, "AB");
    }

    #[test]
    fn sanitize_preserves_normal_html() {
        let input = "Use <b>bold</b> and <i>italic</i>";
        let result = sanitize_content(input);
        assert_eq!(result, "Use <b>bold</b> and <i>italic</i>");
    }

    #[test]
    fn sanitize_handles_nested_tags() {
        let input = "A<system-reminder>outer<persisted-output>inner</persisted-output>still outer</system-reminder>B";
        let result = sanitize_content(input);
        // The outer regex matches system-reminder greedily up to its closing tag.
        // Inner persisted-output is consumed as part of system-reminder content.
        assert_eq!(result, "AB");
    }

    #[test]
    fn sanitize_strips_multiline_tag() {
        let input = "Hello\n<system-reminder>\nline1\nline2\n</system-reminder>\nWorld";
        let result = sanitize_content(input);
        assert_eq!(result, "Hello\n\nWorld");
    }

    #[test]
    fn sanitize_strips_orphan_opening_tag() {
        let input = "Hello <system-reminder> world";
        let result = sanitize_content(input);
        assert_eq!(result, "Hello  world");
    }

    #[test]
    fn sanitize_strips_orphan_closing_tag() {
        let input = "Hello </system-reminder> world";
        let result = sanitize_content(input);
        assert_eq!(result, "Hello  world");
    }

    #[test]
    fn sanitize_empty_result() {
        let input = "<system-reminder>everything is hidden</system-reminder>";
        let result = sanitize_content(input);
        assert_eq!(result, "");
    }

    #[test]
    fn sanitize_content_between_different_tags_preserved() {
        let input = "<system-reminder>hidden</system-reminder>visible<persisted-output>also hidden</persisted-output>";
        let result = sanitize_content(input);
        assert_eq!(result, "visible");
    }

    #[test]
    fn sanitize_no_tags_unchanged() {
        let input = "Just normal text with no XML tags";
        let result = sanitize_content(input);
        assert_eq!(result, "Just normal text with no XML tags");
    }

    // --- extract_tool_summary tests ---

    #[test]
    fn tool_summary_bash_extracts_command() {
        let input = r#"{"command": "ls -la /tmp"}"#;
        let result = extract_tool_summary("Bash", input);
        assert_eq!(result, "ls -la /tmp");
    }

    #[test]
    fn tool_summary_bash_truncates_long_command() {
        let long_cmd = "a".repeat(100);
        let input = format!(r#"{{"command": "{}"}}"#, long_cmd);
        let result = extract_tool_summary("Bash", &input);
        assert_eq!(result.len(), 63); // 60 chars + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn tool_summary_read_extracts_file_path() {
        let input = r#"{"file_path": "/src/main.rs"}"#;
        let result = extract_tool_summary("Read", input);
        assert_eq!(result, "/src/main.rs");
    }

    #[test]
    fn tool_summary_read_with_offset_and_limit() {
        let input = r#"{"file_path": "/src/main.rs", "offset": 10, "limit": 50}"#;
        let result = extract_tool_summary("Read", input);
        assert_eq!(result, "/src/main.rs (10-60)");
    }

    #[test]
    fn tool_summary_read_with_offset_only() {
        let input = r#"{"file_path": "/src/main.rs", "offset": 10}"#;
        let result = extract_tool_summary("Read", input);
        assert_eq!(result, "/src/main.rs (10-)");
    }

    #[test]
    fn tool_summary_edit_extracts_file_path() {
        let input = r#"{"file_path": "/src/theme.rs", "old_string": "foo", "new_string": "bar"}"#;
        let result = extract_tool_summary("Edit", input);
        assert_eq!(result, "/src/theme.rs");
    }

    #[test]
    fn tool_summary_write_extracts_file_path() {
        let input = r#"{"file_path": "/src/main.rs", "content": "fn main() {}"}"#;
        let result = extract_tool_summary("Write", input);
        assert_eq!(result, "/src/main.rs");
    }

    #[test]
    fn tool_summary_websearch_extracts_query() {
        let input = r#"{"query": "ratatui background color"}"#;
        let result = extract_tool_summary("WebSearch", input);
        assert_eq!(result, "ratatui background color");
    }

    #[test]
    fn tool_summary_webfetch_extracts_url() {
        let input = r#"{"url": "https://example.com/page"}"#;
        let result = extract_tool_summary("WebFetch", input);
        assert_eq!(result, "https://example.com/page");
    }

    #[test]
    fn tool_summary_agent_extracts_description() {
        let input = r#"{"description": "Explore code structure"}"#;
        let result = extract_tool_summary("Agent", input);
        assert_eq!(result, "Explore code structure");
    }

    #[test]
    fn tool_summary_unknown_tool_truncates_raw() {
        let input = r#"{"key": "value", "another": "field"}"#;
        let result = extract_tool_summary("UnknownTool", input);
        assert!(result.len() <= 43); // 40 + "..."
    }

    #[test]
    fn tool_summary_missing_field_fallback() {
        let input = r#"{"other_field": "value"}"#;
        let result = extract_tool_summary("Bash", input);
        // Falls back to truncated raw input
        assert!(result.len() <= 43);
    }

    #[test]
    fn tool_summary_non_json_input() {
        let input = "just plain text input";
        let result = extract_tool_summary("Bash", input);
        assert_eq!(result, "just plain text input");
    }

    #[test]
    fn tool_summary_empty_input() {
        let result = extract_tool_summary("Bash", "");
        assert_eq!(result, "");
    }

    #[test]
    fn tool_summary_empty_json() {
        let result = extract_tool_summary("Bash", "{}");
        assert!(result.len() <= 43);
    }

    // --- Code block background tests ---

    #[test]
    fn code_block_spans_have_bg_color() {
        let lines = highlight_code("let x = 1;", "rust");
        // Skip the language label line (index 0), check code lines
        for line in &lines[1..] {
            for span in &line.spans {
                assert_eq!(
                    span.style.bg,
                    Some(theme::BG_L1),
                    "code span '{}' should have BG_L1 background",
                    span.content
                );
            }
        }
    }

    #[test]
    fn code_block_lang_label_has_bg_color() {
        let lines = highlight_code("x = 1", "python");
        // First line is the language label
        assert!(!lines.is_empty());
        let label_line = &lines[0];
        for span in &label_line.spans {
            assert_eq!(
                span.style.bg,
                Some(theme::BG_L1),
                "language label span should have BG_L1 background"
            );
        }
    }

    #[test]
    fn code_block_no_lang_has_bg_color() {
        let lines = highlight_code("plain code", "");
        // No language label, just code
        assert!(!lines.is_empty());
        for line in &lines {
            for span in &line.spans {
                assert_eq!(
                    span.style.bg,
                    Some(theme::BG_L1),
                    "code span '{}' should have BG_L1 background even without lang",
                    span.content
                );
            }
        }
    }
    // --- ToolCall compact rendering tests ---

    #[test]
    fn tool_call_collapsed_shows_summary() {
        let input = r#"{"command": "ls -la"}"#;
        let lines = render_tool_call("Bash", input, &ToolStatus::Running, true);
        assert_eq!(lines.len(), 1);
        let text = lines_to_text(&lines);
        assert!(text.contains("Bash"), "should contain tool name");
        assert!(text.contains("ls -la"), "collapsed should show command summary");
    }

    #[test]
    fn tool_call_expanded_still_shows_full_args() {
        let input = r#"{"command": "ls -la", "description": "list files"}"#;
        let lines = render_tool_call("Bash", input, &ToolStatus::Completed, false);
        // Expanded: header + 2 args
        assert!(lines.len() >= 3);
        let text = lines_to_text(&lines);
        assert!(text.contains("command:"));
        assert!(text.contains("description:"));
    }

    #[test]
    fn tool_call_summary_renders_one_line() {
        let lines = render_tool_call_summary("Edit", r#"{"file_path": "/src/theme.rs"}"#, &ToolStatus::Completed);
        assert_eq!(lines.len(), 1);
        let text = lines_to_text(&lines);
        assert!(text.contains("✓"));
        assert!(text.contains("Edit"));
        assert!(text.contains("/src/theme.rs"));
    }

    #[test]
    fn tool_call_summary_pending_icon() {
        let lines = render_tool_call_summary("Bash", r#"{"command": "cargo test"}"#, &ToolStatus::Pending);
        let text = lines_to_text(&lines);
        assert!(text.contains("⏳"));
        assert!(text.contains("cargo test"));
    }

    #[test]
    fn tool_call_summary_failed_icon() {
        let lines = render_tool_call_summary("Bash", r#"{"command": "false"}"#, &ToolStatus::Failed);
        let text = lines_to_text(&lines);
        assert!(text.contains("✗"));
    }

    // --- Task 5: summary_mode improvement tests ---

    #[test]
    fn summary_mode_shows_tool_call_summary() {
        let block = ContentBlock::ToolCall {
            id: "tc1".into(),
            name: "Read".into(),
            input: r#"{"file_path": "/tmp/file.rs"}"#.into(),
            status: ToolStatus::Completed,
        };
        let lines = render_block_summary(&block, 80);
        assert_eq!(lines.len(), 1, "summary mode should show 1-line tool call summary");
        let text = lines_to_text(&lines);
        assert!(text.contains("Read"));
        assert!(text.contains("/tmp/file.rs"));
        assert!(text.contains("✓"));
    }

    #[test]
    fn summary_mode_still_hides_thinking() {
        let block = ContentBlock::Thinking {
            text: "deep thought".into(),
            started_at: Some(Instant::now()),
            finished_at: Some(Instant::now()),
        };
        let lines = render_block_summary(&block, 80);
        assert!(lines.is_empty(), "summary mode should still hide thinking blocks");
    }

    #[test]
    fn summary_mode_still_hides_tool_result() {
        let block = ContentBlock::ToolResult {
            tool_call_id: "tc1".into(),
            content: "some output".into(),
            is_error: false,
        };
        let lines = render_block_summary(&block, 80);
        assert!(lines.is_empty(), "summary mode should still hide tool result blocks");
    }

    #[test]
    fn summary_mode_shows_error() {
        let block = ContentBlock::Error { message: "something failed".into() };
        let lines = render_block_summary(&block, 80);
        assert!(!lines.is_empty(), "summary mode should show error blocks");
        let text = lines_to_text(&lines);
        assert!(text.contains("something failed"));
    }

}

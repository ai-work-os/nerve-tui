//! Block renderers — each ContentBlock type has its own render function.
//! Input: ContentBlock + terminal width → Output: Vec<Line<'static>>

use crate::theme;
use nerve_tui_protocol::{ContentBlock, ToolStatus};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, CodeBlockKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::LazyLock;
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

/// Options controlling block rendering (expand/collapse state).
#[derive(Debug, Clone, Copy, Default)]
pub struct BlockRenderOpts {
    /// Whether this block is expanded (show full content).
    pub expanded: bool,
}

/// Render any ContentBlock to styled lines (with default collapsed state).
pub fn render_block(block: &ContentBlock, width: u16) -> Vec<Line<'static>> {
    render_block_with_opts(block, width, BlockRenderOpts::default())
}

/// Render a ContentBlock with explicit expand/collapse options.
pub fn render_block_with_opts(block: &ContentBlock, width: u16, opts: BlockRenderOpts) -> Vec<Line<'static>> {
    debug!(block_type = block.kind(), width, expanded = opts.expanded, "rendering content block");
    match block {
        ContentBlock::Text { text } => render_text(text, width),
        ContentBlock::Thinking { text, started_at, finished_at } => {
            let elapsed = match (started_at, finished_at) {
                (Some(s), Some(f)) => Some(f.duration_since(*s)),
                (Some(s), None) => Some(s.elapsed()),
                _ => None,
            };
            render_thinking(text, elapsed, opts.expanded)
        }
        ContentBlock::ToolCall { id: _, name, input, status } => {
            render_tool_call(name, input, status, opts.expanded)
        }
        ContentBlock::ToolResult { tool_call_id: _, content, is_error } => {
            render_tool_result(content, *is_error, opts.expanded)
        }
        ContentBlock::Error { message } => render_error(message),
    }
}

// ---------------------------------------------------------------------------
// Text block: pulldown-cmark + syntect for code highlighting
// ---------------------------------------------------------------------------

fn render_text(content: &str, _width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    render_markdown(content, &mut out);
    out
}

fn render_markdown(content: &str, out: &mut Vec<Line<'static>>) {
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

    for event in parser {
        match event {
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
                        .fg(Color::Cyan)
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
                current_spans.push(Span::styled(
                    text.to_string(),
                    Style::default().fg(Color::Yellow),
                ));
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
                    current_spans.push(Span::styled(text.to_string(), style));
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
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
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

/// Highlight code using syntect. Falls back to plain DarkGray on unknown language.
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

    // Opening border
    let lang_label = if lang.is_empty() {
        "───".to_string()
    } else {
        format!("─── {} ───", lang)
    };
    lines.push(Line::from(Span::styled(
        lang_label,
        Style::default().fg(Color::DarkGray),
    )));

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
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }

    // Closing border
    lines.push(Line::from(Span::styled(
        "───".to_string(),
        Style::default().fg(Color::DarkGray),
    )));

    lines
}

fn syntect_style_to_ratatui(syn: SynStyle) -> Style {
    let fg = Color::Rgb(syn.foreground.r, syn.foreground.g, syn.foreground.b);
    Style::default().fg(fg)
}

// ---------------------------------------------------------------------------
// Thinking block: collapsed by default, shows timer
// ---------------------------------------------------------------------------

fn render_thinking(text: &str, elapsed: Option<std::time::Duration>, expanded: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let timer = elapsed
        .map(|d| format!("{:.1}s", d.as_secs_f64()))
        .unwrap_or_else(|| "…".to_string());

    let expand_hint = if expanded { "▼" } else { "▶" };
    let header = format!("  {} 💭 思考中 ({})", expand_hint, timer);
    lines.push(Line::from(Span::styled(
        header,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));

    if expanded {
        // Show all lines
        for line in text.lines() {
            lines.push(Line::from(Span::styled(
                format!("  │ {}", line),
                Style::default().fg(Color::DarkGray),
            )));
        }
    } else {
        // Show last 3 lines as preview
        let total = text.lines().count();
        let preview_lines: Vec<&str> = text.lines().rev().take(3).collect();
        if total > 3 {
            lines.push(Line::from(Span::styled(
                format!("  │ … 共 {} 行", total),
                Style::default().fg(Color::DarkGray),
            )));
        }
        for line in preview_lines.into_iter().rev() {
            let truncated = if line.len() > 100 {
                format!("{}…", &line[..100])
            } else {
                line.to_string()
            };
            lines.push(Line::from(Span::styled(
                format!("  │ {}", truncated),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    lines
}

// ---------------------------------------------------------------------------
// ToolCall block: tool name + status icon + collapsible args
// ---------------------------------------------------------------------------

fn render_tool_call(name: &str, input: &str, status: &ToolStatus, expanded: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let (icon, icon_color) = match status {
        ToolStatus::Pending => ("⏳", Color::Yellow),
        ToolStatus::Running => ("⏳", Color::Green),
        ToolStatus::Completed => ("✓", Color::Green),
        ToolStatus::Failed => ("✗", Color::Red),
    };

    debug!(tool_name = name, ?status, expanded, "rendering tool_call block");

    let expand_hint = if !input.is_empty() {
        if expanded { " ▼" } else { " ▶" }
    } else {
        ""
    };

    // Header: icon + tool name + expand hint
    lines.push(Line::from(vec![
        Span::styled(format!("  {} ", icon), Style::default().fg(icon_color)),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(theme::TOOL_NAME)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            expand_hint.to_string(),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    // Show input args
    if !input.is_empty() {
        let max_items = if expanded { usize::MAX } else { 3 };
        let max_line_len = if expanded { usize::MAX } else { 100 };

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(input) {
            if let Some(obj) = val.as_object() {
                let mut count = 0;
                for (k, v) in obj {
                    if count >= max_items {
                        lines.push(Line::from(Span::styled(
                            format!("    … {} 个参数", obj.len()),
                            Style::default().fg(theme::TOOL_LABEL),
                        )));
                        break;
                    }
                    let v_str = match v {
                        serde_json::Value::String(s) => truncate(s, max_line_len),
                        _ => truncate(&v.to_string(), max_line_len),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("    {}: ", k),
                            Style::default().fg(theme::TOOL_KEY),
                        ),
                        Span::styled(v_str, Style::default().fg(theme::TOOL_VALUE)),
                    ]));
                    count += 1;
                }
            }
        } else {
            // Plain string input
            let line_limit = if expanded { usize::MAX } else { 3 };
            let total_lines = input.lines().count();
            for (i, line) in input.lines().take(line_limit).enumerate() {
                lines.push(Line::from(Span::styled(
                    format!("    {}", truncate(line, max_line_len)),
                    Style::default().fg(theme::TOOL_VALUE),
                )));
                if !expanded && i == 2 && total_lines > 3 {
                    lines.push(Line::from(Span::styled(
                        format!("    … 共 {} 行", total_lines),
                        Style::default().fg(theme::TOOL_LABEL),
                    )));
                    break;
                }
            }
        }
    }

    lines
}

// ---------------------------------------------------------------------------
// ToolResult block: shows result content with error styling
// ---------------------------------------------------------------------------

fn render_tool_result(content: &str, is_error: bool, expanded: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let expand_hint = if content.lines().count() > 3 {
        if expanded { " ▼" } else { " ▶" }
    } else {
        ""
    };

    let (label, label_color) = if is_error {
        (format!("  ✗ 结果（错误）{}", expand_hint), Color::Red)
    } else {
        (format!("  ✓ 结果{}", expand_hint), theme::TOOL_LABEL)
    };

    lines.push(Line::from(Span::styled(
        label,
        Style::default().fg(label_color),
    )));

    let content_color = if is_error {
        Color::Red
    } else {
        theme::TOOL_VALUE
    };

    let content_lines: Vec<&str> = content.lines().collect();
    let max_line_len = if expanded { usize::MAX } else { 120 };

    if expanded {
        for line in &content_lines {
            lines.push(Line::from(Span::styled(
                format!("    {}", line),
                Style::default().fg(content_color),
            )));
        }
    } else {
        let show_count = content_lines.len().min(3);
        for line in &content_lines[..show_count] {
            lines.push(Line::from(Span::styled(
                format!("    {}", truncate(line, max_line_len)),
                Style::default().fg(content_color),
            )));
        }
        if content_lines.len() > 3 {
            lines.push(Line::from(Span::styled(
                format!("    … 共 {} 行", content_lines.len()),
                Style::default().fg(theme::TOOL_LABEL),
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
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    ))]
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}…", truncated)
    }
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
        assert_eq!(heading_span.unwrap().style.fg, Some(Color::Cyan));
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
        assert_eq!(code_span.unwrap().style.fg, Some(Color::Yellow));
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
    fn thinking_long_text_shows_last_3_lines() {
        let long = (0..10).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_thinking(&long, Some(Duration::from_secs(1)), false);
        // Header + "共 10 行" + 3 preview lines = 5
        assert_eq!(lines.len(), 5);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("共 10 行"));
        assert!(text.contains("line 7"));
        assert!(text.contains("line 8"));
        assert!(text.contains("line 9"));
    }

    #[test]
    fn thinking_expanded_shows_all_lines() {
        let long = (0..10).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_thinking(&long, Some(Duration::from_secs(1)), true);
        // Header + 10 lines = 11
        assert_eq!(lines.len(), 11);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("▼")); // expanded indicator
        assert!(text.contains("line 0"));
        assert!(text.contains("line 9"));
    }

    #[test]
    fn thinking_collapsed_shows_arrow() {
        let lines = render_thinking("short", Some(Duration::from_secs(1)), false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("▶"));
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
    fn tool_call_with_json_args() {
        let input = r#"{"path": "/tmp/test.rs", "content": "fn main() {}"}"#;
        let lines = render_tool_call("Write", input, &ToolStatus::Running, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("path"));
        assert!(text.contains("/tmp/test.rs"));
    }

    #[test]
    fn tool_call_many_args_truncated() {
        let input = r#"{"a":"1","b":"2","c":"3","d":"4","e":"5"}"#;
        let lines = render_tool_call("Foo", input, &ToolStatus::Pending, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        // Should show "… 5 个参数"
        assert!(text.contains("个参数"));
    }

    #[test]
    fn tool_call_expanded_shows_all_args() {
        let input = r#"{"a":"1","b":"2","c":"3","d":"4","e":"5"}"#;
        let lines = render_tool_call("Foo", input, &ToolStatus::Pending, true);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        // Expanded: should show all 5 args, no truncation indicator
        assert!(text.contains("a:"));
        assert!(text.contains("e:"));
        assert!(!text.contains("个参数"));
        assert!(text.contains("▼")); // expanded indicator
    }

    #[test]
    fn tool_call_collapsed_shows_arrow() {
        let input = r#"{"a":"1"}"#;
        let lines = render_tool_call("Foo", input, &ToolStatus::Pending, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("▶"));
    }

    // --- ToolResult block tests ---

    #[test]
    fn tool_result_success() {
        let lines = render_tool_result("file1.txt\nfile2.txt", false, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("✓ 结果"));
        assert!(text.contains("file1.txt"));
    }

    #[test]
    fn tool_result_error() {
        let lines = render_tool_result("command not found", true, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("✗ 结果（错误）"));
        assert!(text.contains("command not found"));
        // Check red color
        let error_span = lines.iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("command not found"));
        assert_eq!(error_span.unwrap().style.fg, Some(Color::Red));
    }

    #[test]
    fn tool_result_long_truncated() {
        let content = (0..10).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, false, false);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("共 10 行"));
    }

    #[test]
    fn tool_result_expanded_shows_all() {
        let content = (0..10).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        let lines = render_tool_result(&content, false, true);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("line 0"));
        assert!(text.contains("line 9"));
        assert!(!text.contains("共 10 行")); // no truncation in expanded
        assert!(text.contains("▼"));
    }

    #[test]
    fn render_block_with_opts_expanded() {
        let block = ContentBlock::Thinking {
            text: "long thought".into(),
            started_at: Some(Instant::now()),
            finished_at: Some(Instant::now()),
        };
        let opts = BlockRenderOpts { expanded: true };
        let lines = render_block_with_opts(&block, 80, opts);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("▼"));
    }

    // --- Error block tests ---

    #[test]
    fn error_block_renders() {
        let lines = render_error("something went wrong");
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("⚠"));
        assert!(text.contains("something went wrong"));
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
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
        // Should have: lang label + 2 code lines + closing border = 4+
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
        assert!(lines.len() >= 3); // border + 1 line + border
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let result = truncate("hello world this is long", 10);
        assert!(result.ends_with('…'));
        assert!(result.chars().count() <= 11); // 10 + …
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
        assert_eq!(span.style.fg, Some(Color::Cyan));
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
}

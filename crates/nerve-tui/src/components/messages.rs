use crate::theme;
use chrono::{Local, TimeZone};
use nerve_tui_protocol::DmMessage;
use nerve_tui_protocol::MessageInfo;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};
use serde_json::Value;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

struct MessageLine {
    from: String,
    content: String,
    timestamp: f64,
}

/// DM mode display state
pub struct DmView {
    pub agent_name: String,
}

pub struct MessagesView {
    lines: Vec<MessageLine>,
    scroll_offset: u16,
    auto_scroll: bool,
    visible_height: u16,
    /// Streaming previews: (agent_name, partial_content)
    pub streaming: Vec<(String, String)>,
    /// Filter: None = all, Some(name) = only from/to this agent
    pub filter: Option<String>,
    /// DM mode: if Some, render DM messages instead of channel messages
    dm_view: Option<DmView>,
    dm_lines: Vec<MessageLine>,
    /// True when new messages arrived while user is scrolled up
    has_new_messages: bool,
}

impl MessagesView {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            visible_height: 0,
            streaming: Vec::new(),
            filter: None,
            dm_view: None,
            dm_lines: Vec::new(),
            has_new_messages: false,
        }
    }

    pub fn push(&mut self, msg: &MessageInfo, _is_agent: bool) {
        self.lines.push(MessageLine {
            from: msg.from.clone(),
            content: msg.content.clone(),
            timestamp: msg.timestamp,
        });
        if self.auto_scroll {
            self.snap_to_bottom();
        } else {
            self.has_new_messages = true;
        }
    }

    pub fn push_system(&mut self, content: &str) {
        self.lines.push(MessageLine {
            from: "系统".to_string(),
            content: content.to_string(),
            timestamp: Local::now().timestamp() as f64,
        });
        if self.auto_scroll {
            self.snap_to_bottom();
        } else {
            self.has_new_messages = true;
        }
    }

    pub fn scroll_down(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
        // Will be clamped in render; if clamped to bottom, auto_scroll re-enabled there
        self.auto_scroll = false;
    }

    pub fn scroll_up(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        self.auto_scroll = false;
    }

    pub fn page_up(&mut self) {
        let page = self.visible_height.max(1);
        self.scroll_up(page);
    }

    pub fn page_down(&mut self) {
        let page = self.visible_height.max(1);
        self.scroll_down(page);
    }

    pub fn snap_to_bottom(&mut self) {
        self.auto_scroll = true;
        self.has_new_messages = false;
        // scroll_offset will be recalculated in render
        self.scroll_offset = u16::MAX;
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.scroll_offset = 0;
        self.has_new_messages = false;
    }

    /// Clear DM-specific state (dm_lines, streaming, scroll).
    pub fn clear_dm(&mut self) {
        self.dm_lines.clear();
        self.streaming.clear();
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.has_new_messages = false;
    }

    // --- DM mode ---

    pub fn is_dm_mode(&self) -> bool {
        self.dm_view.is_some()
    }

    pub fn enter_dm(&mut self, agent_name: &str) {
        self.dm_view = Some(DmView {
            agent_name: agent_name.to_string(),
        });
        self.dm_lines.clear();
        self.streaming.clear();
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.has_new_messages = false;
    }

    pub fn exit_dm(&mut self) {
        self.dm_view = None;
        self.dm_lines.clear();
        self.streaming.clear();
        self.has_new_messages = false;
    }

    pub fn push_dm(&mut self, msg: &DmMessage) {
        self.dm_lines.push(MessageLine {
            from: msg.role.clone(),
            content: msg.content.clone(),
            timestamp: msg.timestamp as f64,
        });
        // Always snap to bottom when user sends a message
        if msg.role == "user" || self.auto_scroll {
            self.snap_to_bottom();
        } else {
            self.has_new_messages = true;
        }
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let title = if let Some(ref dv) = self.dm_view {
            format!(" 与 {} 的对话 ", dv.agent_name)
        } else {
            " Messages ".to_string()
        };

        let block = Block::default()
            .borders(Borders::LEFT)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::BORDER))
            .title(title)
            .title_style(Style::default().fg(theme::BORDER));

        let inner = block.inner(area);
        self.visible_height = inner.height;
        block.render(area, buf);

        let text_lines = self.build_text(inner.width);
        // Estimate visual lines after soft-wrap (use u32 to avoid overflow)
        let width = inner.width.max(1) as usize;
        let total_visual_u32: u32 = text_lines
            .iter()
            .map(|line| {
                let line_width: usize = line.spans.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
                if line_width == 0 {
                    1u32
                } else {
                    ((line_width + width - 1) / width) as u32
                }
            })
            .sum();
        let total_visual = total_visual_u32.min(u16::MAX as u32) as u16;
        let max_offset = total_visual.saturating_sub(self.visible_height);

        if self.auto_scroll {
            self.scroll_offset = max_offset;
        } else {
            self.scroll_offset = self.scroll_offset.min(max_offset);
            // Re-enable auto_scroll if user scrolled back to bottom
            if self.scroll_offset >= max_offset {
                self.auto_scroll = true;
                self.has_new_messages = false;
            }
        }

        let para = Paragraph::new(text_lines)
            .scroll((self.scroll_offset, 0))
            .wrap(Wrap { trim: false });
        para.render(inner, buf);

        // "New messages" indicator when scrolled up
        if self.has_new_messages && !self.auto_scroll && inner.height > 0 {
            let indicator = "↓ 新消息";
            let iw = UnicodeWidthStr::width(indicator) as u16;
            let x = inner.x + inner.width.saturating_sub(iw + 1);
            let y = inner.y + inner.height - 1;
            buf.set_string(
                x,
                y,
                indicator,
                Style::default()
                    .fg(theme::MENTION)
                    .add_modifier(Modifier::BOLD),
            );
        }
    }

    fn build_text(&self, width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();

        let source = if self.dm_view.is_some() {
            &self.dm_lines
        } else {
            &self.lines
        };

        for (i, msg) in source.iter().enumerate() {
            // Filter (only in channel mode)
            if self.dm_view.is_none() {
                if let Some(ref f) = self.filter {
                    if msg.from != *f && !msg.content.contains(&format!("@{}", f)) {
                        continue;
                    }
                }
            }

            // Separator
            if i > 0 {
                out.push(Line::from(""));
            }

            // In DM mode, detect channel-origin prefix "[channel: xxx] from: yyy"
            let (channel_origin, base_content) = if self.dm_view.is_some() {
                extract_channel_origin(&msg.content)
            } else {
                (None, msg.content.clone())
            };

            // Parse routing: extract first @mention as target (channel mode only)
            let (target, display_content) = if self.dm_view.is_some() {
                (None, base_content)
            } else {
                extract_route_target(&base_content)
            };

            // System messages: single line, no timestamp, styled by content
            if msg.from == "系统" {
                let content_lower = display_content.to_lowercase();
                let style = if content_lower.contains("失败")
                    || content_lower.contains("error")
                    || content_lower.contains("错误")
                {
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::ITALIC)
                } else if content_lower.contains("已恢复")
                    || content_lower.contains("成功")
                    || content_lower.contains("已注册")
                {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::ITALIC)
                } else {
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC)
                };
                out.push(Line::from(Span::styled(
                    format!("— {}", display_content),
                    style,
                )));
                continue;
            }

            // Header: from → target  HH:MM:SS  or  from  HH:MM:SS
            let time_str = format_time(msg.timestamp);
            let name_color = theme::agent_color(&msg.from);
            let name_style = Style::default().fg(name_color).add_modifier(Modifier::BOLD);

            let mut header = vec![Span::styled(msg.from.clone(), name_style)];
            if let Some(ref t) = target {
                let target_color = theme::agent_color(t);
                header.push(Span::styled(
                    " → ",
                    Style::default().fg(theme::TIMESTAMP),
                ));
                header.push(Span::styled(
                    t.clone(),
                    Style::default().fg(target_color).add_modifier(Modifier::BOLD),
                ));
            }
            // Show channel origin tag in DM mode
            if let Some(ref origin) = channel_origin {
                header.push(Span::styled(
                    format!("  [来自 #{} @{}]", origin.channel, origin.from),
                    Style::default().fg(theme::SYSTEM_MSG),
                ));
            }
            header.push(Span::raw("  "));
            header.push(Span::styled(time_str, Style::default().fg(theme::TIMESTAMP)));
            out.push(Line::from(header));

            // Content lines: render first (tool_call detection on raw content),
            // then compact blank lines in the rendered output.
            let mut content_lines: Vec<Line<'static>> = Vec::new();
            render_content_lines(&display_content, &mut content_lines);
            compact_rendered_lines(&mut content_lines);
            out.extend(content_lines);
        }

        // Streaming previews
        for (name, content) in &self.streaming {
            out.push(Line::from(""));
            out.push(Line::from(vec![
                Span::styled(
                    name.clone(),
                    Style::default()
                        .fg(theme::AGENT_MSG)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ▌", Style::default().fg(theme::MENTION)),
            ]));
            // Show trailing streaming lines (capped to avoid perf issues with very long output)
            let max_preview = if width > 0 { self.visible_height.max(20) as usize } else { 20 };
            let w = width.max(1) as usize;
            let all_lines: Vec<&str> = content.lines().collect();
            // Count visual lines (accounting for CJK wide chars wrapping)
            let mut visual_count = 0usize;
            let mut start = all_lines.len();
            // Index of a line that needs tail-truncation, and how many visual lines to keep
            let mut truncate_first: Option<(usize, usize)> = None;
            for (i, line) in all_lines.iter().enumerate().rev() {
                let lw = UnicodeWidthStr::width(*line);
                let vl = if lw == 0 { 1 } else { (lw + w - 1) / w };
                visual_count += vl;
                if visual_count > max_preview {
                    // This line pushed us over. Keep the tail portion that fits.
                    let keep_vl = vl.saturating_sub(visual_count - max_preview);
                    if keep_vl > 0 {
                        start = i;
                        truncate_first = Some((i, keep_vl));
                    } else {
                        start = i + 1;
                    }
                    break;
                }
                start = i;
            }
            if start > 0 {
                out.push(Line::from(Span::styled(
                    format!("  … {} 行已省略", start),
                    Style::default().fg(theme::SYSTEM_MSG),
                )));
            }
            // Render the visible streaming lines with markdown
            let mut first_line_truncated = false;
            if let Some((trunc_idx, keep_vl)) = truncate_first {
                if trunc_idx == start {
                    let keep_width = (keep_vl * w).saturating_sub(1);
                    let truncated = tail_by_width(&all_lines[start], keep_width);
                    out.push(Line::from(Span::styled(
                        format!("…{}", truncated),
                        Style::default().fg(theme::AGENT_MSG),
                    )));
                    first_line_truncated = true;
                }
            }
            let md_start = if first_line_truncated { start + 1 } else { start };
            if md_start < all_lines.len() {
                let md_text: String = all_lines[md_start..].join("\n");
                let mut md_lines: Vec<Line<'static>> = Vec::new();
                render_markdown_lines(&md_text, &mut md_lines);
                out.extend(md_lines);
            }
        }

        // Trailing padding so the last message isn't clipped at the panel edge
        if !out.is_empty() {
            out.push(Line::from(""));
        }

        out
    }
}

/// Render the channel messages panel (right side of split view).
/// Uses external scroll state so the main DM scroll is independent.
pub struct ChannelPanelState {
    pub scroll_offset: u16,
    pub auto_scroll: bool,
    pub visible_height: u16,
}

impl ChannelPanelState {
    pub fn new() -> Self {
        Self {
            scroll_offset: u16::MAX,
            auto_scroll: true,
            visible_height: 0,
        }
    }

    pub fn scroll_down(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
        self.auto_scroll = false;
    }

    pub fn scroll_up(&mut self, n: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        self.auto_scroll = false;
    }

    pub fn page_up(&mut self) {
        let page = self.visible_height.max(1);
        self.scroll_up(page);
    }

    pub fn page_down(&mut self) {
        let page = self.visible_height.max(1);
        self.scroll_down(page);
    }

    pub fn snap_to_bottom(&mut self) {
        self.auto_scroll = true;
        self.scroll_offset = u16::MAX;
    }
}

impl MessagesView {
    /// Render channel messages in the split-view right panel.
    pub fn render_channel_panel(
        &self,
        channel_name: &str,
        state: &mut ChannelPanelState,
        focused: bool,
        area: Rect,
        buf: &mut Buffer,
    ) {
        let title = format!(" #{} ", channel_name);
        let border_color = if focused { theme::BORDER } else { theme::TIMESTAMP };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(title)
            .title_style(Style::default().fg(border_color));

        let inner = block.inner(area);
        state.visible_height = inner.height;
        block.render(area, buf);

        // Build text from channel lines (self.lines)
        let text_lines = self.build_channel_text(inner.width);

        let width = inner.width.max(1) as usize;
        let total_visual_u32: u32 = text_lines
            .iter()
            .map(|line| {
                let line_width: usize = line
                    .spans
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                if line_width == 0 {
                    1u32
                } else {
                    ((line_width + width - 1) / width) as u32
                }
            })
            .sum();
        let total_visual = total_visual_u32.min(u16::MAX as u32) as u16;
        let max_offset = total_visual.saturating_sub(state.visible_height);

        if state.auto_scroll {
            state.scroll_offset = max_offset;
        } else {
            state.scroll_offset = state.scroll_offset.min(max_offset);
            if state.scroll_offset >= max_offset {
                state.auto_scroll = true;
            }
        }

        let para = Paragraph::new(text_lines)
            .scroll((state.scroll_offset, 0))
            .wrap(Wrap { trim: false });
        para.render(inner, buf);
    }

    /// Build text lines from channel messages (for split-view right panel).
    fn build_channel_text(&self, _width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();

        for (i, msg) in self.lines.iter().enumerate() {
            // Apply filter if set
            if let Some(ref f) = self.filter {
                if msg.from != *f && !msg.content.contains(&format!("@{}", f)) {
                    continue;
                }
            }

            if i > 0 {
                out.push(Line::from(""));
            }

            let (target, display_content) = extract_route_target(&msg.content);

            // System messages: single line, styled by content (same as main panel)
            if msg.from == "系统" {
                let content_lower = display_content.to_lowercase();
                let style = if content_lower.contains("失败")
                    || content_lower.contains("error")
                    || content_lower.contains("错误")
                {
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::ITALIC)
                } else if content_lower.contains("已恢复")
                    || content_lower.contains("成功")
                    || content_lower.contains("已注册")
                {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::ITALIC)
                } else {
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC)
                };
                out.push(Line::from(Span::styled(
                    format!("— {}", display_content),
                    style,
                )));
                continue;
            }

            let time_str = format_time(msg.timestamp);
            let name_color = theme::agent_color(&msg.from);
            let name_style = Style::default()
                .fg(name_color)
                .add_modifier(Modifier::BOLD);

            let mut header = vec![Span::styled(msg.from.clone(), name_style)];
            if let Some(ref t) = target {
                let target_color = theme::agent_color(t);
                header.push(Span::styled(
                    " → ",
                    Style::default().fg(theme::TIMESTAMP),
                ));
                header.push(Span::styled(
                    t.clone(),
                    Style::default()
                        .fg(target_color)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            header.push(Span::raw("  "));
            header.push(Span::styled(
                time_str,
                Style::default().fg(theme::TIMESTAMP),
            ));
            out.push(Line::from(header));

            let mut content_lines: Vec<Line<'static>> = Vec::new();
            render_content_lines(&display_content, &mut content_lines);
            compact_rendered_lines(&mut content_lines);
            out.extend(content_lines);
        }

        out
    }
}

struct ChannelOrigin {
    channel: String,
    from: String,
}

/// Extract channel origin from DM content prefixed with "[channel: xxx] from: yyy\n\n..."
/// Returns (Some(origin), remaining_content) or (None, original_content)
fn extract_channel_origin(content: &str) -> (Option<ChannelOrigin>, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("[channel:") {
        return (None, content.to_string());
    }
    // Find the end of header block (double newline)
    if let Some(pos) = trimmed.find("\n\n") {
        let header = &trimmed[..pos];
        let rest = trimmed[pos + 2..].to_string();
        // Parse "[channel: xxx] from: yyy"
        let channel = header
            .strip_prefix("[channel:")
            .and_then(|s| s.find(']').map(|i| s[..i].trim().to_string()));
        let from = header
            .find("from:")
            .map(|i| header[i + 5..].trim().to_string());
        if let (Some(ch), Some(f)) = (channel, from) {
            return (Some(ChannelOrigin { channel: ch, from: f }), rest);
        }
    }
    (None, content.to_string())
}

/// Extract the first @mention as route target, return (target, content_without_prefix).
/// "@bob hello world" → (Some("bob"), "hello world")
/// "no mention here"  → (None, "no mention here")
fn extract_route_target(content: &str) -> (Option<String>, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with('@') {
        return (None, content.to_string());
    }
    let after_at = &trimmed[1..];
    let end = after_at
        .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.')
        .unwrap_or(after_at.len());
    if end == 0 {
        return (None, content.to_string());
    }
    let target = after_at[..end].to_string();
    let rest = after_at[end..].trim_start().to_string();
    (Some(target), rest)
}

fn format_time(ts: f64) -> String {
    // Handle both seconds and milliseconds
    let secs = if ts > 1e12 {
        (ts / 1000.0) as i64
    } else {
        ts as i64
    };
    Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

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

/// Compact consecutive blank lines in rendered output (Vec<Line>).
/// Collapses runs of empty lines to at most one, and trims trailing blanks.
fn compact_rendered_lines(lines: &mut Vec<Line<'static>>) {
    let mut i = 0;
    let mut prev_blank = false;
    while i < lines.len() {
        let is_blank = lines[i].spans.is_empty()
            || lines[i]
                .spans
                .iter()
                .all(|s| s.content.trim().is_empty());
        if is_blank && prev_blank {
            lines.remove(i);
        } else {
            prev_blank = is_blank;
            i += 1;
        }
    }
    // Trim trailing blank lines
    while lines
        .last()
        .map_or(false, |l| {
            l.spans.is_empty() || l.spans.iter().all(|s| s.content.trim().is_empty())
        })
    {
        lines.pop();
    }
}

/// Return the tail of `s` whose display width fits within `max_width`.
fn tail_by_width(s: &str, max_width: usize) -> &str {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0usize;
    for (i, c) in s.char_indices().rev() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw > max_width {
            return &s[i + c.len_utf8()..];
        }
        width += cw;
    }
    s
}

/// Detect tool_call JSON and render structured lines.
fn try_render_tool_call(content: &str) -> Option<Vec<Line<'static>>> {
    let trimmed = content.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let val: Value = serde_json::from_str(trimmed).ok()?;
    let obj = val.as_object()?;

    let name = obj.get("name").and_then(|v| v.as_str())?;
    let args = obj.get("arguments").or_else(|| obj.get("input"));

    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("  ⚙ ", Style::default().fg(theme::TOOL_LABEL)),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(theme::TOOL_NAME)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    if let Some(args_val) = args {
        render_args(args_val, &mut lines);
    }

    Some(lines)
}

fn render_args(args_val: &Value, lines: &mut Vec<Line<'static>>) {
    match args_val {
        Value::Object(map) => {
            for (k, v) in map {
                let v_str = match v {
                    Value::String(s) => truncate_str(s, 120),
                    _ => truncate_str(&v.to_string(), 120),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("    {}: ", k), Style::default().fg(theme::TOOL_KEY)),
                    Span::styled(v_str, Style::default().fg(theme::TOOL_VALUE)),
                ]));
            }
        }
        Value::String(s) => {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(s) {
                for (k, v) in &map {
                    let v_str = match v {
                        Value::String(s) => truncate_str(s, 120),
                        _ => truncate_str(&v.to_string(), 120),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("    {}: ", k),
                            Style::default().fg(theme::TOOL_KEY),
                        ),
                        Span::styled(v_str, Style::default().fg(theme::TOOL_VALUE)),
                    ]));
                }
            } else {
                lines.push(Line::from(Span::styled(
                    format!("    {}", truncate_str(s, 120)),
                    Style::default().fg(theme::TOOL_VALUE),
                )));
            }
        }
        _ => {}
    }
}

/// Detect tool_result JSON and render as collapsed summary (first 3 lines).
fn try_render_tool_result(content: &str) -> Option<Vec<Line<'static>>> {
    let trimmed = content.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let val: Value = serde_json::from_str(trimmed).ok()?;
    let obj = val.as_object()?;

    let is_result = obj
        .get("type")
        .and_then(|v| v.as_str())
        .map_or(false, |t| t == "tool_result")
        || obj.contains_key("tool_use_id");

    if !is_result {
        return None;
    }

    let mut lines = Vec::new();
    let result_text = obj
        .get("content")
        .and_then(|v| v.as_str())
        .or_else(|| obj.get("output").and_then(|v| v.as_str()))
        .unwrap_or("");

    let is_error = obj
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let (label, label_color) = if is_error {
        ("  ✗ 结果（错误）", ratatui::style::Color::Red)
    } else {
        ("  ✓ 结果", theme::TOOL_LABEL)
    };
    lines.push(Line::from(Span::styled(
        label.to_string(),
        Style::default().fg(label_color),
    )));

    let result_lines: Vec<&str> = result_text.lines().collect();
    let show_count = result_lines.len().min(3);
    for line in &result_lines[..show_count] {
        lines.push(Line::from(Span::styled(
            format!("    {}", truncate_str(line, 120)),
            Style::default().fg(theme::TOOL_VALUE),
        )));
    }
    if result_lines.len() > 3 {
        lines.push(Line::from(Span::styled(
            format!("    … 共 {} 行", result_lines.len()),
            Style::default().fg(theme::TOOL_LABEL),
        )));
    }

    Some(lines)
}

/// Render content lines with tool_call/tool_result detection and markdown support.
fn render_content_lines(content: &str, out: &mut Vec<Line<'static>>) {
    if let Some(tool_lines) = try_render_tool_call(content) {
        out.extend(tool_lines);
        return;
    }
    if let Some(result_lines) = try_render_tool_result(content) {
        out.extend(result_lines);
        return;
    }
    render_markdown_lines(content, out);
}

/// Parse markdown content and produce styled ratatui Lines.
fn render_markdown_lines(content: &str, out: &mut Vec<Line<'static>>) {
    let opts = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(content, opts);

    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut in_code_block = false;
    let mut is_heading = false;
    let mut bold = false;
    let mut italic = false;
    let mut list_item_pending = false;

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                // Flush current line
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
                out.push(Line::from(Span::styled(
                    "───".to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                // Flush last code line
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
                out.push(Line::from(Span::styled(
                    "───".to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
                in_code_block = false;
            }
            Event::Start(Tag::Heading { .. }) => {
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
                is_heading = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                // Flush heading line with style
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
                // Add blank line between paragraphs (if we already have output)
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
            Event::Start(Tag::Strong) => {
                bold = true;
            }
            Event::End(TagEnd::Strong) => {
                bold = false;
            }
            Event::Start(Tag::Emphasis) => {
                italic = true;
            }
            Event::End(TagEnd::Emphasis) => {
                italic = false;
            }
            Event::Code(text) => {
                current_spans.push(Span::styled(
                    text.to_string(),
                    Style::default().fg(Color::Yellow),
                ));
            }
            Event::Text(text) => {
                if in_code_block {
                    // Code block: split by newlines, each as its own line
                    let lines: Vec<&str> = text.split('\n').collect();
                    for (i, line) in lines.iter().enumerate() {
                        if i > 0 {
                            // Flush previous code line
                            out.push(Line::from(std::mem::take(&mut current_spans)));
                        }
                        current_spans.push(Span::styled(
                            line.to_string(),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                } else if is_heading {
                    current_spans.push(Span::raw(text.to_string()));
                } else {
                    // Handle list item bullet prefix
                    if list_item_pending {
                        current_spans.push(Span::raw("  \u{2022} ".to_string()));
                        list_item_pending = false;
                    }
                    let style = if bold && italic {
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .add_modifier(Modifier::ITALIC)
                    } else if bold {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else if italic {
                        Style::default().add_modifier(Modifier::ITALIC)
                    } else {
                        Style::default()
                    };
                    // Highlight @mentions within the text
                    if text.contains('@') && !bold && !italic {
                        current_spans.extend(highlight_mentions(&text));
                    } else {
                        current_spans.push(Span::styled(text.to_string(), style));
                    }
                }
            }
            Event::SoftBreak => {
                current_spans.push(Span::raw(" ".to_string()));
            }
            Event::HardBreak => {
                out.push(Line::from(std::mem::take(&mut current_spans)));
            }
            _ => {}
        }
    }
    // Flush remaining spans
    if !current_spans.is_empty() {
        out.push(Line::from(current_spans));
    }
}


fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}…", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_tui_protocol::MessageInfo;

    fn make_msg(from: &str, content: &str) -> MessageInfo {
        MessageInfo {
            id: "m1".into(),
            channel_id: "ch1".into(),
            from: from.into(),
            content: content.into(),
            timestamp: 1710000000.0,
            metadata: None,
        }
    }

    #[test]
    fn push_and_count() {
        let mut view = MessagesView::new();
        assert_eq!(view.lines.len(), 0);

        view.push(&make_msg("alice", "hello"), true);
        assert_eq!(view.lines.len(), 1);

        view.push_system("system info");
        assert_eq!(view.lines.len(), 2);
        assert_eq!(view.lines[1].from, "系统");
    }

    #[test]
    fn clear_removes_all() {
        let mut view = MessagesView::new();
        view.push(&make_msg("alice", "hello"), true);
        view.push(&make_msg("bob", "world"), true);
        view.clear();
        assert_eq!(view.lines.len(), 0);
    }

    #[test]
    fn filter_by_agent_name() {
        let mut view = MessagesView::new();
        view.push(&make_msg("alice", "hello"), true);
        view.push(&make_msg("bob", "world"), true);
        view.push(&make_msg("alice", "hi again"), true);

        // No filter - all messages
        view.filter = None;
        let lines = view.build_text(80);
        // Each message = header + content + separator (except first)
        // 3 msgs: msg0(header+content) + sep+msg1(header+content) + sep+msg2(header+content)
        assert!(lines.len() >= 3);

        // Filter for alice - should exclude bob
        view.filter = Some("alice".to_string());
        let filtered = view.build_text(80);
        // Should have alice's 2 messages but not bob's
        for line in &filtered {
            let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            assert!(!text.contains("bob") || text.is_empty());
        }
    }

    #[test]
    fn filter_includes_mentions() {
        let mut view = MessagesView::new();
        view.push(&make_msg("bob", "@alice check this"), true);
        view.filter = Some("alice".to_string());
        let lines = view.build_text(80);
        // bob's message mentioning alice should be included
        assert!(!lines.is_empty());
    }

    #[test]
    fn scroll_up_down() {
        let mut view = MessagesView::new();
        assert!(view.auto_scroll);

        view.scroll_up(5);
        assert!(!view.auto_scroll);
        assert_eq!(view.scroll_offset, 0); // can't go below 0

        view.scroll_down(10);
        assert_eq!(view.scroll_offset, 10);

        view.snap_to_bottom();
        assert!(view.auto_scroll);
    }

    #[test]
    fn highlight_no_mentions() {
        let spans = highlight_mentions("hello world");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "hello world");
    }

    #[test]
    fn highlight_single_mention() {
        let spans = highlight_mentions("hello @alice world");
        // Should be: "hello " + "@alice" + " world"
        assert!(spans.len() >= 3);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "hello @alice world");
    }

    #[test]
    fn highlight_mention_at_start() {
        let spans = highlight_mentions("@bob hi");
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "@bob hi");
        // First span should be the mention (styled)
        assert!(spans[0].style.fg.is_some());
    }

    #[test]
    fn highlight_multiple_mentions() {
        let spans = highlight_mentions("@alice @bob hello");
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "@alice @bob hello");
    }

    #[test]
    fn highlight_mention_with_special_chars() {
        let spans = highlight_mentions("@agent-1 @my_bot done");
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "@agent-1 @my_bot done");
    }

    #[test]
    fn highlight_bare_at_sign() {
        let spans = highlight_mentions("email@ test");
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "email@ test");
    }

    #[test]
    fn format_time_seconds() {
        let s = format_time(1710000000.0);
        assert!(!s.is_empty());
        assert!(s.contains(':'));
    }

    #[test]
    fn format_time_milliseconds() {
        let s = format_time(1710000000000.0);
        assert!(!s.is_empty());
        assert!(s.contains(':'));
    }

    #[test]
    fn streaming_preview() {
        let mut view = MessagesView::new();
        view.streaming
            .push(("alice".to_string(), "partial output...".to_string()));
        let lines = view.build_text(80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("alice"));
        assert!(text.contains("partial output..."));
    }

    // --- DM mode tests ---

    fn make_dm(role: &str, content: &str) -> DmMessage {
        DmMessage {
            role: role.into(),
            content: content.into(),
            timestamp: 1710000000,
        }
    }

    #[test]
    fn dm_enter_exit() {
        let mut view = MessagesView::new();
        assert!(!view.is_dm_mode());

        view.enter_dm("agent-1");
        assert!(view.is_dm_mode());

        view.exit_dm();
        assert!(!view.is_dm_mode());
    }

    #[test]
    fn dm_push_and_render() {
        let mut view = MessagesView::new();
        view.enter_dm("agent-1");

        view.push_dm(&make_dm("user", "hello"));
        view.push_dm(&make_dm("assistant", "hi there"));

        let lines = view.build_text(80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("user"));
        assert!(text.contains("hello"));
        assert!(text.contains("assistant"));
        assert!(text.contains("hi there"));
    }

    #[test]
    fn dm_mode_ignores_channel_filter() {
        let mut view = MessagesView::new();
        // Add channel messages
        view.push(&make_msg("alice", "channel msg"), true);
        // Enter DM mode
        view.enter_dm("bob");
        view.push_dm(&make_dm("user", "dm msg"));

        // Even with a filter set, DM mode shows dm_lines
        view.filter = Some("alice".to_string());
        let lines = view.build_text(80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("dm msg"));
        assert!(!text.contains("channel msg"));
    }

    #[test]
    fn dm_exit_clears_dm_lines() {
        let mut view = MessagesView::new();
        view.enter_dm("agent-1");
        view.push_dm(&make_dm("user", "hello"));
        assert!(!view.dm_lines.is_empty());

        view.exit_dm();
        assert!(view.dm_lines.is_empty());
    }

    #[test]
    fn dm_streaming_then_flush_persists() {
        // Simulates: chunk chunk chunk → idle (no start/end events)
        let mut view = MessagesView::new();
        view.enter_dm("agent-1");

        // User sends message
        view.push_dm(&make_dm("user", "hello"));

        // Streaming chunks accumulate
        view.streaming.push(("agent-1".to_string(), String::new()));
        view.streaming
            .iter_mut()
            .find(|(n, _)| n == "agent-1")
            .unwrap()
            .1
            .push_str("chunk1 ");
        view.streaming
            .iter_mut()
            .find(|(n, _)| n == "agent-1")
            .unwrap()
            .1
            .push_str("chunk2 ");
        view.streaming
            .iter_mut()
            .find(|(n, _)| n == "agent-1")
            .unwrap()
            .1
            .push_str("chunk3");

        // Verify streaming buffer has content
        let buf = &view
            .streaming
            .iter()
            .find(|(n, _)| n == "agent-1")
            .unwrap()
            .1;
        assert_eq!(buf, "chunk1 chunk2 chunk3");

        // Simulate idle-flush: take content, push as DM, clear streaming
        let content = view
            .streaming
            .iter()
            .find(|(n, _)| n == "agent-1")
            .unwrap()
            .1
            .clone();
        let dm_msg = make_dm("assistant", &content);
        view.push_dm(&dm_msg);
        view.streaming.retain(|(n, _)| n != "agent-1");

        // Verify: streaming cleared, DM messages contain both user and assistant
        assert!(view.streaming.is_empty());
        assert_eq!(view.dm_lines.len(), 2);
        let lines = view.build_text(80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("hello"));
        assert!(text.contains("chunk1 chunk2 chunk3"));
    }

    #[test]
    fn extract_route_with_mention() {
        let (target, content) = extract_route_target("@bob hello world");
        assert_eq!(target, Some("bob".to_string()));
        assert_eq!(content, "hello world");
    }

    #[test]
    fn extract_route_no_mention() {
        let (target, content) = extract_route_target("just a message");
        assert_eq!(target, None);
        assert_eq!(content, "just a message");
    }

    #[test]
    fn extract_route_mention_with_dash() {
        let (target, content) = extract_route_target("@agent-1 do this");
        assert_eq!(target, Some("agent-1".to_string()));
        assert_eq!(content, "do this");
    }

    #[test]
    fn extract_route_only_mention() {
        let (target, content) = extract_route_target("@bob");
        assert_eq!(target, Some("bob".to_string()));
        assert_eq!(content, "");
    }

    #[test]
    fn extract_route_mention_mid_text() {
        // @mention not at start — no route extraction
        let (target, content) = extract_route_target("hello @bob world");
        assert_eq!(target, None);
        assert_eq!(content, "hello @bob world");
    }

    #[test]
    fn route_display_in_build_text() {
        let mut view = MessagesView::new();
        view.push(&make_msg("alice", "@bob check this"), true);
        let lines = view.build_text(80);
        // Header should contain "alice" and "bob" (route arrow)
        let header: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(header.contains("alice"), "header has sender");
        assert!(header.contains("→"), "header has arrow");
        assert!(header.contains("bob"), "header has target");
        // Content should NOT start with @bob
        let content: String = lines[1].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(!content.starts_with("@bob"), "content stripped @mention prefix");
        assert!(content.contains("check this"), "content preserved");
    }

    #[test]
    fn dm_replay_flush_on_user_message() {
        // Simulates replay: user_msg → chunks → user_msg → chunks
        // Each user_message should flush preceding agent chunks
        let mut view = MessagesView::new();
        view.enter_dm("agent-1");

        // Turn 1: user message
        view.push_dm(&make_dm("user", "question 1"));

        // Turn 1: agent chunks (no start/end)
        view.streaming
            .push(("agent-1".to_string(), "answer 1".to_string()));

        // Turn 2: new user_message arrives — flush agent streaming first
        let agent_content = view
            .streaming
            .iter()
            .find(|(n, _)| n == "agent-1")
            .map(|(_, c)| c.clone());
        if let Some(content) = agent_content {
            if !content.is_empty() {
                view.push_dm(&make_dm("assistant", &content));
            }
        }
        view.streaming.retain(|(n, _)| n != "agent-1");
        view.push_dm(&make_dm("user", "question 2"));

        // Turn 2: agent chunks
        view.streaming
            .push(("agent-1".to_string(), "answer 2".to_string()));

        // Final flush (idle)
        let agent_content = view
            .streaming
            .iter()
            .find(|(n, _)| n == "agent-1")
            .map(|(_, c)| c.clone());
        if let Some(content) = agent_content {
            if !content.is_empty() {
                view.push_dm(&make_dm("assistant", &content));
            }
        }
        view.streaming.retain(|(n, _)| n != "agent-1");

        // Should have 4 messages in correct order
        assert_eq!(view.dm_lines.len(), 4);
        assert_eq!(view.dm_lines[0].from, "user");
        assert_eq!(view.dm_lines[0].content, "question 1");
        assert_eq!(view.dm_lines[1].from, "assistant");
        assert_eq!(view.dm_lines[1].content, "answer 1");
        assert_eq!(view.dm_lines[2].from, "user");
        assert_eq!(view.dm_lines[2].content, "question 2");
        assert_eq!(view.dm_lines[3].from, "assistant");
        assert_eq!(view.dm_lines[3].content, "answer 2");
    }

    #[test]
    fn extract_channel_origin_with_prefix() {
        let (origin, rest) =
            extract_channel_origin("[channel: ch_abc] from: alice\n\n@agent do something");
        assert!(origin.is_some());
        let o = origin.unwrap();
        assert_eq!(o.channel, "ch_abc");
        assert_eq!(o.from, "alice");
        assert_eq!(rest, "@agent do something");
    }

    #[test]
    fn extract_channel_origin_no_prefix() {
        let (origin, rest) = extract_channel_origin("just a normal message");
        assert!(origin.is_none());
        assert_eq!(rest, "just a normal message");
    }

    #[test]
    fn dm_channel_origin_shows_tag() {
        let mut view = MessagesView::new();
        view.enter_dm("agent-1");
        view.push_dm(&make_dm(
            "user",
            "[channel: ch_123] from: main\n\n@agent-1 fix this bug",
        ));
        let lines = view.build_text(80);
        let header: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(header.contains("来自"), "header shows channel origin tag");
        assert!(header.contains("ch_123"), "header contains channel id");
        assert!(header.contains("main"), "header contains sender name");
        // Content should NOT contain the [channel:...] prefix
        let content: String = lines[1..]
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            !content.contains("[channel:"),
            "content stripped channel prefix"
        );
        assert!(content.contains("fix this bug"), "content preserved");
    }

    #[test]
    fn dm_normal_message_no_tag() {
        let mut view = MessagesView::new();
        view.enter_dm("agent-1");
        view.push_dm(&make_dm("user", "hello there"));
        let lines = view.build_text(80);
        let header: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(!header.contains("来自"), "normal DM has no channel tag");
    }

    #[test]
    fn streaming_preview_shows_more_than_3_lines() {
        let mut view = MessagesView::new();
        view.visible_height = 20;
        let long_content = (0..10).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        view.streaming.push(("agent".to_string(), long_content));
        let lines = view.build_text(80);
        // Should contain all 10 lines (not just last 3)
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("line 0"), "should show first line");
        assert!(text.contains("line 9"), "should show last line");
    }

    #[test]
    fn streaming_preview_caps_at_visible_height() {
        let mut view = MessagesView::new();
        view.visible_height = 5;
        let long_content = (0..50).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        view.streaming.push(("agent".to_string(), long_content));
        let lines = view.build_text(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        // Should NOT show line 0 (capped to last visible_height lines)
        assert!(!text.contains("line 0"), "should cap early lines");
        assert!(text.contains("line 49"), "should show last line");
        // Should show "已省略" indicator
        assert!(text.contains("已省略"), "should show truncation indicator");
    }
}

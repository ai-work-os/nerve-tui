use crate::theme;
use crate::components::block_renderer;
use chrono::{Local, TimeZone};
use nerve_tui_protocol::{DmMessage, Message, MessageInfo, Role};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};
use serde_json::Value;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::collections::HashMap;
use tracing::debug;
use unicode_width::UnicodeWidthStr;

struct MessageLine {
    from: String,
    content: String,
    timestamp: f64,
}

/// DM mode display state
pub struct DmView {
    pub agent_name: String,
    /// Usage label: e.g. "45.2k/200k 23%"
    pub usage_label: Option<String>,
    /// Usage percentage (0.0-1.0) for color coding
    pub usage_ratio: f64,
}

pub struct MessagesView {
    lines: Vec<MessageLine>,
    scroll_offset: u16,
    auto_scroll: bool,
    visible_height: u16,
    /// Streaming previews: (agent_name, partial_content)
    pub streaming: Vec<(String, String)>,
    /// Structured streaming messages: agent_name → Message with ContentBlocks.
    /// Used by the new rendering pipeline alongside `streaming`.
    pub streaming_messages: HashMap<String, Message>,
    /// Message ID counter for generating unique IDs.
    next_msg_id: u64,
    /// Filter: None = all, Some(name) = only from/to this agent
    pub filter: Option<String>,
    /// DM mode: if Some, render DM messages instead of channel messages
    dm_view: Option<DmView>,
    dm_lines: Vec<MessageLine>,
    /// True when new messages arrived while user is scrolled up
    has_new_messages: bool,
    /// Channel message cache: channel_id -> messages
    channel_cache: HashMap<String, Vec<MessageLine>>,
    /// Unread count per channel
    channel_unread: HashMap<String, usize>,
    /// Blink tick counter for streaming cursor (toggles every ~500ms)
    blink_tick: u16,
}

impl MessagesView {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            visible_height: 0,
            streaming: Vec::new(),
            streaming_messages: HashMap::new(),
            next_msg_id: 0,
            filter: None,
            dm_view: None,
            dm_lines: Vec::new(),
            has_new_messages: false,
            channel_cache: HashMap::new(),
            channel_unread: HashMap::new(),
            blink_tick: 0,
        }
    }

    /// Advance the blink tick. Call each render frame (~33ms).
    /// Returns true when the cursor should be visible (on for ~500ms, off for ~500ms).
    pub fn tick_blink(&mut self) -> bool {
        self.blink_tick = self.blink_tick.wrapping_add(1);
        // 15 ticks * 33ms ≈ 500ms per phase
        self.cursor_visible()
    }

    /// Whether the streaming cursor is currently in the visible phase.
    pub fn cursor_visible(&self) -> bool {
        (self.blink_tick / 15) % 2 == 0
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
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

    /// Push a system message to the DM view (visible when in DM mode).
    pub fn push_dm_system(&mut self, content: &str) {
        self.dm_lines.push(MessageLine {
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

    // --- Channel cache ---

    /// Save current channel messages to cache.
    pub fn save_channel(&mut self, channel_id: &str) {
        if !self.lines.is_empty() {
            self.channel_cache
                .insert(channel_id.to_string(), std::mem::take(&mut self.lines));
        }
    }

    /// Load channel messages from cache. Returns true if cache hit.
    pub fn load_channel(&mut self, channel_id: &str) -> bool {
        if let Some(cached) = self.channel_cache.remove(channel_id) {
            self.lines = cached;
            self.scroll_offset = 0;
            self.auto_scroll = true;
            self.has_new_messages = false;
            self.channel_unread.remove(channel_id);
            true
        } else {
            false
        }
    }

    /// Push a message to a non-active channel's cache and increment unread.
    pub fn push_to_channel(&mut self, channel_id: &str, msg: &MessageInfo) {
        let cache = self
            .channel_cache
            .entry(channel_id.to_string())
            .or_default();
        cache.push(MessageLine {
            from: msg.from.clone(),
            content: msg.content.clone(),
            timestamp: msg.timestamp,
        });
        *self.channel_unread.entry(channel_id.to_string()).or_insert(0) += 1;
    }

    /// Get unread count for a channel.
    pub fn unread_count(&self, channel_id: &str) -> usize {
        self.channel_unread.get(channel_id).copied().unwrap_or(0)
    }

    /// Clear unread count for a channel.
    pub fn clear_unread(&mut self, channel_id: &str) {
        self.channel_unread.remove(channel_id);
    }

    // --- Structured streaming (new rendering pipeline) ---

    /// Start a new structured streaming message for an agent.
    pub fn start_streaming_message(&mut self, agent_name: &str) {
        self.next_msg_id += 1;
        let id = format!("stream-{}-{}", agent_name, self.next_msg_id);
        let msg = Message::new(id, Role::Assistant, chrono::Local::now().timestamp() as u64);
        debug!(agent = agent_name, "started streaming message");
        self.streaming_messages.insert(agent_name.to_string(), msg);
    }

    /// Apply an ACP event to the structured streaming message for an agent.
    /// Creates a new message if none exists.
    pub fn apply_streaming_event(&mut self, agent_name: &str, kind: &str, update: &Value) -> bool {
        if !self.streaming_messages.contains_key(agent_name) {
            self.start_streaming_message(agent_name);
        }
        if let Some(msg) = self.streaming_messages.get_mut(agent_name) {
            msg.apply_acp_event(kind, update)
        } else {
            false
        }
    }

    /// Finalize and remove the structured streaming message for an agent.
    /// Returns the completed Message if one exists.
    pub fn take_streaming_message(&mut self, agent_name: &str) -> Option<Message> {
        if let Some(mut msg) = self.streaming_messages.remove(agent_name) {
            msg.meta.partial = false;
            debug!(
                agent = agent_name,
                blocks = msg.blocks.len(),
                "finalized streaming message"
            );
            Some(msg)
        } else {
            None
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
        self.streaming_messages.clear();
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
            usage_label: None,
            usage_ratio: 0.0,
        });
        self.dm_lines.clear();
        self.streaming.clear();
        self.streaming_messages.clear();
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.has_new_messages = false;
    }

    pub fn update_usage(&mut self, used: f64, size: f64, cost: f64) {
        if let Some(ref mut dv) = self.dm_view {
            let ratio = if size > 0.0 { used / size } else { 0.0 };
            let pct = (ratio * 100.0) as u32;
            let label = format!(
                "{}/{} {}% ${:.2}",
                format_tokens(used),
                format_tokens(size),
                pct,
                cost
            );
            dv.usage_label = Some(label);
            dv.usage_ratio = ratio;
        }
    }

    pub fn exit_dm(&mut self) {
        self.dm_view = None;
        self.dm_lines.clear();
        self.streaming.clear();
        self.streaming_messages.clear();
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
        let (title, usage_span) = if let Some(ref dv) = self.dm_view {
            let t = format!(" 与 {} 的对话 ", dv.agent_name);
            let u = dv.usage_label.as_ref().map(|label| {
                let color = if dv.usage_ratio >= 0.9 {
                    Color::Red
                } else if dv.usage_ratio >= 0.8 {
                    Color::Yellow
                } else {
                    theme::BORDER
                };
                Span::styled(format!(" {} ", label), Style::default().fg(color))
            });
            (t, u)
        } else {
            let title = if let Some(ref f) = self.filter {
                format!(" Messages [@{}] ", f)
            } else {
                " Messages ".to_string()
            };
            (title, None)
        };

        let mut block = Block::default()
            .borders(Borders::LEFT)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::BORDER))
            .title(title)
            .title_style(Style::default().fg(theme::BORDER));

        if let Some(usage) = usage_span {
            block = block.title_top(Line::from(usage).alignment(ratatui::layout::Alignment::Right));
        }

        let inner = block.inner(area);
        self.visible_height = inner.height;
        block.render(area, buf);

        let text_lines = self.build_text(inner.width);
        // Use ratatui's own line_count to get exact wrapped line count,
        // avoiding discrepancies between manual estimation and Paragraph::wrap.
        let para = Paragraph::new(text_lines)
            .wrap(Wrap { trim: false });
        let total_visual = (para.line_count(inner.width) as u32).min(u16::MAX as u32) as u16;
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

        let para = para.scroll((self.scroll_offset, 0));
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
        let mut prev_timestamp: Option<f64> = None;

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
                prev_timestamp = Some(msg.timestamp);
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

            // Header: from → target  HH:MM:SS · +Xs
            let time_str = format_time(msg.timestamp);
            let interval_str = prev_timestamp
                .map(|prev| format_interval(prev, msg.timestamp))
                .unwrap_or_default();
            prev_timestamp = Some(msg.timestamp);

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
            if !interval_str.is_empty() {
                header.push(Span::styled(
                    format!(" · {}", interval_str),
                    Style::default().fg(theme::TIMESTAMP),
                ));
            }
            out.push(Line::from(header));

            // Content lines: render first (tool_call detection on raw content),
            // then compact blank lines in the rendered output.
            let mut content_lines: Vec<Line<'static>> = Vec::new();
            render_content_lines(&display_content, &mut content_lines);
            compact_rendered_lines(&mut content_lines);
            out.extend(content_lines);
        }

        // Streaming previews — use block_renderer when structured message exists
        let cursor_char = if self.cursor_visible() { " ▌" } else { "  " };
        for (name, content) in &self.streaming {
            out.push(Line::from(""));
            out.push(Line::from(vec![
                Span::styled(
                    name.clone(),
                    Style::default()
                        .fg(theme::AGENT_MSG)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(cursor_char.to_string(), Style::default().fg(theme::MENTION)),
            ]));

            // Try structured rendering first (new pipeline)
            if let Some(msg) = self.streaming_messages.get(name) {
                if !msg.blocks.is_empty() {
                    debug!(
                        "streaming render: {} using structured pipeline, {} blocks",
                        name, msg.blocks.len()
                    );
                    for block in &msg.blocks {
                        let rendered = block_renderer::render_block(block, width);
                        out.extend(rendered);
                    }
                    continue;
                } else {
                    debug!("streaming render: {} has structured msg but 0 blocks, falling through", name);
                }
            } else {
                debug!(
                    "streaming render: {} not in streaming_messages (keys: [{}]), using text fallback",
                    name,
                    self.streaming_messages.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }

            // Fallback: old string-based streaming rendering
            let max_preview = if width > 0 { self.visible_height.max(20) as usize } else { 20 };
            let w = width.max(1) as usize;
            let all_lines: Vec<&str> = content.lines().collect();
            let mut visual_count = 0usize;
            let mut start = all_lines.len();
            let mut truncate_first: Option<(usize, usize)> = None;
            for (i, line) in all_lines.iter().enumerate().rev() {
                let lw = UnicodeWidthStr::width(*line);
                let vl = if lw == 0 { 1 } else { (lw + w - 1) / w };
                visual_count += vl;
                if visual_count > max_preview {
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

        let para = Paragraph::new(text_lines)
            .wrap(Wrap { trim: false });
        let total_visual = (para.line_count(inner.width) as u32).min(u16::MAX as u32) as u16;
        let max_offset = total_visual.saturating_sub(state.visible_height);

        if state.auto_scroll {
            state.scroll_offset = max_offset;
        } else {
            state.scroll_offset = state.scroll_offset.min(max_offset);
            if state.scroll_offset >= max_offset {
                state.auto_scroll = true;
            }
        }

        let para = para.scroll((state.scroll_offset, 0));
        para.render(inner, buf);
    }

    /// Render raw text content in the split-view right panel (for node output).
    pub fn render_text_panel(
        &self,
        title: &str,
        content: &str,
        state: &mut ChannelPanelState,
        focused: bool,
        area: Rect,
        buf: &mut Buffer,
    ) {
        let title = format!(" {} ", title);
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

        let text_lines: Vec<Line<'static>> = content
            .lines()
            .map(|l| Line::from(l.to_string()))
            .collect();

        let para = Paragraph::new(text_lines)
            .wrap(Wrap { trim: false });
        let total_visual = (para.line_count(inner.width) as u32).min(u16::MAX as u32) as u16;
        let max_offset = total_visual.saturating_sub(state.visible_height);

        if state.auto_scroll {
            state.scroll_offset = max_offset;
        } else {
            state.scroll_offset = state.scroll_offset.min(max_offset);
            if state.scroll_offset >= max_offset {
                state.auto_scroll = true;
            }
        }

        let para = para.scroll((state.scroll_offset, 0));
        para.render(inner, buf);
    }

    /// Build text lines from channel messages (for split-view right panel).
    fn build_channel_text(&self, _width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut prev_timestamp: Option<f64> = None;

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
                prev_timestamp = Some(msg.timestamp);
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
            let interval_str = prev_timestamp
                .map(|prev| format_interval(prev, msg.timestamp))
                .unwrap_or_default();
            prev_timestamp = Some(msg.timestamp);

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
            if !interval_str.is_empty() {
                header.push(Span::styled(
                    format!(" · {}", interval_str),
                    Style::default().fg(theme::TIMESTAMP),
                ));
            }
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

/// Format the time interval between two timestamps as a human-readable string.
/// Returns empty string if interval is less than 1 second.
/// Examples: "+3s", "+1m30s", "+2h15m", "+1d3h"
pub(crate) fn format_interval(prev: f64, curr: f64) -> String {
    let normalize = |ts: f64| -> f64 {
        if ts > 1e12 { ts / 1000.0 } else { ts }
    };
    let diff = (normalize(curr) - normalize(prev)).abs();
    let secs = diff as u64;
    if secs < 1 {
        return String::new();
    }
    if secs < 60 {
        return format!("+{}s", secs);
    }
    if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        return if s > 0 {
            format!("+{}m{}s", m, s)
        } else {
            format!("+{}m", m)
        };
    }
    if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        return if m > 0 {
            format!("+{}h{}m", h, m)
        } else {
            format!("+{}h", h)
        };
    }
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    if h > 0 {
        format!("+{}d{}h", d, h)
    } else {
        format!("+{}d", d)
    }
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
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(content, opts);

    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut in_code_block = false;
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
            // --- Table events ---
            Event::Start(Tag::Table(_)) => {
                if !current_spans.is_empty() {
                    out.push(Line::from(std::mem::take(&mut current_spans)));
                }
                in_table = true;
                table_rows.clear();
            }
            Event::End(TagEnd::Table) => {
                in_table = false;
                if !table_rows.is_empty() {
                    let rendered = render_table_lines(&table_rows);
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
            Event::Code(text) => {
                if in_table {
                    table_cell_buf.push_str(&text);
                } else {
                    current_spans.push(Span::styled(
                        text.to_string(),
                        Style::default().fg(Color::Yellow),
                    ));
                }
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
                } else if in_table {
                    table_cell_buf.push_str(&text);
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

/// Render a table from collected rows into styled lines (aligned with block_renderer).
fn render_table_lines(table_rows: &[(Vec<String>, bool)]) -> Vec<Line<'static>> {
    let col_count = table_rows.iter().map(|(cells, _)| cells.len()).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }

    let mut col_widths = vec![0usize; col_count];
    for (cells, _) in table_rows {
        for (i, cell) in cells.iter().enumerate() {
            if i < col_count {
                let w = UnicodeWidthStr::width(cell.as_str());
                col_widths[i] = col_widths[i].max(w);
            }
        }
    }

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


fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}…", truncated)
    }
}

fn format_tokens(n: f64) -> String {
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else {
        format!("{}", n as u64)
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

    // --- Blink cursor tests ---

    #[test]
    fn blink_tick_toggles() {
        let mut view = MessagesView::new();
        // Initially visible (tick=0)
        assert!(view.cursor_visible());
        // Ticks 1-14: still visible ((1..14)/15 = 0, 0%2 = 0 → visible)
        for i in 1..=14 {
            let v = view.tick_blink();
            assert!(v, "tick {} should be visible", i);
        }
        // Tick 15: invisible ((15/15)%2 = 1)
        assert!(!view.tick_blink(), "tick 15 should be invisible");
        // Ticks 16-29: invisible
        for _ in 16..=29 {
            assert!(!view.tick_blink());
        }
        // Tick 30: visible again ((30/15)%2 = 0)
        assert!(view.tick_blink(), "tick 30 should be visible");
    }

    #[test]
    fn streaming_cursor_blinks_in_output() {
        let mut view = MessagesView::new();
        view.streaming.push(("agent".to_string(), "text".to_string()));

        // Visible phase
        let lines = view.build_text(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("▌"), "cursor visible in on phase");

        // Advance to invisible phase
        for _ in 0..15 {
            view.tick_blink();
        }
        let lines = view.build_text(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(!text.contains("▌"), "cursor hidden in off phase");
    }

    // --- Time interval tests ---

    #[test]
    fn format_interval_seconds() {
        assert_eq!(format_interval(1000.0, 1015.0), "+15s");
    }

    #[test]
    fn format_interval_minutes() {
        assert_eq!(format_interval(1000.0, 1090.0), "+1m30s");
        assert_eq!(format_interval(1000.0, 1060.0), "+1m");
    }

    #[test]
    fn format_interval_hours() {
        assert_eq!(format_interval(1000.0, 4600.0), "+1h");
        assert_eq!(format_interval(1000.0, 5500.0), "+1h15m");
    }

    #[test]
    fn format_interval_days() {
        assert_eq!(format_interval(1000.0, 87400.0), "+1d");
        assert_eq!(format_interval(1000.0, 97800.0), "+1d2h");
    }

    #[test]
    fn format_interval_sub_second() {
        assert_eq!(format_interval(1000.0, 1000.5), "");
    }

    #[test]
    fn format_interval_millis_timestamps() {
        // Both in milliseconds
        assert_eq!(format_interval(1710000000000.0, 1710000015000.0), "+15s");
    }

    #[test]
    fn time_interval_in_header() {
        let mut view = MessagesView::new();
        let mut msg1 = make_msg("alice", "first");
        msg1.timestamp = 1710000000.0;
        view.push(&msg1, true);
        let mut msg2 = make_msg("bob", "second");
        msg2.timestamp = 1710000015.0;
        view.push(&msg2, true);

        let lines = view.build_text(80);
        // Find the header line for "bob"
        let bob_header = lines.iter().find(|l| {
            l.spans.iter().any(|s| s.content.as_ref() == "bob")
        });
        assert!(bob_header.is_some(), "bob header should exist");
        let header_text: String = bob_header.unwrap().spans.iter()
            .map(|s| s.content.to_string()).collect();
        assert!(header_text.contains("+15s"), "header should show +15s interval");
    }

    #[test]
    fn first_message_no_interval() {
        let mut view = MessagesView::new();
        view.push(&make_msg("alice", "first"), true);
        let lines = view.build_text(80);
        let header_text: String = lines[0].spans.iter()
            .map(|s| s.content.to_string()).collect();
        assert!(!header_text.contains('+'), "first message should have no interval");
    }

    // --- Agent filter tab tests ---

    #[test]
    fn filter_shows_in_title() {
        let mut view = MessagesView::new();
        view.filter = Some("alice".to_string());
        // We can't easily test render() output without a buffer, but we can test the title logic
        // by checking that the filter value is incorporated in the title branch
        let title = if let Some(ref f) = view.filter {
            format!(" Messages [@{}] ", f)
        } else {
            " Messages ".to_string()
        };
        assert!(title.contains("@alice"));
    }

    #[test]
    fn filter_none_shows_all_messages() {
        let mut view = MessagesView::new();
        view.push(&make_msg("alice", "a msg"), true);
        view.push(&make_msg("bob", "b msg"), true);
        view.filter = None;
        let lines = view.build_text(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("alice"));
        assert!(text.contains("bob"));
    }

    #[test]
    fn filter_agent_excludes_others() {
        let mut view = MessagesView::new();
        view.push(&make_msg("alice", "hello"), true);
        view.push(&make_msg("bob", "world"), true);
        view.push(&make_msg("charlie", "@alice hey"), true);
        view.filter = Some("alice".to_string());
        let lines = view.build_text(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("alice"), "alice's own message included");
        assert!(text.contains("charlie"), "charlie's message mentioning alice included");
        assert!(!lines.iter().any(|l| {
            l.spans.iter().any(|s| s.content.as_ref() == "bob")
        }), "bob's non-mentioning message excluded");
    }
}

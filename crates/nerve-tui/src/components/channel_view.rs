use crate::theme;
use crate::components::block_renderer;
use chrono::Local;
use nerve_tui_protocol::{ContentBlock, Message, MessageInfo};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};
use std::collections::HashMap;
use tracing::debug;
use unicode_width::UnicodeWidthStr;

use super::messages::{compact_rendered_lines, extract_route_target, format_interval, format_time};

/// Scroll snapshot saved/restored on channel switch (inspired by nvim WinInfo).
struct ViewSnapshot {
    scroll_offset: u16,
    auto_scroll: bool,
    has_new_messages: bool,
}

#[allow(dead_code)]
pub(crate) struct MessageLine {
    pub from: String,
    pub content: String,
    pub timestamp: f64,
    pub blocks: Vec<ContentBlock>,
}

/// Channel messages view — handles channel.message events, caching, filtering, scrolling.
pub struct ChannelView {
    pub(crate) messages: Vec<MessageLine>,
    scroll_offset: u16,
    auto_scroll: bool,
    visible_height: u16,
    has_new_messages: bool,
    pub filter: Option<String>,
    /// channel_id → (messages, scroll snapshot)
    channel_cache: HashMap<String, (Vec<MessageLine>, ViewSnapshot)>,
    channel_unread: HashMap<String, usize>,
}

impl ChannelView {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            visible_height: 0,
            has_new_messages: false,
            filter: None,
            channel_cache: HashMap::new(),
            channel_unread: HashMap::new(),
        }
    }

    pub fn line_count(&self) -> usize {
        self.messages.len()
    }

    pub fn push(&mut self, msg: &MessageInfo, _is_agent: bool) {
        let blocks = Message::content_to_blocks(&msg.content);
        debug!(from = %msg.from, block_count = blocks.len(), "channel push: parsed content to blocks");
        self.messages.push(MessageLine {
            from: msg.from.clone(),
            content: msg.content.clone(),
            timestamp: msg.timestamp,
            blocks,
        });
        if self.auto_scroll {
            self.snap_to_bottom();
        } else {
            self.has_new_messages = true;
        }
    }

    pub fn push_system(&mut self, content: &str) {
        self.messages.push(MessageLine {
            from: "系统".to_string(),
            content: content.to_string(),
            timestamp: Local::now().timestamp() as f64,
            blocks: vec![ContentBlock::Text { text: content.to_string() }],
        });
        if self.auto_scroll {
            self.snap_to_bottom();
        } else {
            self.has_new_messages = true;
        }
    }

    // --- Channel cache with ViewSnapshot ---

    /// Save current channel messages and scroll state to cache.
    pub fn save_channel(&mut self, channel_id: &str) {
        if !self.messages.is_empty() {
            let snapshot = ViewSnapshot {
                scroll_offset: self.scroll_offset,
                auto_scroll: self.auto_scroll,
                has_new_messages: self.has_new_messages,
            };
            self.channel_cache.insert(
                channel_id.to_string(),
                (std::mem::take(&mut self.messages), snapshot),
            );
        }
    }

    /// Load channel messages and scroll state from cache. Returns true if cache hit.
    pub fn load_channel(&mut self, channel_id: &str) -> bool {
        if let Some((cached, snapshot)) = self.channel_cache.remove(channel_id) {
            self.messages = cached;
            self.scroll_offset = snapshot.scroll_offset;
            self.auto_scroll = snapshot.auto_scroll;
            self.has_new_messages = snapshot.has_new_messages;
            self.channel_unread.remove(channel_id);
            true
        } else {
            false
        }
    }

    /// Push a message to a non-active channel's cache and increment unread.
    pub fn push_to_channel(&mut self, channel_id: &str, msg: &MessageInfo) {
        let (cache, _snapshot) = self
            .channel_cache
            .entry(channel_id.to_string())
            .or_insert_with(|| (Vec::new(), ViewSnapshot {
                scroll_offset: 0,
                auto_scroll: true,
                has_new_messages: false,
            }));
        let blocks = Message::content_to_blocks(&msg.content);
        cache.push(MessageLine {
            from: msg.from.clone(),
            content: msg.content.clone(),
            timestamp: msg.timestamp,
            blocks,
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

    // --- Scrolling ---

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
        self.has_new_messages = false;
        self.scroll_offset = u16::MAX;
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.scroll_offset = 0;
        self.has_new_messages = false;
    }

    // --- Rendering ---

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let title = if let Some(ref f) = self.filter {
            format!(" Messages [@{}] ", f)
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
        let para = Paragraph::new(text_lines)
            .wrap(Wrap { trim: false });
        let total_visual = (para.line_count(inner.width) as u32).min(u16::MAX as u32) as u16;
        let max_offset = total_visual.saturating_sub(self.visible_height);

        if self.auto_scroll {
            self.scroll_offset = max_offset;
        } else {
            self.scroll_offset = self.scroll_offset.min(max_offset);
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

    /// Render channel messages in the split-view right panel.
    pub fn render_panel(
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

        let text_lines = self.build_text(inner.width);

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

    /// Public accessor for build_text (used by tests).
    #[allow(dead_code)]
    pub(crate) fn build_text_pub(&self, width: u16) -> Vec<Line<'static>> {
        self.build_text(width)
    }

    fn build_text(&self, width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut prev_timestamp: Option<f64> = None;

        for (i, msg) in self.messages.iter().enumerate() {
            // Filter
            if let Some(ref f) = self.filter {
                if msg.from != *f && !msg.content.contains(&format!("@{}", f)) {
                    continue;
                }
            }

            // Separator
            if i > 0 {
                out.push(Line::from(""));
            }

            // Parse routing: extract first @mention as target
            let (target, display_content) = extract_route_target(&msg.content);

            // System messages
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
            header.push(Span::raw("  "));
            header.push(Span::styled(time_str, Style::default().fg(theme::TIMESTAMP)));
            if !interval_str.is_empty() {
                header.push(Span::styled(
                    format!(" · {}", interval_str),
                    Style::default().fg(theme::TIMESTAMP),
                ));
            }
            out.push(Line::from(header));

            // Unified rendering: parse content to blocks, render collapsed via block_renderer
            let blocks = Message::content_to_blocks(&display_content);
            let mut content_lines: Vec<Line<'static>> = Vec::new();
            for block in &blocks {
                content_lines.extend(block_renderer::render_block_collapsed(block, width));
            }
            compact_rendered_lines(&mut content_lines);
            out.extend(content_lines);
        }

        // Trailing padding
        if !out.is_empty() {
            out.push(Line::from(""));
        }

        out
    }
}

/// Scroll state for the channel panel in split view.
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

/// Render raw text content in the split-view right panel (for node output).
pub fn render_text_panel(
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

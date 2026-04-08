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

/// Cached result of build_text for channel messages.
struct TextCacheEntry {
    lines: Vec<Line<'static>>,
    width: u16,
    msg_count: usize,
    filter: Option<String>,
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
    /// Cached rendered lines for build_text.
    text_cache: Option<TextCacheEntry>,
    /// Number of cache hits (for diagnostics/testing).
    pub cache_hit_count: u64,
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
            text_cache: None,
            cache_hit_count: 0,
        }
    }

    pub fn line_count(&self) -> usize {
        self.messages.len()
    }

    /// Return the content of the last system message, if any.
    pub fn last_system_content(&self) -> Option<&str> {
        self.messages.iter().rev().find(|m| m.from == "系统").map(|m| m.content.as_str())
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
        self.text_cache = None;
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
        self.text_cache = None;
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
            self.text_cache = None;
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
        self.text_cache = None;
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
        &mut self,
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
    pub(crate) fn build_text_pub(&mut self, width: u16) -> Vec<Line<'static>> {
        self.build_text(width)
    }

    fn build_text(&mut self, width: u16) -> Vec<Line<'static>> {
        // Check cache validity
        let cache_valid = self.text_cache.as_ref().map_or(false, |c| {
            c.width == width
                && c.msg_count == self.messages.len()
                && c.filter == self.filter
        });

        if cache_valid {
            self.cache_hit_count += 1;
            return self.text_cache.as_ref().unwrap().lines.clone();
        }

        let lines = self.build_text_inner(width);
        self.text_cache = Some(TextCacheEntry {
            lines: lines.clone(),
            width,
            msg_count: self.messages.len(),
            filter: self.filter.clone(),
        });
        lines
    }

    fn build_text_inner(&self, width: u16) -> Vec<Line<'static>> {
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
#[derive(Debug, Clone, PartialEq)]
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

/// Render a DM split panel with pre-built Lines (from DmView.build_text).
pub fn render_dm_panel(
    title: &str,
    lines: Vec<Line<'static>>,
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

    let para = Paragraph::new(lines)
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

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_tui_protocol::MessageInfo;

    fn make_msg(from: &str, content: &str) -> MessageInfo {
        MessageInfo {
            id: "m1".to_string(),
            channel_id: "ch1".to_string(),
            from: from.to_string(),
            content: content.to_string(),
            timestamp: 1000.0,
            metadata: None,
        }
    }

    // --- Message management ---

    #[test]
    fn new_creates_empty_view() {
        let v = ChannelView::new();
        assert_eq!(v.line_count(), 0);
        assert!(v.auto_scroll);
        assert_eq!(v.scroll_offset, 0);
        assert!(v.filter.is_none());
    }

    #[test]
    fn push_adds_message() {
        let mut v = ChannelView::new();
        v.push(&make_msg("alice", "hello"), false);
        assert_eq!(v.line_count(), 1);
        assert_eq!(v.messages[0].from, "alice");
        assert_eq!(v.messages[0].content, "hello");
    }

    #[test]
    fn push_system_adds_system_message() {
        let mut v = ChannelView::new();
        v.push_system("connected");
        assert_eq!(v.line_count(), 1);
        assert_eq!(v.messages[0].from, "系统");
        assert_eq!(v.messages[0].content, "connected");
    }

    #[test]
    fn clear_removes_all_messages() {
        let mut v = ChannelView::new();
        v.push_system("a");
        v.push_system("b");
        assert_eq!(v.line_count(), 2);
        v.clear();
        assert_eq!(v.line_count(), 0);
    }

    #[test]
    fn last_system_content_returns_last() {
        let mut v = ChannelView::new();
        v.push_system("first");
        v.push(&make_msg("bob", "hi"), false);
        v.push_system("second");
        assert_eq!(v.last_system_content(), Some("second"));
    }

    #[test]
    fn last_system_content_none_when_empty() {
        let v = ChannelView::new();
        assert_eq!(v.last_system_content(), None);
    }

    // --- Scrolling ---

    #[test]
    fn scroll_down_increases_offset() {
        let mut v = ChannelView::new();
        v.scroll_offset = 0;
        v.scroll_down(5);
        assert_eq!(v.scroll_offset, 5);
    }

    #[test]
    fn scroll_up_decreases_offset() {
        let mut v = ChannelView::new();
        v.scroll_offset = 10;
        v.scroll_up(3);
        assert_eq!(v.scroll_offset, 7);
    }

    #[test]
    fn scroll_down_disables_auto_scroll() {
        let mut v = ChannelView::new();
        assert!(v.auto_scroll);
        v.scroll_down(1);
        assert!(!v.auto_scroll);
    }

    #[test]
    fn scroll_up_disables_auto_scroll() {
        let mut v = ChannelView::new();
        assert!(v.auto_scroll);
        v.scroll_up(1);
        assert!(!v.auto_scroll);
    }

    #[test]
    fn snap_to_bottom_enables_auto_scroll() {
        let mut v = ChannelView::new();
        v.scroll_down(5);
        assert!(!v.auto_scroll);
        v.snap_to_bottom();
        assert!(v.auto_scroll);
        assert_eq!(v.scroll_offset, u16::MAX);
        assert!(!v.has_new_messages);
    }

    #[test]
    fn page_up_scrolls_by_visible_height() {
        let mut v = ChannelView::new();
        v.visible_height = 20;
        v.scroll_offset = 50;
        v.page_up();
        assert_eq!(v.scroll_offset, 30);
    }

    // --- Channel cache ---

    #[test]
    fn save_and_load_channel_round_trips() {
        let mut v = ChannelView::new();
        v.push_system("msg1");
        v.push_system("msg2");
        assert_eq!(v.line_count(), 2);

        v.save_channel("ch-a");
        assert_eq!(v.line_count(), 0);

        let loaded = v.load_channel("ch-a");
        assert!(loaded);
        assert_eq!(v.line_count(), 2);
        assert_eq!(v.messages[0].content, "msg1");
    }

    #[test]
    fn load_returns_false_when_no_cache() {
        let mut v = ChannelView::new();
        assert!(!v.load_channel("nonexistent"));
    }

    #[test]
    fn save_preserves_scroll_state() {
        let mut v = ChannelView::new();
        v.auto_scroll = false;
        v.scroll_offset = 0;
        v.push_system("x");
        // push_system does NOT snap because auto_scroll is false
        v.scroll_down(7);
        assert!(!v.auto_scroll);
        assert_eq!(v.scroll_offset, 7);

        v.save_channel("ch-s");
        v.auto_scroll = true;
        v.scroll_offset = 0;

        v.load_channel("ch-s");
        assert_eq!(v.scroll_offset, 7);
        assert!(!v.auto_scroll);
    }

    #[test]
    fn push_to_channel_increments_unread() {
        let mut v = ChannelView::new();
        let msg = make_msg("alice", "hello");
        v.push_to_channel("ch-b", &msg);
        assert_eq!(v.unread_count("ch-b"), 1);
        v.push_to_channel("ch-b", &msg);
        assert_eq!(v.unread_count("ch-b"), 2);
    }

    // --- Unread ---

    #[test]
    fn unread_count_zero_by_default() {
        let v = ChannelView::new();
        assert_eq!(v.unread_count("any"), 0);
    }

    #[test]
    fn clear_unread_resets_count() {
        let mut v = ChannelView::new();
        v.push_to_channel("ch-c", &make_msg("a", "b"));
        v.push_to_channel("ch-c", &make_msg("a", "c"));
        assert_eq!(v.unread_count("ch-c"), 2);
        v.clear_unread("ch-c");
        assert_eq!(v.unread_count("ch-c"), 0);
    }

    #[test]
    fn load_channel_clears_unread() {
        let mut v = ChannelView::new();
        v.push_to_channel("ch-d", &make_msg("a", "x"));
        assert_eq!(v.unread_count("ch-d"), 1);
        v.load_channel("ch-d");
        assert_eq!(v.unread_count("ch-d"), 0);
    }

    // --- Filter ---

    // --- build_text cache ---

    #[test]
    fn build_text_idempotent() {
        let mut v = ChannelView::new();
        v.push(&make_msg("alice", "hello"), false);
        v.push(&make_msg("bob", "world"), false);
        let lines1 = v.build_text_pub(80);
        let lines2 = v.build_text_pub(80);
        assert_eq!(lines1.len(), lines2.len());
        for (a, b) in lines1.iter().zip(lines2.iter()) {
            assert_eq!(format!("{:?}", a), format!("{:?}", b));
        }
    }

    #[test]
    fn build_text_cache_invalidated_on_push() {
        let mut v = ChannelView::new();
        v.push(&make_msg("alice", "hello"), false);
        let lines_before = v.build_text_pub(80);
        v.push(&make_msg("bob", "world"), false);
        let lines_after = v.build_text_pub(80);
        assert!(lines_after.len() > lines_before.len());
    }

    #[test]
    fn build_text_cache_reports_hits() {
        let mut v = ChannelView::new();
        v.push(&make_msg("alice", "hello"), false);
        assert_eq!(v.cache_hit_count, 0);
        v.build_text_pub(80); // miss
        assert_eq!(v.cache_hit_count, 0);
        v.build_text_pub(80); // hit
        assert_eq!(v.cache_hit_count, 1);
        v.push(&make_msg("bob", "world"), false);
        v.build_text_pub(80); // miss
        assert_eq!(v.cache_hit_count, 1);
    }

    // --- Filter ---

    #[test]
    fn filter_affects_build_text_output() {
        let mut v = ChannelView::new();
        v.push(&make_msg("alice", "hello from alice"), false);
        v.push(&make_msg("bob", "hello from bob"), false);
        v.push(&make_msg("alice", "second from alice"), false);

        let lines_all = v.build_text_pub(80);
        let all_text: String = lines_all.iter().map(|l| format!("{:?}", l)).collect();
        assert!(all_text.contains("alice"));
        assert!(all_text.contains("bob"));

        v.filter = Some("alice".to_string());
        let lines_filtered = v.build_text_pub(80);
        let filtered_text: String = lines_filtered.iter().map(|l| format!("{:?}", l)).collect();
        assert!(filtered_text.contains("alice"));
        assert!(!filtered_text.contains("bob"));
    }
}

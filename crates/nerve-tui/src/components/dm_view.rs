use crate::theme;
use crate::components::block_renderer;
use chrono::Local;
use nerve_tui_protocol::{ContentBlock, DmMessage, Message, Role, ToolStatus};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use tracing::debug;
use unicode_width::UnicodeWidthStr;

use super::messages::{compact_rendered_lines, format_interval, format_time};

#[allow(dead_code)]
pub(crate) struct MessageLine {
    pub from: String,
    pub content: String,
    pub timestamp: f64,
    pub blocks: Vec<ContentBlock>,
}

/// DM view — handles node.update events, streaming, DM messages, scrolling.
pub struct DmView {
    agent_name: String,
    pub(crate) messages: Vec<MessageLine>,
    scroll_offset: u16,
    auto_scroll: bool,
    pub(crate) visible_height: u16,
    has_new_messages: bool,
    // Streaming — structured message pipeline
    pub streaming_messages: HashMap<String, Message>,
    next_msg_id: u64,
    // UI
    pub(crate) model_label: Option<String>,
    usage_label: Option<String>,
    usage_ratio: f64,
    blink_tick: u16,
    /// Agents whose streaming buffer was already flushed by idle (to avoid double-persist).
    pub flushed_agents: HashSet<String>,
    /// DM message history for persistence (DmMessage format, separate from render MessageLine).
    pub dm_history: Vec<DmMessage>,
    /// Whether the agent is currently responding (blocks user input).
    pub is_responding: bool,
    /// Summary mode: history messages show only text (no thinking/tool_call/code fences).
    pub summary_mode: bool,
}

impl DmView {
    pub fn new(agent_name: &str) -> Self {
        Self {
            agent_name: agent_name.to_string(),
            messages: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            visible_height: 0,
            has_new_messages: false,
            streaming_messages: HashMap::new(),
            next_msg_id: 0,
            model_label: None,
            usage_label: None,
            usage_ratio: 0.0,
            blink_tick: 0,
            flushed_agents: HashSet::new(),
            dm_history: Vec::new(),
            is_responding: false,
            summary_mode: false,
        }
    }

    /// Create a default (inactive) DmView.
    pub fn inactive() -> Self {
        Self::new("")
    }

    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    pub fn is_active(&self) -> bool {
        !self.agent_name.is_empty()
    }

    pub fn set_responding(&mut self, responding: bool) {
        self.is_responding = responding;
        if !responding {
            self.summary_mode = true;
        }
    }

    pub fn toggle_summary_mode(&mut self) {
        self.summary_mode = !self.summary_mode;
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.streaming_messages.clear();
        self.flushed_agents.clear();
        self.dm_history.clear();
        self.is_responding = false;
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.has_new_messages = false;
    }

    pub fn push(&mut self, msg: &DmMessage) {
        let blocks = Message::content_to_blocks(&msg.content);
        self.push_with_blocks(msg, blocks);
    }

    /// Push a DM message with pre-built blocks (skips content_to_blocks parsing).
    pub fn push_with_blocks(&mut self, msg: &DmMessage, blocks: Vec<ContentBlock>) {
        self.messages.push(MessageLine {
            from: msg.role.clone(),
            content: msg.content.clone(),
            timestamp: msg.timestamp as f64,
            blocks,
        });
        if msg.role == "user" || self.auto_scroll {
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

    pub fn update_usage(&mut self, used: f64, size: f64, cost: f64) {
        let ratio = if size > 0.0 { used / size } else { 0.0 };
        let pct = (ratio * 100.0) as u32;
        let label = format!(
            "{}/{} {}% ${:.2}",
            format_tokens(used),
            format_tokens(size),
            pct,
            cost
        );
        self.usage_label = Some(label);
        self.usage_ratio = ratio;
    }

    pub fn set_model_label(&mut self, model: Option<&str>, token_size: Option<f64>) {
        self.model_label = match model {
            Some(m) => {
                if let Some(size) = token_size {
                    Some(format!("{} / {}", m, format_tokens(size)))
                } else {
                    Some(m.to_string())
                }
            }
            None => None,
        };
    }

    // --- Node log entries (program node observability) ---

    pub fn push_log_entries(&mut self, update: &Value) {
        if let Some(entries) = update.get("entries").and_then(|v| v.as_array()) {
            for entry in entries {
                let level = entry.get("level").and_then(|v| v.as_str()).unwrap_or("info");
                let message = entry.get("message").and_then(|v| v.as_str()).unwrap_or("");
                let ts_str = entry.get("ts").and_then(|v| v.as_str()).unwrap_or("");
                // Parse timestamp for display: extract HH:MM:SS from ISO string
                let time_display = ts_str.get(11..19).unwrap_or("??:??:??");
                let formatted = format!("[{}] [{}] {}", time_display, level.to_uppercase(), message);
                self.messages.push(MessageLine {
                    from: "log".to_string(),
                    content: formatted.clone(),
                    timestamp: Local::now().timestamp() as f64,
                    blocks: vec![ContentBlock::Text { text: formatted }],
                });
            }
            if self.auto_scroll {
                self.snap_to_bottom();
            } else {
                self.has_new_messages = true;
            }
        }
    }

    // --- Structured streaming ---

    pub fn start_streaming_message(&mut self, agent_name: &str) {
        self.next_msg_id += 1;
        let id = format!("stream-{}-{}", agent_name, self.next_msg_id);
        let msg = Message::new(id, Role::Assistant, chrono::Local::now().timestamp() as u64);
        debug!(agent = agent_name, "started streaming message");
        self.streaming_messages.insert(agent_name.to_string(), msg);
    }

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

    // --- Blink ---

    pub fn tick_blink(&mut self) -> bool {
        self.blink_tick = self.blink_tick.wrapping_add(1);
        self.cursor_visible()
    }

    pub fn cursor_visible(&self) -> bool {
        (self.blink_tick / 15) % 2 == 0
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

    // --- Rendering ---

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let title = format!(" 与 {} 的对话 ", self.agent_name);

        let mut block = Block::default()
            .borders(Borders::LEFT)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::BORDER))
            .title(title)
            .title_style(Style::default().fg(theme::BORDER));

        // Right-aligned top: model label + usage (e.g. "opus[1m] / 200k  50K/100K 50% $1.23")
        let mut right_spans: Vec<Span> = Vec::new();
        if let Some(ref model) = self.model_label {
            right_spans.push(Span::styled(
                format!(" {} ", model),
                Style::default().fg(theme::BORDER),
            ));
        }
        if let Some(ref label) = self.usage_label {
            let color = if self.usage_ratio >= 0.9 {
                Color::Red
            } else if self.usage_ratio >= 0.8 {
                Color::Yellow
            } else {
                theme::BORDER
            };
            right_spans.push(Span::styled(
                format!(" {} ", label),
                Style::default().fg(color),
            ));
        }
        if !right_spans.is_empty() {
            block = block.title_top(Line::from(right_spans).alignment(ratatui::layout::Alignment::Right));
        }

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

    pub(crate) fn build_text(&self, width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut prev_timestamp: Option<f64> = None;

        for (i, msg) in self.messages.iter().enumerate() {
            if i > 0 {
                out.push(Line::from(""));
            }

            // DM mode: detect channel-origin prefix
            let (channel_origin, base_content) = extract_channel_origin(&msg.content);
            let display_content = base_content;

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

            // Log entries from program nodes
            if msg.from == "log" {
                prev_timestamp = Some(msg.timestamp);
                let style = if display_content.contains("[ERROR]") {
                    Style::default().fg(Color::Red)
                } else if display_content.contains("[WARN]") {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                out.push(Line::from(Span::styled(display_content.clone(), style)));
                continue;
            }

            // Header
            let time_str = format_time(msg.timestamp);
            let interval_str = prev_timestamp
                .map(|prev| format_interval(prev, msg.timestamp))
                .unwrap_or_default();
            prev_timestamp = Some(msg.timestamp);

            let name_color = theme::agent_color(&msg.from);
            let name_style = Style::default().fg(name_color).add_modifier(Modifier::BOLD);

            let mut header = vec![Span::styled(msg.from.clone(), name_style)];
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

            // Content via block_renderer — use pre-built blocks if they contain
            // structured content (tool calls, thinking, etc). Plain text-only blocks
            // go through content_to_blocks for channel-origin stripping etc.
            let fallback_blocks;
            let has_structured = msg.blocks.iter().any(|b| !matches!(b, ContentBlock::Text { .. }));
            let blocks = if has_structured {
                &msg.blocks
            } else {
                fallback_blocks = Message::content_to_blocks(&display_content);
                &fallback_blocks
            };
            let mut content_lines: Vec<Line<'static>> = Vec::new();
            for block in blocks {
                if self.summary_mode {
                    content_lines.extend(block_renderer::render_block_summary(block, width));
                } else {
                    content_lines.extend(block_renderer::render_block_collapsed(block, width));
                }
            }
            compact_rendered_lines(&mut content_lines);
            out.extend(content_lines);
        }

        // Streaming previews — sorted keys for stable render order
        let cursor_char = if self.cursor_visible() { " ▌" } else { "  " };
        let mut streaming_names: Vec<&String> = self.streaming_messages.keys().collect();
        streaming_names.sort();
        for name in streaming_names {
            let msg = &self.streaming_messages[name];
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

            if !msg.blocks.is_empty() {
                debug!(
                    "streaming render: {} blocks={}",
                    name, msg.blocks.len()
                );
                for block in &msg.blocks {
                    let rendered = block_renderer::render_block(block, width);
                    out.extend(rendered);
                }
            } else {
                debug!("streaming render: {} has 0 blocks", name);
            }
        }

        // Trailing padding
        if !out.is_empty() {
            out.push(Line::from(""));
        }

        out
    }
}

/// Convert structured content blocks to a plain text string for DmMessage persistence.
/// Thinking blocks are excluded (not persisted to DM history).
pub fn blocks_to_text(blocks: &[ContentBlock]) -> String {
    let parts: Vec<String> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => {
                if text.is_empty() { None } else { Some(text.clone()) }
            }
            ContentBlock::Thinking { .. } => None,
            ContentBlock::ToolCall { name, status, .. } => {
                let marker = match status {
                    ToolStatus::Pending => "…",
                    ToolStatus::Running => "⟳",
                    ToolStatus::Completed => "✓",
                    ToolStatus::Failed => "✗",
                };
                Some(format!("[tool:{} {}]", name, marker))
            }
            ContentBlock::ToolResult { content, is_error, .. } => {
                if content.is_empty() {
                    None
                } else if *is_error {
                    Some(format!("[error] {}", content))
                } else {
                    Some(content.clone())
                }
            }
            ContentBlock::Error { message } => Some(format!("[error] {}", message)),
        })
        .collect();
    parts.join("\n")
}

// --- Helper functions (DM-specific) ---

struct ChannelOrigin {
    channel: String,
    from: String,
}

fn extract_channel_origin(content: &str) -> (Option<ChannelOrigin>, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("[channel:") {
        return (None, content.to_string());
    }
    if let Some(pos) = trimmed.find("\n\n") {
        let header = &trimmed[..pos];
        let rest = trimmed[pos + 2..].to_string();
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
    use nerve_tui_protocol::{ContentBlock, DmMessage};
    use serde_json::json;

    fn make_dm(role: &str, content: &str) -> DmMessage {
        DmMessage {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: 1000,
        }
    }

    // --- Basics ---

    #[test]
    fn new_sets_agent_name() {
        let dm = DmView::new("alice");
        assert_eq!(dm.agent_name(), "alice");
    }

    #[test]
    fn inactive_is_not_active() {
        let dm = DmView::inactive();
        assert!(!dm.is_active());
    }

    #[test]
    fn new_is_active() {
        let dm = DmView::new("alice");
        assert!(dm.is_active());
    }

    // --- Messages ---

    #[test]
    fn push_adds_message() {
        let mut dm = DmView::new("alice");
        assert_eq!(dm.messages.len(), 0);
        dm.push(&make_dm("user", "hello"));
        assert_eq!(dm.messages.len(), 1);
        assert_eq!(dm.messages[0].from, "user");
    }

    #[test]
    fn push_system_adds_system_message() {
        let mut dm = DmView::new("alice");
        dm.push_system("connected");
        assert_eq!(dm.messages.len(), 1);
        assert_eq!(dm.messages[0].from, "系统");
        assert_eq!(dm.messages[0].content, "connected");
    }

    #[test]
    fn clear_resets_all_state() {
        let mut dm = DmView::new("alice");
        dm.push(&make_dm("user", "hi"));
        dm.start_streaming_message("alice");
        dm.dm_history.push(make_dm("user", "hi"));
        dm.is_responding = true;

        dm.clear();

        assert!(dm.messages.is_empty());
        assert!(dm.streaming_messages.is_empty());
        assert!(dm.dm_history.is_empty());
        assert!(!dm.is_responding);
        assert!(dm.auto_scroll);
    }

    // --- Streaming ---

    #[test]
    fn start_streaming_creates_message() {
        let mut dm = DmView::new("alice");
        dm.start_streaming_message("alice");
        assert!(dm.streaming_messages.contains_key("alice"));
    }

    #[test]
    fn apply_streaming_accumulates_text() {
        let mut dm = DmView::new("alice");
        dm.start_streaming_message("alice");
        let update = json!({ "content": { "text": "hello" } });
        dm.apply_streaming_event("alice", "agent_message_chunk", &update);
        let update2 = json!({ "content": { "text": " world" } });
        dm.apply_streaming_event("alice", "agent_message_chunk", &update2);

        let msg = dm.streaming_messages.get("alice").unwrap();
        let text: String = msg.blocks.iter().filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        }).collect();
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
    }

    #[test]
    fn apply_streaming_accumulates_thinking() {
        let mut dm = DmView::new("alice");
        dm.start_streaming_message("alice");
        let update = json!({ "content": { "text": "hmm" } });
        dm.apply_streaming_event("alice", "agent_thought_chunk", &update);

        let msg = dm.streaming_messages.get("alice").unwrap();
        let has_thinking = msg.blocks.iter().any(|b| matches!(b, ContentBlock::Thinking { .. }));
        assert!(has_thinking);
    }

    #[test]
    fn apply_auto_starts_streaming() {
        let mut dm = DmView::new("alice");
        // No start_streaming_message call
        let update = json!({ "content": { "text": "auto" } });
        dm.apply_streaming_event("bob", "agent_message_chunk", &update);
        assert!(dm.streaming_messages.contains_key("bob"));
    }

    #[test]
    fn take_streaming_returns_message() {
        let mut dm = DmView::new("alice");
        dm.start_streaming_message("alice");
        let result = dm.take_streaming_message("alice");
        assert!(result.is_some());
    }

    #[test]
    fn take_streaming_removes_from_map() {
        let mut dm = DmView::new("alice");
        dm.start_streaming_message("alice");
        dm.take_streaming_message("alice");
        assert!(dm.streaming_messages.is_empty());
    }

    #[test]
    fn take_streaming_sets_partial_false() {
        let mut dm = DmView::new("alice");
        dm.start_streaming_message("alice");
        let msg = dm.take_streaming_message("alice").unwrap();
        assert!(!msg.meta.partial);
    }

    // --- Blink ---

    #[test]
    fn tick_blink_increments() {
        let mut dm = DmView::new("alice");
        assert_eq!(dm.blink_tick, 0);
        dm.tick_blink();
        assert_eq!(dm.blink_tick, 1);
        dm.tick_blink();
        assert_eq!(dm.blink_tick, 2);
    }

    #[test]
    fn cursor_visible_alternates() {
        let mut dm = DmView::new("alice");
        // blink_tick=0 -> (0/15)%2==0 -> visible
        assert!(dm.cursor_visible());
        // Advance to tick 15 -> (15/15)%2==1 -> not visible
        for _ in 0..15 {
            dm.tick_blink();
        }
        assert!(!dm.cursor_visible());
        // Advance to tick 30 -> (30/15)%2==0 -> visible again
        for _ in 0..15 {
            dm.tick_blink();
        }
        assert!(dm.cursor_visible());
    }

    // --- Scrolling ---

    #[test]
    fn scroll_down_disables_auto_scroll() {
        let mut dm = DmView::new("alice");
        assert!(dm.auto_scroll);
        dm.scroll_down(1);
        assert!(!dm.auto_scroll);
    }

    #[test]
    fn scroll_up_disables_auto_scroll() {
        let mut dm = DmView::new("alice");
        assert!(dm.auto_scroll);
        dm.scroll_up(1);
        assert!(!dm.auto_scroll);
    }

    #[test]
    fn snap_to_bottom_enables_auto_scroll() {
        let mut dm = DmView::new("alice");
        dm.scroll_down(5);
        assert!(!dm.auto_scroll);
        dm.snap_to_bottom();
        assert!(dm.auto_scroll);
        assert_eq!(dm.scroll_offset, u16::MAX);
    }

    // --- Summary mode ---

    #[test]
    fn toggle_summary_mode_flips() {
        let mut dm = DmView::new("alice");
        assert!(!dm.summary_mode);
        dm.toggle_summary_mode();
        assert!(dm.summary_mode);
        dm.toggle_summary_mode();
        assert!(!dm.summary_mode);
    }

    // --- set_responding ---

    #[test]
    fn set_responding_false_enables_summary_mode() {
        let mut dm = DmView::new("alice");
        dm.is_responding = true;
        dm.summary_mode = false;
        dm.set_responding(false);
        assert!(!dm.is_responding);
        assert!(dm.summary_mode);
    }

    #[test]
    fn set_responding_true_keeps_summary_mode() {
        let mut dm = DmView::new("alice");
        dm.summary_mode = false;
        dm.set_responding(true);
        assert!(dm.is_responding);
        assert!(!dm.summary_mode);
    }

    // --- Model label ---

    #[test]
    fn set_model_label_stores_value() {
        let mut dm = DmView::new("alice");
        assert!(dm.model_label.is_none());
        dm.set_model_label(Some("opus[1m]"), Some(200_000.0));
        assert_eq!(dm.model_label.as_deref(), Some("opus[1m] / 200.0k"));
    }

    #[test]
    fn set_model_label_without_context_window() {
        let mut dm = DmView::new("alice");
        dm.set_model_label(Some("sonnet"), None);
        assert_eq!(dm.model_label.as_deref(), Some("sonnet"));
    }

    #[test]
    fn set_model_label_none_clears() {
        let mut dm = DmView::new("alice");
        dm.set_model_label(Some("opus"), None);
        assert!(dm.model_label.is_some());
        dm.set_model_label(None, None);
        assert!(dm.model_label.is_none());
    }

    // --- Usage ---

    #[test]
    fn update_usage_sets_label() {
        let mut dm = DmView::new("alice");
        assert!(dm.usage_label.is_none());
        dm.update_usage(50000.0, 100000.0, 1.23);
        assert!(dm.usage_label.is_some());
        let label = dm.usage_label.as_ref().unwrap();
        assert!(label.contains("50%"));
        assert!(label.contains("$1.23"));
    }

    // --- blocks_to_text ---

    #[test]
    fn blocks_to_text_filters_thinking() {
        let blocks = vec![
            ContentBlock::Thinking {
                text: "internal thought".to_string(),
                started_at: None,
                finished_at: None,
            },
            ContentBlock::Text { text: "visible".to_string() },
        ];
        let result = blocks_to_text(&blocks);
        assert!(!result.contains("internal thought"));
        assert!(result.contains("visible"));
    }

    #[test]
    fn blocks_to_text_includes_text() {
        let blocks = vec![
            ContentBlock::Text { text: "hello".to_string() },
            ContentBlock::Text { text: "world".to_string() },
        ];
        let result = blocks_to_text(&blocks);
        assert_eq!(result, "hello\nworld");
    }

    // --- format_tokens ---

    #[test]
    fn format_tokens_small() {
        assert_eq!(format_tokens(500.0), "500");
    }

    #[test]
    fn format_tokens_thousands() {
        assert_eq!(format_tokens(1500.0), "1.5k");
    }

    #[test]
    fn format_tokens_millions() {
        assert_eq!(format_tokens(2_500_000.0), "2.5M");
    }

    // --- extract_channel_origin ---

    #[test]
    fn extract_channel_origin_no_prefix() {
        let (origin, text) = extract_channel_origin("plain message");
        assert!(origin.is_none());
        assert_eq!(text, "plain message");
    }

    #[test]
    fn extract_channel_origin_with_prefix() {
        let input = "[channel:general] from:bob\n\nactual content";
        let (origin, text) = extract_channel_origin(input);
        assert!(origin.is_some());
        let o = origin.unwrap();
        assert_eq!(o.channel, "general");
        assert_eq!(o.from, "bob");
        assert_eq!(text, "actual content");
    }
}

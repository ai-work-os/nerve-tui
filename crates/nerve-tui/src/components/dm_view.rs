use crate::theme;
use crate::components::block_renderer;
use chrono::Local;
use nerve_tui_protocol::{ContentBlock, DmMessage, Message, Role};
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
    // Streaming (old pipeline)
    pub streaming: Vec<(String, String)>,
    // Streaming (new pipeline)
    pub streaming_messages: HashMap<String, Message>,
    next_msg_id: u64,
    // UI
    usage_label: Option<String>,
    usage_ratio: f64,
    blink_tick: u16,
    /// Agents whose streaming buffer was already flushed by idle (to avoid double-persist).
    pub flushed_agents: HashSet<String>,
    /// DM message history for persistence (DmMessage format, separate from render MessageLine).
    pub dm_history: Vec<DmMessage>,
    /// Whether the agent is currently responding (blocks user input).
    pub is_responding: bool,
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
            streaming: Vec::new(),
            streaming_messages: HashMap::new(),
            next_msg_id: 0,
            usage_label: None,
            usage_ratio: 0.0,
            blink_tick: 0,
            flushed_agents: HashSet::new(),
            dm_history: Vec::new(),
            is_responding: false,
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

    pub fn clear(&mut self) {
        self.messages.clear();
        self.streaming.clear();
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
        let usage_span = self.usage_label.as_ref().map(|label| {
            let color = if self.usage_ratio >= 0.9 {
                Color::Red
            } else if self.usage_ratio >= 0.8 {
                Color::Yellow
            } else {
                theme::BORDER
            };
            Span::styled(format!(" {} ", label), Style::default().fg(color))
        });

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

            // Content via block_renderer
            let blocks = Message::content_to_blocks(&display_content);
            let mut content_lines: Vec<Line<'static>> = Vec::new();
            for block in &blocks {
                content_lines.extend(block_renderer::render_block_collapsed(block, width));
            }
            compact_rendered_lines(&mut content_lines);
            out.extend(content_lines);
        }

        // Streaming previews
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
            for (idx, line) in all_lines.iter().enumerate().rev() {
                let lw = UnicodeWidthStr::width(*line);
                let vl = if lw == 0 { 1 } else { (lw + w - 1) / w };
                visual_count += vl;
                if visual_count > max_preview {
                    let keep_vl = vl.saturating_sub(visual_count - max_preview);
                    if keep_vl > 0 {
                        start = idx;
                        truncate_first = Some((idx, keep_vl));
                    } else {
                        start = idx + 1;
                    }
                    break;
                }
                start = idx;
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
                let blocks = Message::content_to_blocks(&md_text);
                for block in &blocks {
                    out.extend(block_renderer::render_block(block, width));
                }
            }
        }

        // Trailing padding
        if !out.is_empty() {
            out.push(Line::from(""));
        }

        out
    }
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

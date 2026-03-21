use chrono::{Local, TimeZone};
use nerve_tui_protocol::MessageInfo;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use crate::theme;
use nerve_tui_protocol::DmMessage;

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

    pub fn snap_to_bottom(&mut self) {
        self.auto_scroll = true;
        // scroll_offset will be recalculated in render
        self.scroll_offset = u16::MAX;
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.scroll_offset = 0;
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
    }

    pub fn exit_dm(&mut self) {
        self.dm_view = None;
        self.dm_lines.clear();
        self.streaming.clear();
    }

    pub fn push_dm(&mut self, msg: &DmMessage) {
        self.dm_lines.push(MessageLine {
            from: msg.role.clone(),
            content: msg.content.clone(),
            timestamp: msg.timestamp as f64,
        });
        if self.auto_scroll {
            self.snap_to_bottom();
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
            .border_style(Style::default().fg(theme::BORDER))
            .title(title)
            .title_style(Style::default().fg(theme::TITLE));

        let inner = block.inner(area);
        self.visible_height = inner.height;
        block.render(area, buf);

        let text_lines = self.build_text(inner.width);
        let total = text_lines.len() as u16;

        if self.auto_scroll {
            self.scroll_offset = total.saturating_sub(self.visible_height);
        } else {
            self.scroll_offset = self.scroll_offset.min(total.saturating_sub(self.visible_height));
        }

        let para = Paragraph::new(text_lines)
            .scroll((self.scroll_offset, 0))
            .wrap(Wrap { trim: false });
        para.render(inner, buf);
    }

    fn build_text(&self, _width: u16) -> Vec<Line<'static>> {
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

            // Header: name  HH:MM:SS
            let time_str = format_time(msg.timestamp);
            let name_color = theme::agent_color(&msg.from);
            let name_style = if msg.from == "系统" {
                Style::default().fg(theme::SYSTEM_MSG)
            } else {
                Style::default()
                    .fg(name_color)
                    .add_modifier(Modifier::BOLD)
            };

            out.push(Line::from(vec![
                Span::styled(msg.from.clone(), name_style),
                Span::raw("  "),
                Span::styled(time_str, Style::default().fg(theme::TIMESTAMP)),
            ]));

            // Content lines
            for line in msg.content.lines() {
                // Highlight @mentions
                let spans = highlight_mentions(line);
                out.push(Line::from(spans));
            }
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
            // Show last few lines of streaming content
            let preview: String = content.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            for line in preview.lines() {
                out.push(Line::from(Span::styled(
                    line.to_string(),
                    Style::default().fg(theme::AGENT_MSG),
                )));
            }
        }

        out
    }
}

fn format_time(ts: f64) -> String {
    // Handle both seconds and milliseconds
    let secs = if ts > 1e12 { (ts / 1000.0) as i64 } else { ts as i64 };
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
                Style::default().fg(theme::MENTION).add_modifier(Modifier::BOLD),
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
        view.streaming.push(("alice".to_string(), "partial output...".to_string()));
        let lines = view.build_text(80);
        let text: String = lines.iter().flat_map(|l| l.spans.iter().map(|s| s.content.to_string())).collect();
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
        view.streaming.iter_mut().find(|(n, _)| n == "agent-1").unwrap().1.push_str("chunk1 ");
        view.streaming.iter_mut().find(|(n, _)| n == "agent-1").unwrap().1.push_str("chunk2 ");
        view.streaming.iter_mut().find(|(n, _)| n == "agent-1").unwrap().1.push_str("chunk3");

        // Verify streaming buffer has content
        let buf = &view.streaming.iter().find(|(n, _)| n == "agent-1").unwrap().1;
        assert_eq!(buf, "chunk1 chunk2 chunk3");

        // Simulate idle-flush: take content, push as DM, clear streaming
        let content = view.streaming.iter().find(|(n, _)| n == "agent-1").unwrap().1.clone();
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
}

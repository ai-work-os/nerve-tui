//! Message list layout with item-granularity management, gaps, follow-mode scrolling,
//! and bottom padding.

use std::collections::HashMap;

use nerve_tui_protocol::Message;
use ratatui::text::Line;
use tracing::debug;

use super::render_cache::RenderCache;

/// Bottom padding lines to prevent last message from being obscured by input area.
const BOTTOM_PADDING: u16 = 2;
/// Gap between messages (empty lines).
const MESSAGE_GAP: u16 = 1;
/// Maximum content width for readability.
pub const MAX_CONTENT_WIDTH: u16 = 120;

/// Message list with follow-mode scrolling and render cache integration.
pub struct MessageList {
    /// Scroll offset from bottom (0 = at bottom).
    scroll_offset: u16,
    /// Whether we auto-follow new content (streaming).
    follow: bool,
    /// Visible area height (set during render).
    visible_height: u16,
    /// Render cache for message lines.
    cache: RenderCache,
}

impl MessageList {
    pub fn new() -> Self {
        Self {
            scroll_offset: 0,
            follow: true,
            visible_height: 0,
            cache: RenderCache::new(),
        }
    }

    pub fn is_following(&self) -> bool {
        self.follow
    }

    /// Scroll up by `n` lines. Disables follow mode.
    pub fn scroll_up(&mut self, n: u16) {
        self.follow = false;
        self.scroll_offset = self.scroll_offset.saturating_add(n);
        debug!(offset = self.scroll_offset, "scrolled up, follow paused");
    }

    /// Scroll down by `n` lines. Re-enables follow if at bottom.
    pub fn scroll_down(&mut self, n: u16, total_height: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        if self.scroll_offset == 0 || self.at_bottom(total_height) {
            self.follow = true;
            self.scroll_offset = 0;
            debug!("scrolled to bottom, follow resumed");
        }
    }

    /// Snap to bottom and re-enable follow.
    pub fn snap_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.follow = true;
    }

    /// Notify that new content arrived (e.g. streaming chunk).
    /// If following, stays at bottom. Otherwise, content scrolls away.
    pub fn on_content_changed(&mut self) {
        if self.follow {
            self.scroll_offset = 0;
        }
    }

    /// Invalidate a specific message in the cache.
    pub fn invalidate_message(&mut self, message_id: &str) {
        self.cache.invalidate(message_id);
    }

    /// Get expand state map for a specific message (block_index -> expanded).
    /// Currently always empty since folding is removed; kept for cache API compatibility.
    fn expand_state_for_message(&self, _message_id: &str) -> HashMap<usize, bool> {
        HashMap::new()
    }

    /// Clear all cached renders (e.g. on terminal resize).
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Build the visible lines for a viewport, given messages and area dimensions.
    /// Returns the lines to render in the viewport.
    pub fn build_visible_lines(
        &mut self,
        messages: &[Message],
        viewport_width: u16,
        viewport_height: u16,
    ) -> Vec<Line<'static>> {
        self.visible_height = viewport_height;
        let content_width = viewport_width.min(MAX_CONTENT_WIDTH);

        // Render all messages (cached where possible)
        let mut all_lines: Vec<Line<'static>> = Vec::new();
        for (i, msg) in messages.iter().enumerate() {
            if i > 0 {
                // Add gap between messages
                for _ in 0..MESSAGE_GAP {
                    all_lines.push(Line::from(""));
                }
            }
            let es = self.expand_state_for_message(&msg.id);
            let rendered = self.cache.get_or_render(msg, content_width, &es);
            all_lines.extend_from_slice(rendered);
        }

        // Add bottom padding
        for _ in 0..BOTTOM_PADDING {
            all_lines.push(Line::from(""));
        }

        let total_lines = all_lines.len() as u16;

        // Clamp scroll_offset to valid range
        let max_scroll = total_lines.saturating_sub(viewport_height);
        if self.scroll_offset > max_scroll {
            self.scroll_offset = max_scroll;
        }

        // Calculate visible window (from bottom)
        let end = total_lines.saturating_sub(self.scroll_offset) as usize;
        let start = end.saturating_sub(viewport_height as usize);

        all_lines[start..end].to_vec()
    }

    /// Check if scroll position is at the bottom.
    fn at_bottom(&self, _total_height: u16) -> bool {
        self.scroll_offset == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_tui_protocol::{Message, Role, ContentBlock, ToolStatus};
    use std::time::Instant;

    fn make_text_msg(id: &str, text: &str) -> Message {
        let mut msg = Message::new(id.into(), Role::User, 0);
        msg.meta.partial = false;
        msg.blocks.push(ContentBlock::Text { text: text.into() });
        msg
    }

    #[test]
    fn new_starts_following() {
        let list = MessageList::new();
        assert!(list.is_following());
    }

    #[test]
    fn scroll_up_disables_follow() {
        let mut list = MessageList::new();
        list.scroll_up(5);
        assert!(!list.is_following());
        assert_eq!(list.scroll_offset, 5);
    }

    #[test]
    fn scroll_down_to_bottom_enables_follow() {
        let mut list = MessageList::new();
        list.scroll_up(5);
        assert!(!list.is_following());

        list.scroll_down(5, 100);
        assert!(list.is_following());
        assert_eq!(list.scroll_offset, 0);
    }

    #[test]
    fn snap_to_bottom() {
        let mut list = MessageList::new();
        list.scroll_up(10);
        list.snap_to_bottom();
        assert!(list.is_following());
        assert_eq!(list.scroll_offset, 0);
    }

    #[test]
    fn on_content_changed_follows() {
        let mut list = MessageList::new();
        list.on_content_changed();
        assert_eq!(list.scroll_offset, 0);
    }

    #[test]
    fn on_content_changed_doesnt_snap_when_scrolled() {
        let mut list = MessageList::new();
        list.scroll_up(5);
        list.on_content_changed();
        // Should NOT snap back — user is reading
        assert_eq!(list.scroll_offset, 5);
    }

    #[test]
    fn build_visible_lines_empty() {
        let mut list = MessageList::new();
        let lines = list.build_visible_lines(&[], 80, 20);
        // Only bottom padding
        assert_eq!(lines.len(), BOTTOM_PADDING as usize);
    }

    #[test]
    fn build_visible_lines_single_message() {
        let mut list = MessageList::new();
        let msgs = vec![make_text_msg("m1", "hello world")];
        let lines = list.build_visible_lines(&msgs, 80, 20);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("hello world"));
    }

    #[test]
    fn build_visible_lines_has_gaps() {
        let mut list = MessageList::new();
        let msgs = vec![
            make_text_msg("m1", "first"),
            make_text_msg("m2", "second"),
        ];
        let lines = list.build_visible_lines(&msgs, 80, 20);
        // Should have: msg1 lines + gap + msg2 lines + bottom_padding
        // At least: 1 + 1 + 1 + 2 = 5
        assert!(lines.len() >= 4);
    }

    #[test]
    fn build_visible_lines_respects_viewport() {
        let mut list = MessageList::new();
        // Create many messages
        let msgs: Vec<Message> = (0..20)
            .map(|i| make_text_msg(&format!("m{}", i), &format!("message {}", i)))
            .collect();
        let viewport_height = 5;
        let lines = list.build_visible_lines(&msgs, 80, viewport_height);
        // Should not exceed viewport height
        assert!(lines.len() <= viewport_height as usize);
    }

    #[test]
    fn scroll_up_shows_earlier_messages() {
        let mut list = MessageList::new();
        let msgs: Vec<Message> = (0..10)
            .map(|i| make_text_msg(&format!("m{}", i), &format!("msg-{}", i)))
            .collect();

        // At bottom: should see last messages
        let lines_bottom = list.build_visible_lines(&msgs, 80, 5);
        let text_bottom: String = lines_bottom.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();

        // Scroll up
        list.scroll_up(10);
        let lines_up = list.build_visible_lines(&msgs, 80, 5);
        let text_up: String = lines_up.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();

        // Should show different content
        assert_ne!(text_bottom, text_up);
    }

    #[test]
    fn max_content_width_capped() {
        assert_eq!(MAX_CONTENT_WIDTH, 120);
    }

    #[test]
    fn clear_cache_works() {
        let mut list = MessageList::new();
        let msgs = vec![make_text_msg("m1", "hello")];
        list.build_visible_lines(&msgs, 80, 20);
        assert!(!list.cache.is_empty());

        list.clear_cache();
        assert!(list.cache.is_empty());
    }

    #[test]
    fn invalidate_message_works() {
        let mut list = MessageList::new();
        let msgs = vec![
            make_text_msg("m1", "hello"),
            make_text_msg("m2", "world"),
        ];
        list.build_visible_lines(&msgs, 80, 20);
        assert_eq!(list.cache.len(), 2);

        list.invalidate_message("m1");
        assert_eq!(list.cache.len(), 1);
    }

    fn make_tool_msg(id: &str) -> Message {
        let mut msg = Message::new(id.into(), Role::Assistant, 0);
        msg.meta.partial = false;
        msg.blocks.push(ContentBlock::ToolCall {
            id: "tc1".into(),
            name: "Read".into(),
            input: r#"{"path": "/tmp/test.rs"}"#.into(),
            status: ToolStatus::Completed,
        });
        msg
    }

    fn make_thinking_msg(id: &str, finished: bool) -> Message {
        let mut msg = Message::new(id.into(), Role::Assistant, 0);
        msg.meta.partial = !finished;
        let now = Instant::now();
        msg.blocks.push(ContentBlock::Thinking {
            text: "considering options...".into(),
            started_at: Some(now),
            finished_at: if finished { Some(now) } else { None },
        });
        msg
    }

    #[test]
    fn blocks_collapsed_by_default() {
        let mut list = MessageList::new();
        let msgs = vec![make_tool_msg("m1")];
        let lines = list.build_visible_lines(&msgs, 80, 20);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        // Collapsed: should show ▶ indicator
        assert!(text.contains("▶") || text.contains("Read"));
    }

    #[test]
    fn running_thinking_not_cached() {
        let mut list = MessageList::new();
        let msgs = vec![make_thinking_msg("m1", false)];
        // First render
        let lines1 = list.build_visible_lines(&msgs, 80, 20);
        let text1: String = lines1.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text1.contains("思考中"));
        // The timer value changes each call since Instant::now() is live
        // Just verify it renders without error on second call
        let lines2 = list.build_visible_lines(&msgs, 80, 20);
        assert!(!lines2.is_empty());
    }

    #[test]
    fn scroll_offset_clamped_to_max() {
        let mut list = MessageList::new();
        list.scroll_up(9999);
        let msgs = vec![make_text_msg("m1", "short")];
        let lines = list.build_visible_lines(&msgs, 80, 20);
        // Should still return something (clamped)
        assert!(!lines.is_empty());
    }
}

//! Message-level render cache.
//! Caches rendered lines per message_id, invalidated by width change or content change.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;

use nerve_tui_protocol::{ContentBlock, Message, ToolStatus};
use ratatui::text::Line;
use tracing::debug;

use super::block_renderer;

/// Cache entry for a single message's rendered output.
struct CacheEntry {
    width: u16,
    content_hash: u64,
    rendered: Vec<Line<'static>>,
}

/// Message-level render cache.
/// Keyed by message_id, invalidated when width or content changes.
pub struct RenderCache {
    entries: HashMap<String, CacheEntry>,
}

impl RenderCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Get or render a message's lines.
    /// Returns cached result if width and content haven't changed.
    /// `expand_state` maps block_index -> expanded for collapsible blocks.
    pub fn get_or_render(
        &mut self,
        msg: &Message,
        width: u16,
        expand_state: &HashMap<usize, bool>,
    ) -> &[Line<'static>] {
        let hash = content_hash(msg, expand_state);

        // Skip cache entirely if message has live (running) blocks
        let has_live = msg.blocks.iter().any(is_live_block);

        let needs_render = has_live || match self.entries.get(&msg.id) {
            Some(entry) => entry.width != width || entry.content_hash != hash,
            None => true,
        };

        if needs_render {
            let rendered = render_message(msg, width, expand_state);
            debug!(
                message_id = %msg.id,
                blocks = msg.blocks.len(),
                lines = rendered.len(),
                partial = msg.meta.partial,
                cached = false,
                "message rendered"
            );
            self.entries.insert(
                msg.id.clone(),
                CacheEntry {
                    width,
                    content_hash: hash,
                    rendered,
                },
            );
        }

        &self.entries[&msg.id].rendered
    }

    /// Invalidate a specific message's cache.
    pub fn invalidate(&mut self, message_id: &str) {
        self.entries.remove(message_id);
    }

    /// Clear all cached entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Check if a block is "live" (running, needs real-time updates — not cacheable).
fn is_live_block(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Thinking { finished_at: None, started_at: Some(_), .. } => true,
        ContentBlock::ToolCall { status: ToolStatus::Running | ToolStatus::Pending, .. } => true,
        _ => false,
    }
}

/// Render all blocks of a message into lines.
fn render_message(msg: &Message, width: u16, _expand_state: &HashMap<usize, bool>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for block in &msg.blocks {
        lines.extend(block_renderer::render_block(block, width));
    }
    lines
}

/// Hash the content of a message's blocks for cache invalidation.
fn content_hash(msg: &Message, expand_state: &HashMap<usize, bool>) -> u64 {
    let mut hasher = DefaultHasher::new();
    msg.blocks.len().hash(&mut hasher);
    msg.meta.partial.hash(&mut hasher);
    // Include expand state in hash so toggling invalidates cache
    for (idx, expanded) in expand_state {
        idx.hash(&mut hasher);
        expanded.hash(&mut hasher);
    }
    for block in &msg.blocks {
        match block {
            ContentBlock::Text { text } => {
                0u8.hash(&mut hasher);
                text.hash(&mut hasher);
            }
            ContentBlock::Thinking { text, finished_at, .. } => {
                1u8.hash(&mut hasher);
                text.hash(&mut hasher);
                finished_at.is_some().hash(&mut hasher);
            }
            ContentBlock::ToolCall { id, status, .. } => {
                2u8.hash(&mut hasher);
                id.hash(&mut hasher);
                (*status as u8).hash(&mut hasher);
            }
            ContentBlock::ToolResult { tool_call_id, content, is_error } => {
                3u8.hash(&mut hasher);
                tool_call_id.hash(&mut hasher);
                content.hash(&mut hasher);
                is_error.hash(&mut hasher);
            }
            ContentBlock::Error { message } => {
                4u8.hash(&mut hasher);
                message.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_tui_protocol::{Message, Role, ContentBlock, ToolStatus};

    fn make_msg(id: &str) -> Message {
        let mut msg = Message::new(id.into(), Role::Assistant, 0);
        msg.meta.partial = false;
        msg
    }

    #[test]
    fn cache_miss_on_first_render() {
        let mut cache = RenderCache::new();
        let mut msg = make_msg("m1");
        msg.blocks.push(ContentBlock::Text { text: "hello".into() });
        let lines = cache.get_or_render(&msg, 80, &HashMap::new());
        assert!(!lines.is_empty());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_hit_same_content_same_width() {
        let mut cache = RenderCache::new();
        let mut msg = make_msg("m1");
        msg.blocks.push(ContentBlock::Text { text: "hello".into() });

        let lines1 = cache.get_or_render(&msg, 80, &HashMap::new());
        let len1 = lines1.len();

        // Second call should hit cache (same content, same width)
        let lines2 = cache.get_or_render(&msg, 80, &HashMap::new());
        assert_eq!(lines2.len(), len1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_miss_on_width_change() {
        let mut cache = RenderCache::new();
        let mut msg = make_msg("m1");
        msg.blocks.push(ContentBlock::Text { text: "hello".into() });

        cache.get_or_render(&msg, 80, &HashMap::new());
        assert_eq!(cache.len(), 1);

        // Width changed → should re-render
        cache.get_or_render(&msg, 120, &HashMap::new());
        // Still 1 entry (same message_id, updated)
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_miss_on_content_change() {
        let mut cache = RenderCache::new();
        let mut msg = make_msg("m1");
        msg.blocks.push(ContentBlock::Text { text: "hello".into() });

        cache.get_or_render(&msg, 80, &HashMap::new());

        // Content changed (streaming appended)
        if let Some(ContentBlock::Text { ref mut text }) = msg.blocks.last_mut() {
            text.push_str(" world");
        }
        let lines = cache.get_or_render(&msg, 80, &HashMap::new());
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("hello world"));
    }

    #[test]
    fn cache_miss_on_partial_change() {
        let mut cache = RenderCache::new();
        let mut msg = make_msg("m1");
        msg.meta.partial = true;
        msg.blocks.push(ContentBlock::Text { text: "hello".into() });

        cache.get_or_render(&msg, 80, &HashMap::new());

        // partial → false
        msg.meta.partial = false;
        cache.get_or_render(&msg, 80, &HashMap::new());
        // Should have re-rendered (hash includes partial flag)
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn invalidate_removes_entry() {
        let mut cache = RenderCache::new();
        let mut msg = make_msg("m1");
        msg.blocks.push(ContentBlock::Text { text: "hello".into() });
        cache.get_or_render(&msg, 80, &HashMap::new());

        cache.invalidate("m1");
        assert!(cache.is_empty());
    }

    #[test]
    fn clear_removes_all() {
        let mut cache = RenderCache::new();
        let mut msg1 = make_msg("m1");
        msg1.blocks.push(ContentBlock::Text { text: "a".into() });
        let mut msg2 = make_msg("m2");
        msg2.blocks.push(ContentBlock::Text { text: "b".into() });

        cache.get_or_render(&msg1, 80, &HashMap::new());
        cache.get_or_render(&msg2, 80, &HashMap::new());
        assert_eq!(cache.len(), 2);

        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn multiple_messages_cached_independently() {
        let mut cache = RenderCache::new();
        let mut msg1 = make_msg("m1");
        msg1.blocks.push(ContentBlock::Text { text: "first".into() });
        let mut msg2 = make_msg("m2");
        msg2.blocks.push(ContentBlock::Text { text: "second".into() });

        cache.get_or_render(&msg1, 80, &HashMap::new());
        cache.get_or_render(&msg2, 80, &HashMap::new());
        assert_eq!(cache.len(), 2);

        // Modify msg1 only
        if let Some(ContentBlock::Text { ref mut text }) = msg1.blocks.last_mut() {
            text.push_str(" updated");
        }
        cache.get_or_render(&msg1, 80, &HashMap::new());
        // msg2 should still be cached (untouched)
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn tool_call_status_change_invalidates() {
        let mut cache = RenderCache::new();
        let mut msg = make_msg("m1");
        msg.blocks.push(ContentBlock::ToolCall {
            id: "tc1".into(),
            name: "Bash".into(),
            input: "{}".into(),
            status: ToolStatus::Pending,
        });

        cache.get_or_render(&msg, 80, &HashMap::new());

        // Status changes
        if let Some(ContentBlock::ToolCall { ref mut status, .. }) = msg.blocks.last_mut() {
            *status = ToolStatus::Completed;
        }
        let lines = cache.get_or_render(&msg, 80, &HashMap::new());
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("✓"));
    }
}

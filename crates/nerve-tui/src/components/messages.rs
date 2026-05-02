//! Shared utility functions used by channel_view and dm_view.

use chrono::{Local, TimeZone};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme;

/// Extract the first @mention as route target, return (target, content_without_prefix).
/// "@bob hello world" → (Some("bob"), "hello world")
/// "no mention here"  → (None, "no mention here")
pub(crate) fn extract_route_target(content: &str) -> (Option<String>, String) {
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

pub(crate) fn format_time(ts: f64) -> String {
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

pub(crate) fn format_time_short(ts: f64) -> String {
    let secs = if ts > 1e12 {
        (ts / 1000.0) as i64
    } else {
        ts as i64
    };
    Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_default()
}

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

pub(crate) fn build_message_footer(from: &str, model: &str, duration_secs: Option<f64>) -> Line<'static> {
    if from == "user" {
        return Line::from(vec![]);
    }
    let t = theme::current();
    let agent_c = t.agent_color(from);
    let mut spans = vec![
        Span::styled("▣ ", Style::default().fg(agent_c)),
        Span::styled(from.to_string(), Style::default().fg(t.text)),
    ];
    if !model.is_empty() {
        spans.push(Span::styled(
            format!(" · {}", model),
            Style::default().fg(t.text_muted),
        ));
    }
    if let Some(d) = duration_secs {
        spans.push(Span::styled(
            format!(" · {:.1}s", d),
            Style::default().fg(t.text_muted),
        ));
    }
    Line::from(spans)
}

pub(crate) fn compact_rendered_lines(lines: &mut Vec<Line<'static>>) {
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
    while lines
        .last()
        .map_or(false, |l| {
            l.spans.is_empty() || l.spans.iter().all(|s| s.content.trim().is_empty())
        })
    {
        lines.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::channel_view::ChannelView;
    use crate::components::dm_view::DmView;
    use nerve_tui_protocol::{DmMessage, MessageInfo};

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

    fn make_dm(role: &str, content: &str) -> DmMessage {
        DmMessage {
            role: role.into(),
            content: content.into(),
            timestamp: 1710000000,
        }
    }

    // --- ChannelView tests ---

    #[test]
    fn push_and_count() {
        let mut cv = ChannelView::new();
        assert_eq!(cv.messages.len(), 0);
        cv.push(&make_msg("alice", "hello"), true);
        assert_eq!(cv.messages.len(), 1);
        cv.push_system("system info");
        assert_eq!(cv.messages.len(), 2);
        assert_eq!(cv.messages[1].from, "系统");
    }

    #[test]
    fn clear_removes_all() {
        let mut cv = ChannelView::new();
        cv.push(&make_msg("alice", "hello"), true);
        cv.push(&make_msg("bob", "world"), true);
        cv.clear();
        assert_eq!(cv.messages.len(), 0);
    }

    #[test]
    fn filter_by_agent_name() {
        let mut cv = ChannelView::new();
        cv.push(&make_msg("alice", "hello"), true);
        cv.push(&make_msg("bob", "world"), true);
        cv.push(&make_msg("alice", "hi again"), true);

        cv.filter = None;
        let lines = cv.build_text_pub(80);
        assert!(lines.len() >= 3);

        cv.filter = Some("alice".to_string());
        let filtered = cv.build_text_pub(80);
        for line in &filtered {
            let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            assert!(!text.contains("bob") || text.is_empty());
        }
    }

    #[test]
    fn filter_includes_mentions() {
        let mut cv = ChannelView::new();
        cv.push(&make_msg("bob", "@alice check this"), true);
        cv.filter = Some("alice".to_string());
        let lines = cv.build_text_pub(80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn route_display_in_build_text() {
        let mut cv = ChannelView::new();
        cv.push(&make_msg("alice", "@bob check this"), true);
        let lines = cv.build_text_pub(80);
        let header: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(header.contains("alice"), "header has sender");
        assert!(header.contains("→"), "header has arrow");
        assert!(header.contains("bob"), "header has target");
        let content: String = lines[1].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(!content.starts_with("@bob"), "content stripped @mention prefix");
        assert!(content.contains("check this"), "content preserved");
    }

    #[test]
    fn time_interval_in_header() {
        let mut cv = ChannelView::new();
        let mut msg1 = make_msg("alice", "first");
        msg1.timestamp = 1710000000.0;
        cv.push(&msg1, true);
        let mut msg2 = make_msg("bob", "second");
        msg2.timestamp = 1710000015.0;
        cv.push(&msg2, true);

        let lines = cv.build_text_pub(80);
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
        let mut cv = ChannelView::new();
        cv.push(&make_msg("alice", "first"), true);
        let lines = cv.build_text_pub(80);
        let header_text: String = lines[0].spans.iter()
            .map(|s| s.content.to_string()).collect();
        assert!(!header_text.contains('+'), "first message should have no interval");
    }

    #[test]
    fn filter_shows_in_title() {
        let mut cv = ChannelView::new();
        cv.filter = Some("alice".to_string());
        let title = if let Some(ref f) = cv.filter {
            format!(" Messages [@{}] ", f)
        } else {
            " Messages ".to_string()
        };
        assert!(title.contains("@alice"));
    }

    #[test]
    fn filter_none_shows_all_messages() {
        let mut cv = ChannelView::new();
        cv.push(&make_msg("alice", "a msg"), true);
        cv.push(&make_msg("bob", "b msg"), true);
        cv.filter = None;
        let lines = cv.build_text_pub(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("alice"));
        assert!(text.contains("bob"));
    }

    #[test]
    fn filter_agent_excludes_others() {
        let mut cv = ChannelView::new();
        cv.push(&make_msg("alice", "hello"), true);
        cv.push(&make_msg("bob", "world"), true);
        cv.push(&make_msg("charlie", "@alice hey"), true);
        cv.filter = Some("alice".to_string());
        let lines = cv.build_text_pub(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("alice"), "alice's own message included");
        assert!(text.contains("charlie"), "charlie's message mentioning alice included");
        assert!(!lines.iter().any(|l| {
            l.spans.iter().any(|s| s.content.as_ref() == "bob")
        }), "bob's non-mentioning message excluded");
    }

    // --- DmView tests ---

    #[test]
    fn dm_push_and_render() {
        let mut dm = DmView::new("agent-1");
        dm.push(&make_dm("user", "hello"));
        dm.push(&make_dm("assistant", "hi there"));

        let lines = dm.build_text(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("user"));
        assert!(text.contains("hello"));
        assert!(text.contains("assistant"));
        assert!(text.contains("hi there"));
    }

    #[test]
    fn dm_exit_clears() {
        let mut dm = DmView::new("agent-1");
        dm.push(&make_dm("user", "hello"));
        assert!(!dm.messages.is_empty());
        dm.clear();
        assert!(dm.messages.is_empty());
    }

    #[test]
    fn dm_streaming_then_flush_persists() {
        let mut dm = DmView::new("agent-1");
        dm.push(&make_dm("user", "hello"));

        // Use structured pipeline
        dm.start_streaming_message("agent-1");
        let chunk1 = serde_json::json!({ "content": { "text": "chunk1 " } });
        let chunk2 = serde_json::json!({ "content": { "text": "chunk2 " } });
        let chunk3 = serde_json::json!({ "content": { "text": "chunk3" } });
        dm.apply_streaming_event("agent-1", "agent_message_chunk", &chunk1);
        dm.apply_streaming_event("agent-1", "agent_message_chunk", &chunk2);
        dm.apply_streaming_event("agent-1", "agent_message_chunk", &chunk3);

        // Flush: take message, convert to DmMessage
        let msg = dm.take_streaming_message("agent-1").unwrap();
        let content = super::super::dm_view::blocks_to_text(&msg.blocks);
        dm.push_with_blocks(&make_dm("assistant", &content), msg.blocks);

        assert!(dm.streaming_messages.is_empty());
        assert_eq!(dm.messages.len(), 2);
    }

    #[test]
    fn dm_channel_origin_strips_prefix_from_content() {
        let mut dm = DmView::new("agent-1");
        dm.push(&make_dm("user", "[channel: ch_123] from: main\n\n@agent-1 fix this bug"));
        let lines = dm.build_text(80);
        // Header is simplified: no channel origin shown there anymore
        let header: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(header.contains("user"), "header shows sender");
        assert!(!header.contains("来自"), "channel origin removed from header");
        let content: String = lines[1..].iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(!content.contains("[channel:"), "content stripped channel prefix");
        assert!(content.contains("fix this bug"), "content preserved");
    }

    #[test]
    fn dm_normal_message_no_tag() {
        let mut dm = DmView::new("agent-1");
        dm.push(&make_dm("user", "hello there"));
        let lines = dm.build_text(80);
        let header: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(!header.contains("来自"), "normal DM has no channel tag");
    }

    #[test]
    fn streaming_preview() {
        let mut dm = DmView::new("alice");
        dm.start_streaming_message("alice");
        let update = serde_json::json!({ "content": { "text": "partial output..." } });
        dm.apply_streaming_event("alice", "agent_message_chunk", &update);
        let lines = dm.build_text(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("alice"));
        assert!(text.contains("partial output..."));
    }

    #[test]
    fn streaming_preview_shows_more_than_3_lines() {
        let mut dm = DmView::new("agent");
        dm.visible_height = 20;
        let long_content = (0..10).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
        dm.start_streaming_message("agent");
        let update = serde_json::json!({ "content": { "text": long_content } });
        dm.apply_streaming_event("agent", "agent_message_chunk", &update);
        let lines = dm.build_text(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("line 0"), "should show first line");
        assert!(text.contains("line 9"), "should show last line");
    }

    #[test]
    fn streaming_preview_renders_blocks() {
        // With the new pipeline, streaming renders via block_renderer.
        // This test verifies that structured blocks get rendered.
        let mut dm = DmView::new("agent");
        dm.start_streaming_message("agent");
        let update = serde_json::json!({ "content": { "text": "hello world" } });
        dm.apply_streaming_event("agent", "agent_message_chunk", &update);
        let lines = dm.build_text(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("hello world"));
    }

    #[test]
    fn blink_tick_toggles() {
        let mut dm = DmView::new("agent");
        assert!(dm.cursor_visible());
        for i in 1..=14 {
            let v = dm.tick_blink();
            assert!(v, "tick {} should be visible", i);
        }
        assert!(!dm.tick_blink(), "tick 15 should be invisible");
        for _ in 16..=29 {
            assert!(!dm.tick_blink());
        }
        assert!(dm.tick_blink(), "tick 30 should be visible");
    }

    #[test]
    fn streaming_cursor_blinks_in_output() {
        let mut dm = DmView::new("agent");
        dm.start_streaming_message("agent");
        let update = serde_json::json!({ "content": { "text": "text" } });
        dm.apply_streaming_event("agent", "agent_message_chunk", &update);

        let lines = dm.build_text(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("▌"), "cursor visible in on phase");

        for _ in 0..15 {
            dm.tick_blink();
        }
        let lines = dm.build_text(80);
        let text: String = lines.iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(!text.contains("▌"), "cursor hidden in off phase");
    }

    #[test]
    fn dm_replay_flush_on_user_message() {
        let mut dm = DmView::new("agent-1");
        dm.push(&make_dm("user", "question 1"));

        // Simulate streaming + flush via structured pipeline
        dm.start_streaming_message("agent-1");
        let chunk = serde_json::json!({ "content": { "text": "answer 1" } });
        dm.apply_streaming_event("agent-1", "agent_message_chunk", &chunk);
        let msg = dm.take_streaming_message("agent-1").unwrap();
        let content = super::super::dm_view::blocks_to_text(&msg.blocks);
        dm.push_with_blocks(&make_dm("assistant", &content), msg.blocks);

        dm.push(&make_dm("user", "question 2"));

        dm.start_streaming_message("agent-1");
        let chunk = serde_json::json!({ "content": { "text": "answer 2" } });
        dm.apply_streaming_event("agent-1", "agent_message_chunk", &chunk);
        let msg = dm.take_streaming_message("agent-1").unwrap();
        let content = super::super::dm_view::blocks_to_text(&msg.blocks);
        dm.push_with_blocks(&make_dm("assistant", &content), msg.blocks);

        assert_eq!(dm.messages.len(), 4);
        assert_eq!(dm.messages[0].from, "user");
        assert_eq!(dm.messages[0].content, "question 1");
        assert_eq!(dm.messages[1].from, "assistant");
        assert_eq!(dm.messages[1].content, "answer 1");
        assert_eq!(dm.messages[2].from, "user");
        assert_eq!(dm.messages[2].content, "question 2");
        assert_eq!(dm.messages[3].from, "assistant");
        assert_eq!(dm.messages[3].content, "answer 2");
    }

    // --- build_message_footer tests ---

    #[test]
    fn footer_contains_agent_info() {
        let footer = build_message_footer("claude", "opus-4", Some(2.3));
        let text: String = footer.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("▣"));
        assert!(text.contains("claude"));
        assert!(text.contains("opus-4"));
        assert!(text.contains("2.3s"));
    }

    #[test]
    fn user_footer_is_empty() {
        let footer = build_message_footer("user", "", None);
        assert!(footer.spans.is_empty());
    }

    #[test]
    fn footer_without_model() {
        let footer = build_message_footer("claude", "", None);
        let text: String = footer.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("▣"));
        assert!(text.contains("claude"));
        assert!(!text.contains("·")); // No model separator
    }

    // --- Shared utility tests ---

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
        let (target, content) = extract_route_target("hello @bob world");
        assert_eq!(target, None);
        assert_eq!(content, "hello @bob world");
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
        assert_eq!(format_interval(1710000000000.0, 1710000015000.0), "+15s");
    }

    #[test]
    fn scroll_channel_and_dm() {
        let mut cv = ChannelView::new();
        cv.scroll_up(5);
        cv.scroll_down(10);
        cv.snap_to_bottom();

        let mut dm = DmView::new("agent");
        dm.scroll_up(5);
        dm.scroll_down(10);
        dm.snap_to_bottom();
    }
}

mod app_state;
mod event_handler;
mod renderer;

// Re-export the public API
pub use app_state::App;

use anyhow::Result;
use crossterm::event::Event;
use futures_util::StreamExt;
use nerve_tui_core::Transport;
use tracing::debug;

impl<T: Transport> App<T> {
    /// Initialize: fetch channels, nodes, join first channel if exists.
    pub async fn init(&mut self) -> Result<()> {
        self.refresh_agents().await;
        self.refresh_channels().await;

        // Validate active_channel is in the current filtered list, fallback if not
        let should_join = if let Some(ref ch_id) = self.active_channel {
            if self.channels.iter().any(|c| c.id == *ch_id) {
                None // already valid
            } else {
                self.active_channel = None;
                self.channels.first().map(|c| c.id.clone())
            }
        } else {
            self.channels.first().map(|c| c.id.clone())
        };

        if let Some(ch_id) = should_join {
            self.join_channel(&ch_id).await;
        }

        self.update_completions();
        Ok(())
    }

    pub async fn run(
        &mut self,
        terminal: &mut ratatui::Terminal<impl ratatui::backend::Backend>,
    ) -> Result<()> {
        let mut event_stream = crossterm::event::EventStream::new();
        let mut redraw_interval = tokio::time::interval(tokio::time::Duration::from_millis(33));

        loop {
            if self.force_clear {
                terminal.clear()?;
                self.force_clear = false;
                self.needs_redraw = true;
            }
            if self.needs_redraw {
                terminal.draw(|frame| self.render(frame))?;
                self.needs_redraw = false;
            }

            if self.should_quit {
                break;
            }

            // Wait for at least one event
            tokio::select! {
                Some(Ok(evt)) = event_stream.next() => {
                    match evt {
                        Event::Key(key) => {
                            debug!("key event: code={:?} modifiers={:?}", key.code, key.modifiers);
                            self.handle_key(key).await;
                        }
                        Event::Mouse(mouse) => self.handle_mouse(mouse),
                        Event::Paste(text) => self.handle_paste(&text).await,
                        _ => {}
                    }
                    self.needs_redraw = true;
                }
                Some(event) = self.event_rx.recv() => {
                    self.handle_nerve_event(event).await;
                    // Drain all pending nerve events before redrawing (batch chunks)
                    while let Ok(event) = self.event_rx.try_recv() {
                        self.handle_nerve_event(event).await;
                    }
                    self.needs_redraw = true;
                }
                Some(err_msg) = self.error_rx.recv() => {
                    self.channel_view.push_system(&err_msg);
                    self.needs_redraw = true;
                }
                _ = redraw_interval.tick() => {
                    // Periodic redraw for animations (blink cursor, thinking timer)
                    self.needs_redraw = true;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_tui_core::mock::MockTransport;
    use ratatui::layout::Rect;
    use tokio::sync::mpsc;

    use nerve_tui_protocol::*;

    use super::app_state::{SplitFocus, SplitPanel, SplitTarget};
    use crate::components::channel_view::{self, ChannelPanelState};
    use crate::components::dm_view::DmView;
    use crate::components::*;

    fn make_app() -> App<MockTransport> {
        let transport = MockTransport::new("test-user");
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        App::new(transport, event_rx)
    }

    #[test]
    fn project_name_uses_last_path_segment() {
        assert_eq!(
            App::<MockTransport>::project_name_from_path("/tmp/demo-project"),
            Some("demo-project".to_string())
        );
        assert_eq!(
            App::<MockTransport>::project_name_from_path("/tmp/demo-project/"),
            Some("demo-project".to_string())
        );
    }

    #[test]
    fn new_with_project_sets_project_context() {
        let transport = MockTransport::new("test-user");
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        let app = App::new_with_project(transport, event_rx, Some("/tmp/demo-project".into()));

        assert_eq!(app.project_path.as_deref(), Some("/tmp/demo-project"));
        assert_eq!(app.project_name.as_deref(), Some("demo-project"));
    }

    #[tokio::test]
    async fn handle_input_blocks_while_dm_is_responding() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.dm_view.is_responding = true;
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        app.handle_input("hello").await;

        assert_eq!(app.dm_view.dm_history.len(), 1);
        assert_eq!(app.dm_view.dm_history[0].role, "系统");
        assert!(app.dm_view.dm_history[0].content.contains("agent 正在回复"));
    }

    #[test]
    fn flush_streaming_as_dm_clears_responding_flag() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.dm_view.is_responding = true;
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };
        // Use structured pipeline
        app.dm_view.start_streaming_message("alice");
        let update = serde_json::json!({ "content": { "text": "partial" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &update);

        app.flush_streaming_as_dm("n1", "alice");

        assert!(!app.dm_view.is_responding);
        assert_eq!(app.dm_view.dm_history.len(), 1);
        assert_eq!(app.dm_view.dm_history[0].content, "partial");
    }

    #[test]
    fn flush_sets_summary_mode_true_for_auto_collapse() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.dm_view.is_responding = true;
        app.dm_view.summary_mode = false;
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        app.dm_view.start_streaming_message("alice");
        let think = serde_json::json!({ "content": { "text": "reasoning..." } });
        app.dm_view.apply_streaming_event("alice", "agent_thought_chunk", &think);
        let text = serde_json::json!({ "content": { "text": "answer" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &text);

        app.flush_streaming_as_dm("n1", "alice");

        assert!(
            app.dm_view.summary_mode,
            "after flush, summary_mode should be true (auto-collapse thinking/tool_call blocks)"
        );
    }

    #[test]
    fn reset_dm_before_enter_reinitializes_same_agent_dm() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };
        app.dm_view.start_streaming_message("alice");

        let old_node_id = app.reset_dm_before_enter("n1");

        assert_eq!(old_node_id.as_deref(), Some("n1"));
        assert!(!app.is_dm_mode());
        assert!(app.dm_view.streaming_messages.is_empty());
    }

    #[test]
    fn sync_navigation_selection_tracks_active_channel() {
        let mut app = make_app();
        app.channels = vec![
            ChannelDisplay {
                id: "ch1".into(),
                name: Some("main".into()),
                node_count: 2,
                members: Vec::new(),
                unread: 0,
            },
            ChannelDisplay {
                id: "ch2".into(),
                name: Some("ops".into()),
                node_count: 1,
                members: Vec::new(),
                unread: 0,
            },
        ];
        app.agents = vec![AgentDisplay {
            name: "alice".into(),
            status: "idle".into(),
            activity: None,
            adapter: Some("claude".into()),
            model: None,
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: None,
            tool_call_started: None,
            waiting_for: None,
        }];
        app.active_channel = Some("ch2".into());

        app.sync_navigation_selection();

        assert_eq!(app.status_bar.selected_nav, 1);
    }

    #[test]
    fn sync_navigation_selection_tracks_active_dm() {
        let mut app = make_app();
        app.channels = vec![ChannelDisplay {
            id: "ch1".into(),
            name: Some("main".into()),
            node_count: 2,
            members: Vec::new(),
            unread: 0,
        }];
        app.agents = vec![
            AgentDisplay {
                name: "alice".into(),
                status: "idle".into(),
                activity: None,
                adapter: Some("claude".into()),
                model: None,
                node_id: "n1".into(),
                transport: "stdio".into(),
                capabilities: vec![],
                usage: None,
                tool_call_name: None,
                tool_call_started: None,
                waiting_for: None,
            },
            AgentDisplay {
                name: "bob".into(),
                status: "busy".into(),
                activity: None,
                adapter: Some("codex".into()),
                model: None,
                node_id: "n2".into(),
                transport: "stdio".into(),
                capabilities: vec![],
                usage: None,
                tool_call_name: None,
                tool_call_started: None,
                waiting_for: None,
            },
        ];
        app.active_channel = Some("ch1".into());
        app.view_mode = ViewMode::Dm { node_id: "n2".into(), node_name: "bob".into() };

        app.sync_navigation_selection();

        assert_eq!(app.status_bar.selected_nav, 3);
    }

    #[tokio::test]
    async fn channel_closed_clears_active_channel() {
        let mut app = make_app();
        app.channels = vec![
            ChannelDisplay {
                id: "ch1".into(),
                name: Some("main".into()),
                node_count: 2,
                members: Vec::new(),
                unread: 0,
            },
            ChannelDisplay {
                id: "ch2".into(),
                name: Some("ops".into()),
                node_count: 1,
                members: Vec::new(),
                unread: 0,
            },
        ];
        app.active_channel = Some("ch1".into());

        app.handle_nerve_event(NerveEvent::ChannelClosed {
            channel_id: "ch1".into(),
            name: Some("main".into()),
        })
        .await;

        assert!(app.active_channel.is_none());
    }

    #[tokio::test]
    async fn channel_closed_other_channel_keeps_active() {
        let mut app = make_app();
        app.channels = vec![
            ChannelDisplay {
                id: "ch1".into(),
                name: Some("main".into()),
                node_count: 2,
                members: Vec::new(),
                unread: 0,
            },
            ChannelDisplay {
                id: "ch2".into(),
                name: Some("ops".into()),
                node_count: 1,
                members: Vec::new(),
                unread: 0,
            },
        ];
        app.active_channel = Some("ch1".into());

        app.handle_nerve_event(NerveEvent::ChannelClosed {
            channel_id: "ch2".into(),
            name: Some("ops".into()),
        })
        .await;

        assert_eq!(app.active_channel.as_deref(), Some("ch1"));
    }

    #[test]
    fn cwd_filter_respects_global_mode() {
        let transport = MockTransport::new("test-user");
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        let mut app = App::new_with_project(transport, event_rx, Some("/tmp/project".into()));

        assert_eq!(app.cwd_filter(), Some("/tmp/project"));

        app.global_mode = true;
        assert!(app.cwd_filter().is_none());

        app.global_mode = false;
        assert_eq!(app.cwd_filter(), Some("/tmp/project"));
    }

    #[test]
    fn cwd_filter_none_without_project_path() {
        let app = make_app();
        assert!(app.cwd_filter().is_none());
    }

    #[tokio::test]
    async fn dm_mode_does_not_swallow_other_channel_mention() {
        let mut app = make_app();
        app.dm_view = DmView::new("bob");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "bob".into() };
        app.active_channel = Some("ch1".into());

        let initial_count = app.channel_view.line_count();

        app.handle_nerve_event(NerveEvent::ChannelMention {
            channel_id: "ch2".into(),
            message: MessageInfo {
                id: "m1".into(),
                channel_id: "ch2".into(),
                from: "alice".into(),
                content: "@user hello".into(),
                timestamp: 1710000000.0,
                metadata: None,
            },
        })
        .await;

        assert_eq!(
            app.channel_view.line_count(),
            initial_count + 1,
            "non-active channel mention should display even in DM mode"
        );
    }

    #[tokio::test]
    async fn active_channel_mention_deduped() {
        let mut app = make_app();
        app.active_channel = Some("ch1".into());

        let initial_count = app.channel_view.line_count();

        app.handle_nerve_event(NerveEvent::ChannelMention {
            channel_id: "ch1".into(),
            message: MessageInfo {
                id: "m2".into(),
                channel_id: "ch1".into(),
                from: "alice".into(),
                content: "@user hello".into(),
                timestamp: 1710000000.0,
                metadata: None,
            },
        })
        .await;

        assert_eq!(
            app.channel_view.line_count(),
            initial_count,
            "active channel mention should be deduped"
        );
    }

    #[test]
    fn flush_streaming_from_structured_message() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.dm_view.is_responding = true;
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        app.dm_view.start_streaming_message("alice");
        let update = serde_json::json!({ "content": { "text": "hello world" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &update);

        app.flush_streaming_as_dm("n1", "alice");

        assert!(!app.dm_view.is_responding);
        assert_eq!(app.dm_view.dm_history.len(), 1);
        assert!(app.dm_view.dm_history[0].content.contains("hello world"));
        assert!(!app.dm_view.streaming_messages.contains_key("alice"));
    }

    #[test]
    fn flush_empty_streaming_messages_no_panic() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        app.flush_streaming_as_dm("n1", "alice");

        assert!(app.dm_view.dm_history.is_empty());
    }

    #[test]
    fn flush_structured_blocks_no_thinking_in_content() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        app.dm_view.start_streaming_message("alice");
        let think = serde_json::json!({ "content": { "text": "let me think..." } });
        app.dm_view.apply_streaming_event("alice", "agent_thought_chunk", &think);
        let text = serde_json::json!({ "content": { "text": "the answer is 42" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &text);

        app.flush_streaming_as_dm("n1", "alice");

        assert_eq!(app.dm_view.dm_history.len(), 1);
        let content = &app.dm_view.dm_history[0].content;
        assert!(!content.contains("think"), "thinking should not be in persisted content");
        assert!(content.contains("the answer is 42"));
    }

    #[test]
    fn flush_tool_call_blocks_include_status() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        app.dm_view.start_streaming_message("alice");
        let tc = serde_json::json!({
            "toolCall": { "name": "Read", "id": "tc1", "input": {} },
        });
        app.dm_view.apply_streaming_event("alice", "tool_call", &tc);
        let tcu = serde_json::json!({
            "toolCallUpdate": { "toolCallId": "tc1", "status": "completed", "result": { "value": "ok" } }
        });
        app.dm_view.apply_streaming_event("alice", "tool_call_update", &tcu);
        let text = serde_json::json!({ "content": { "text": "done" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &text);

        app.flush_streaming_as_dm("n1", "alice");

        assert_eq!(app.dm_view.dm_history.len(), 1);
        let content = &app.dm_view.dm_history[0].content;
        assert!(content.contains("done"));
        assert!(content.contains("[tool:Read"), "should have tool call in content");
    }

    #[test]
    fn flush_preserves_blocks_in_message_line() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        app.dm_view.start_streaming_message("alice");
        let text = serde_json::json!({ "content": { "text": "structured content" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &text);

        let before_count = app.dm_view.messages.len();
        app.flush_streaming_as_dm("n1", "alice");

        assert_eq!(app.dm_view.messages.len(), before_count + 1);
        let last_msg = app.dm_view.messages.last().unwrap();
        assert!(
            last_msg.blocks.iter().any(|b| matches!(b, ContentBlock::Text { .. })),
            "MessageLine should have structured Text blocks"
        );
    }

    #[tokio::test]
    async fn ctrl_l_sets_force_clear() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = make_app();
        app.force_clear = false;

        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)).await;

        assert!(app.force_clear, "Ctrl+L should set force_clear = true");
        assert!(app.needs_redraw, "Ctrl+L should also set needs_redraw = true");
    }

    #[test]
    fn ch_command_finds_channel_by_name() {
        let channels = vec![
            ChannelDisplay {
                id: "ch1".into(),
                name: Some("main".into()),
                node_count: 2,
                members: Vec::new(),
                unread: 0,
            },
            ChannelDisplay {
                id: "ch2".into(),
                name: Some("ops".into()),
                node_count: 1,
                members: Vec::new(),
                unread: 0,
            },
        ];

        let found = channels.iter().find(|c| c.name.as_deref() == Some("ops") || c.id == "ops");
        assert_eq!(found.unwrap().id, "ch2");

        let found = channels.iter().find(|c| c.name.as_deref() == Some("ch1") || c.id == "ch1");
        assert_eq!(found.unwrap().id, "ch1");

        let found = channels.iter().find(|c| c.name.as_deref() == Some("nope") || c.id == "nope");
        assert!(found.is_none());
    }

    #[test]
    fn ch_command_finds_channel_with_spaces() {
        let channels = vec![ChannelDisplay {
            id: "ch1".into(),
            name: Some("team sync".into()),
            node_count: 2,
            members: Vec::new(),
            unread: 0,
        }];

        let input = "/ch team sync";
        let rest = input.strip_prefix("/ch ").map(str::trim).filter(|s| !s.is_empty());
        assert_eq!(rest, Some("team sync"));

        let found = channels.iter().find(|c| c.name.as_deref() == rest || c.id == rest.unwrap());
        assert!(found.is_some(), "should find channel with space in name");
    }

    #[tokio::test]
    async fn ch_command_unknown_channel_shows_error() {
        let mut app = make_app();
        app.channels = vec![ChannelDisplay {
            id: "ch1".into(),
            name: Some("main".into()),
            node_count: 2,
            members: Vec::new(),
            unread: 0,
        }];

        app.handle_command("/ch nonexistent").await;

        let last_line = app.channel_view.last_system_content();
        assert!(
            last_line.map_or(false, |s| s.contains("找不到频道")),
            "should show error for unknown channel"
        );
    }

    #[tokio::test]
    async fn ch_command_no_arg_shows_usage() {
        let mut app = make_app();

        app.handle_command("/ch").await;

        let last_line = app.channel_view.last_system_content();
        assert!(
            last_line.map_or(false, |s| s.contains("/ch")),
            "should show usage hint"
        );
    }

    #[test]
    fn completions_include_ch_command() {
        let mut app = make_app();
        app.update_completions();
        assert!(
            app.input.completions.iter().any(|c| c == "/ch"),
            "completions should include /ch"
        );
    }

    #[test]
    fn completions_include_scene_command() {
        let mut app = make_app();
        app.update_completions();
        assert!(
            app.input.completions.iter().any(|c| c == "/scene"),
            "completions should include /scene"
        );
    }

    #[test]
    fn agent_message_start_clears_previous_streaming() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");

        app.dm_view.start_streaming_message("alice");
        let text = serde_json::json!({ "content": { "text": "old content" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &text);

        app.dm_view.start_streaming_message("alice");
        let msg = app.dm_view.streaming_messages.get("alice").unwrap();
        assert!(msg.blocks.is_empty(), "new streaming message should start with empty blocks");
    }

    #[test]
    fn multi_agent_streaming_isolated() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        app.dm_view.start_streaming_message("alice");
        app.dm_view.start_streaming_message("bob");

        let text_a = serde_json::json!({ "content": { "text": "from alice" } });
        let text_b = serde_json::json!({ "content": { "text": "from bob" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &text_a);
        app.dm_view.apply_streaming_event("bob", "agent_message_chunk", &text_b);

        app.flush_streaming_as_dm("n1", "alice");

        assert_eq!(app.dm_view.dm_history.len(), 1);
        assert!(app.dm_view.dm_history[0].content.contains("from alice"));
        assert!(app.dm_view.streaming_messages.contains_key("bob"));
        assert!(!app.dm_view.streaming_messages.contains_key("alice"));
    }

    #[test]
    fn paste_text_fallback_inserts_directly() {
        let mut app = make_app();
        app.input.insert_str("hello world");
        assert!(
            app.input.text.contains("hello world"),
            "text should be inserted: '{}'",
            app.input.text
        );
    }

    #[test]
    fn tool_name_from_title_extracts_after_colon() {
        let title = "tool: Read";
        let extracted = title.split(':').last().map(str::trim).unwrap_or(title);
        assert_eq!(extracted, "Read");
    }

    #[test]
    fn tool_name_from_title_no_colon() {
        let title = "Read";
        let extracted = title.split(':').last().map(str::trim).unwrap_or(title);
        assert_eq!(extracted, "Read");
    }

    #[test]
    fn tool_name_from_title_mcp_format() {
        let title = "mcp: nerve: nerve_post";
        let extracted = title.split(':').last().map(str::trim).unwrap_or(title);
        assert_eq!(extracted, "nerve_post");
    }

    #[test]
    fn is_split_false_when_no_panels() {
        let app = make_app();
        assert!(!app.is_split());
    }

    #[test]
    fn is_split_true_when_panels_exist() {
        let mut app = make_app();
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Channel,
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });
        assert!(app.is_split());
    }

    #[test]
    fn focused_panel_mut_can_modify_panel() {
        let mut app = make_app();
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Channel,
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });
        app.split_focus = SplitFocus::Panel(0);

        app.focused_panel_mut().unwrap().node_buffer.push_str("hello");
        assert_eq!(app.split_panels[0].node_buffer, "hello");
    }

    #[test]
    fn split_focus_panel_replaces_old_channel_variant() {
        let focus = SplitFocus::Panel(0);
        assert_ne!(focus, SplitFocus::Dm);
        assert_eq!(focus, SplitFocus::Panel(0));
    }

    #[test]
    fn split_panel_holds_channel_target() {
        let panel = SplitPanel {
            target: SplitTarget::Channel,
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        };
        assert_eq!(panel.target, SplitTarget::Channel);
        assert!(panel.node_buffer.is_empty());
    }

    #[test]
    fn split_panel_holds_node_target() {
        let panel = SplitPanel {
            target: SplitTarget::Node {
                node_id: "n1".into(),
                node_name: "alice".into(),
            },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        };
        assert!(matches!(panel.target, SplitTarget::Node { .. }));
        if let SplitTarget::Node { node_id, node_name } = &panel.target {
            assert_eq!(node_id, "n1");
            assert_eq!(node_name, "alice");
        }
    }

    #[test]
    fn split_panel_has_independent_panel_state() {
        let mut app = make_app();
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Channel,
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node {
                node_id: "n1".into(),
                node_name: "alice".into(),
            },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });

        app.split_panels[0].panel_state.auto_scroll = false;
        app.split_panels[0].panel_state.scroll_offset = 42;

        assert!(app.split_panels[1].panel_state.auto_scroll);
        assert_eq!(app.split_panels[1].panel_state.scroll_offset, u16::MAX);
    }

    fn make_split_panel(target: SplitTarget) -> SplitPanel {
        SplitPanel {
            target,
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        }
    }

    #[test]
    fn panel_x_boundaries_populated_from_layout() {
        use crate::layout::AppLayout;
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n1".into(),
            node_name: "alice".into(),
        }));

        let area = Rect::new(0, 0, 120, 30);
        let layout = AppLayout::build(area, 3, true, 2);

        app.panel_x_boundaries.clear();
        for panel_area in &layout.panels {
            app.panel_x_boundaries.push(panel_area.x);
        }

        assert_eq!(app.panel_x_boundaries.len(), 2);
        assert!(app.panel_x_boundaries[0] < app.panel_x_boundaries[1]);
    }

    #[test]
    fn panel_x_boundaries_matches_layout_panels_count() {
        use crate::layout::AppLayout;
        let mut app = make_app();
        for _ in 0..3 {
            app.split_panels.push(make_split_panel(SplitTarget::Channel));
        }

        let area = Rect::new(0, 0, 120, 30);
        let layout = AppLayout::build(area, 3, true, 3);

        app.panel_x_boundaries.clear();
        for panel_area in &layout.panels {
            app.panel_x_boundaries.push(panel_area.x);
        }

        assert_eq!(app.panel_x_boundaries.len(), layout.panels.len());
        assert_eq!(app.panel_x_boundaries.len(), app.split_panels.len());
    }

    #[test]
    fn focus_highlights_only_target_panel() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n1".into(),
            node_name: "alice".into(),
        }));
        app.split_panels.push(make_split_panel(SplitTarget::Channel));

        app.split_focus = SplitFocus::Panel(1);

        for i in 0..3 {
            let focused = app.split_focus == SplitFocus::Panel(i);
            if i == 1 {
                assert!(focused, "panel 1 should be focused");
            } else {
                assert!(!focused, "panel {} should NOT be focused", i);
            }
        }
    }

    #[test]
    fn focus_dm_highlights_no_panel() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_focus = SplitFocus::Dm;

        for i in 0..2 {
            assert!(
                app.split_focus != SplitFocus::Panel(i),
                "panel {} should NOT be focused when Dm",
                i
            );
        }
    }

    #[test]
    fn mouse_panel_index_empty_boundaries() {
        let app = make_app();
        assert!(app.mouse_panel_index(50).is_none());
        assert!(app.mouse_panel_index(0).is_none());
    }

    #[test]
    fn mouse_panel_index_single_panel() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.panel_x_boundaries = vec![60];

        assert!(app.mouse_panel_index(59).is_none(), "before panel boundary");
        assert_eq!(app.mouse_panel_index(60), Some(0), "at panel boundary");
        assert_eq!(app.mouse_panel_index(100), Some(0), "inside panel");
    }

    #[test]
    fn mouse_panel_index_multi_panel() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n1".into(),
            node_name: "alice".into(),
        }));
        app.panel_x_boundaries = vec![40, 70];

        assert!(app.mouse_panel_index(39).is_none(), "before any panel");
        assert_eq!(app.mouse_panel_index(40), Some(0), "at panel 0 boundary");
        assert_eq!(app.mouse_panel_index(55), Some(0), "inside panel 0");
        assert_eq!(app.mouse_panel_index(70), Some(1), "at panel 1 boundary");
        assert_eq!(app.mouse_panel_index(100), Some(1), "inside panel 1");
    }

    #[test]
    fn node_panel_uses_own_buffer() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n1".into(),
            node_name: "alice".into(),
        }));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n2".into(),
            node_name: "bob".into(),
        }));

        app.split_panels[0].node_buffer.push_str("alice output");
        app.split_panels[1].node_buffer.push_str("bob output");

        assert_eq!(app.split_panels[0].node_buffer, "alice output");
        assert_eq!(app.split_panels[1].node_buffer, "bob output");
        assert!(matches!(
            &app.split_panels[0].target,
            SplitTarget::Node { node_name, .. } if node_name == "alice"
        ));
        assert!(matches!(
            &app.split_panels[1].target,
            SplitTarget::Node { node_name, .. } if node_name == "bob"
        ));
    }

    #[test]
    fn channel_panel_target_distinct_from_node() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n1".into(),
            node_name: "alice".into(),
        }));

        assert!(matches!(app.split_panels[0].target, SplitTarget::Channel));
        assert!(matches!(app.split_panels[1].target, SplitTarget::Node { .. }));
    }

    fn cycle_split_focus(app: &mut App<MockTransport>) {
        if app.is_split() {
            app.split_focus = match app.split_focus {
                SplitFocus::Dm => SplitFocus::Panel(0),
                SplitFocus::Panel(i) => {
                    if i + 1 < app.split_panel_count() {
                        SplitFocus::Panel(i + 1)
                    } else {
                        SplitFocus::Dm
                    }
                }
            };
        }
    }

    #[test]
    fn ctrl_w_single_panel_toggles_dm_and_panel0() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_focus = SplitFocus::Dm;

        cycle_split_focus(&mut app);
        assert_eq!(app.split_focus, SplitFocus::Panel(0));

        cycle_split_focus(&mut app);
        assert_eq!(app.split_focus, SplitFocus::Dm);

        cycle_split_focus(&mut app);
        assert_eq!(app.split_focus, SplitFocus::Panel(0));
    }

    #[test]
    fn ctrl_w_two_panels_cycles_dm_p0_p1_dm() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n1".into(),
            node_name: "alice".into(),
        }));
        app.split_focus = SplitFocus::Dm;

        cycle_split_focus(&mut app);
        assert_eq!(app.split_focus, SplitFocus::Panel(0));

        cycle_split_focus(&mut app);
        assert_eq!(app.split_focus, SplitFocus::Panel(1));

        cycle_split_focus(&mut app);
        assert_eq!(app.split_focus, SplitFocus::Dm);
    }

    #[test]
    fn ctrl_w_three_panels_cycles_all() {
        let mut app = make_app();
        for _ in 0..3 {
            app.split_panels.push(make_split_panel(SplitTarget::Channel));
        }
        app.split_focus = SplitFocus::Dm;

        let expected = [
            SplitFocus::Panel(0),
            SplitFocus::Panel(1),
            SplitFocus::Panel(2),
            SplitFocus::Dm,
        ];
        for exp in &expected {
            cycle_split_focus(&mut app);
            assert_eq!(app.split_focus, *exp);
        }
    }

    #[test]
    fn scroll_routes_to_focused_panel() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n1".into(),
            node_name: "alice".into(),
        }));
        app.split_focus = SplitFocus::Panel(1);

        if let Some(panel) = app.focused_panel_mut() {
            panel.panel_state.scroll_up(5);
        }

        assert_ne!(app.split_panels[1].panel_state.scroll_offset, u16::MAX);
        assert_eq!(app.split_panels[0].panel_state.scroll_offset, u16::MAX);
    }

    #[test]
    fn focus_fallback_after_panel_removal() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n1".into(),
            node_name: "alice".into(),
        }));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n2".into(),
            node_name: "bob".into(),
        }));
        app.split_focus = SplitFocus::Panel(2);

        app.split_panels.remove(2);

        app.clamp_split_focus();
        assert_eq!(app.split_focus, SplitFocus::Panel(1));
    }

    #[test]
    fn focus_fallback_all_panels_removed() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_focus = SplitFocus::Panel(0);

        app.split_panels.clear();
        app.clamp_split_focus();

        assert_eq!(app.split_focus, SplitFocus::Dm);
    }

    #[tokio::test]
    async fn ctrl_w_no_panels_acts_as_delete_word() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };
        app.input.insert_str("hello world");

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)).await;

        assert!(!app.input.text.contains("world"), "Ctrl+W without panels should delete word, got: {}", app.input.text);
    }

    fn make_dm_app() -> App<MockTransport> {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };
        app.active_channel = Some("ch1".into());
        app.agents = vec![AgentDisplay {
            name: "alice".into(),
            status: "idle".into(),
            activity: None,
            adapter: Some("claude".into()),
            model: None,
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: None,
            tool_call_started: None,
            waiting_for: None,
        }];
        app
    }

    #[tokio::test]
    async fn enter_dm_sets_model_label() {
        let mut app = make_app();
        app.agents = vec![AgentDisplay {
            name: "alice".into(),
            status: "idle".into(),
            activity: None,
            adapter: Some("claude".into()),
            model: Some("opus[1m]".into()),
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: Some((50_000.0, 200_000.0, 1.5)),
            tool_call_name: None,
            tool_call_started: None,
            waiting_for: None,
        }];
        app.enter_dm("alice").await;
        assert_eq!(app.dm_view.model_label.as_deref(), Some("opus[1m] / 200.0k"));
    }

    #[tokio::test]
    async fn enter_dm_no_model_sets_none() {
        let mut app = make_app();
        app.agents = vec![AgentDisplay {
            name: "bob".into(),
            status: "idle".into(),
            activity: None,
            adapter: None,
            model: None,
            node_id: "n2".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: None,
            tool_call_started: None,
            waiting_for: None,
        }];
        app.enter_dm("bob").await;
        assert!(app.dm_view.model_label.is_none());
    }

    #[tokio::test]
    async fn split_no_arg_no_panels_adds_channel_panel() {
        let mut app = make_dm_app();
        assert!(app.split_panels.is_empty());

        app.handle_command("/split").await;

        assert_eq!(app.split_panels.len(), 1);
        assert_eq!(app.split_panels[0].target, SplitTarget::Channel);
    }

    #[tokio::test]
    async fn split_no_arg_with_panels_clears_all() {
        let mut app = make_dm_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        assert_eq!(app.split_panels.len(), 2);

        app.handle_command("/split").await;

        assert!(app.split_panels.is_empty());
        assert_eq!(app.split_focus, SplitFocus::Dm);
    }

    #[test]
    fn split_at_agent_adds_node_panel() {
        let mut app = make_dm_app();

        let new_panel = SplitPanel {
            target: SplitTarget::Node { node_id: "n1".into(), node_name: "alice".into() },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        };
        app.split_panels.push(new_panel);
        app.split_focus = SplitFocus::Dm;

        assert_eq!(app.split_panels.len(), 1);
        assert!(matches!(
            &app.split_panels[0].target,
            SplitTarget::Node { node_name, .. } if node_name == "alice"
        ));
        assert_eq!(app.split_focus, SplitFocus::Dm);
    }

    #[tokio::test]
    async fn split_at_agent_dedup_focuses_existing() {
        let mut app = make_dm_app();
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node { node_id: "n1".into(), node_name: "alice".into() },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_focus = SplitFocus::Dm;

        app.handle_command("/split @alice").await;

        assert_eq!(app.split_panels.len(), 2, "duplicate @alice should not add panel");
        assert_eq!(app.split_focus, SplitFocus::Panel(0));
    }

    #[tokio::test]
    async fn split_close_removes_focused_panel() {
        let mut app = make_dm_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n1".into(),
            node_name: "alice".into(),
        }));
        app.split_focus = SplitFocus::Panel(1);

        app.handle_command("/split close").await;

        assert_eq!(app.split_panels.len(), 1, "focused panel should be removed");
        assert_eq!(app.split_focus, SplitFocus::Panel(0));
    }

    #[tokio::test]
    async fn close_all_split_panels_unsubscribes_and_clears() {
        let mut app = make_dm_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n2".into(),
            node_name: "bob".into(),
        }));
        app.split_focus = SplitFocus::Panel(1);

        app.close_all_split_panels().await;

        assert!(app.split_panels.is_empty());
        assert_eq!(app.split_focus, SplitFocus::Dm);
    }

    #[tokio::test]
    async fn close_split_panel_removes_single() {
        let mut app = make_dm_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n2".into(),
            node_name: "bob".into(),
        }));
        app.split_focus = SplitFocus::Panel(1);

        app.close_split_panel(1).await;

        assert_eq!(app.split_panels.len(), 1);
        assert_eq!(app.split_panels[0].target, SplitTarget::Channel);
    }

    #[tokio::test]
    async fn split_close_all_clears_everything() {
        let mut app = make_dm_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_focus = SplitFocus::Panel(2);

        app.handle_command("/split close all").await;

        assert!(app.split_panels.is_empty());
        assert_eq!(app.split_focus, SplitFocus::Dm);
    }

    #[tokio::test]
    async fn exit_dm_preserves_split_panels() {
        let mut app = make_dm_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n2".into(),
            node_name: "bob".into(),
        }));
        assert_eq!(app.split_panels.len(), 2);

        app.exit_dm().await;

        assert_eq!(app.split_panels.len(), 2, "split panels should persist after exit_dm");
    }

    #[tokio::test]
    async fn split_renders_in_channel_mode() {
        let mut app = make_app();
        app.view_mode = ViewMode::Channel { channel_id: "ch1".into() };
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n1".into(),
            node_name: "alice".into(),
        }));

        assert!(!app.is_dm_mode());
        assert_eq!(app.split_panel_count(), 1);
    }

    #[tokio::test]
    async fn switch_dm_preserves_split_panels() {
        let mut app = make_dm_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.agents.push(AgentDisplay {
            name: "bob".into(),
            status: "idle".into(),
            activity: None,
            adapter: None,
            model: None,
            node_id: "n2".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: None,
            tool_call_started: None,
            waiting_for: None,
        });
        assert_eq!(app.split_panels.len(), 1);

        app.enter_dm("bob").await;

        assert_eq!(app.split_panels.len(), 1, "split panels should persist after DM switch");
    }

    #[tokio::test]
    async fn split_panel_limit_rejects_fifth() {
        let mut app = make_dm_app();
        for _ in 0..4 {
            app.split_panels.push(make_split_panel(SplitTarget::Channel));
        }
        assert_eq!(app.split_panels.len(), 4);

        app.handle_command("/split #extra").await;

        assert_eq!(app.split_panels.len(), 4, "should not exceed 4 panels");
    }

    #[tokio::test]
    async fn split_hash_channel_adds_channel_panel() {
        let mut app = make_dm_app();
        app.channels = vec![ChannelDisplay {
            id: "ch2".into(),
            name: Some("ops".into()),
            node_count: 1,
            members: Vec::new(),
            unread: 0,
        }];

        app.handle_command("/split #ops").await;

        assert_eq!(app.split_panels.len(), 1, "/split #ops should add a panel");
    }

    fn node_update_chunk(text: &str) -> serde_json::Value {
        serde_json::json!({
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "text": text }
            }
        })
    }

    fn node_update_end() -> serde_json::Value {
        serde_json::json!({
            "update": {
                "sessionUpdate": "agent_message_end"
            }
        })
    }

    #[test]
    fn node_log_routes_to_single_matching_panel() {
        let mut app = make_app();
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node { node_id: "n1".into(), node_name: "alice".into() },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });

        let detail = node_update_chunk("hello from alice");
        app.handle_node_update("n1", "alice", &detail);

        assert!(app.split_panels[0].node_buffer.contains("hello from alice"));
        assert!(app.split_panels[0].node_buffer.starts_with("assistant"));
    }

    #[test]
    fn node_log_routes_to_both_panels_subscribing_same_node() {
        let mut app = make_app();
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node { node_id: "n1".into(), node_name: "alice".into() },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node { node_id: "n1".into(), node_name: "alice".into() },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });

        let detail = node_update_chunk("broadcast");
        app.handle_node_update("n1", "alice", &detail);

        assert!(app.split_panels[0].node_buffer.contains("broadcast"));
        assert!(app.split_panels[1].node_buffer.contains("broadcast"));
    }

    #[test]
    fn node_log_isolates_different_nodes() {
        let mut app = make_app();
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node { node_id: "n1".into(), node_name: "alice".into() },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node { node_id: "n2".into(), node_name: "bob".into() },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });

        let detail_a = node_update_chunk("alice output");
        app.handle_node_update("n1", "alice", &detail_a);

        let detail_b = node_update_chunk("bob output");
        app.handle_node_update("n2", "bob", &detail_b);

        assert!(app.split_panels[0].node_buffer.contains("alice output"));
        assert!(app.split_panels[1].node_buffer.contains("bob output"));
    }

    #[test]
    fn node_log_no_matching_panel_no_panic() {
        let mut app = make_app();
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node { node_id: "n1".into(), node_name: "alice".into() },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });

        let detail = node_update_chunk("orphan log");
        app.handle_node_update("n2", "bob", &detail);

        assert!(app.split_panels[0].node_buffer.is_empty());
    }

    #[test]
    fn node_log_preserves_auto_scroll() {
        let mut app = make_app();
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node { node_id: "n1".into(), node_name: "alice".into() },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });
        assert!(app.split_panels[0].panel_state.auto_scroll);

        let detail = node_update_chunk("some log");
        app.handle_node_update("n1", "alice", &detail);

        assert!(app.split_panels[0].panel_state.auto_scroll);
    }

    #[test]
    fn update_completions_includes_channel_names() {
        let mut app = make_app();
        app.channels = vec![
            ChannelDisplay {
                id: "ch1".into(),
                name: Some("main".into()),
                node_count: 2,
                members: Vec::new(),
                unread: 0,
            },
            ChannelDisplay {
                id: "ch2".into(),
                name: Some("ops".into()),
                node_count: 1,
                members: Vec::new(),
                unread: 0,
            },
        ];

        app.update_completions();

        assert!(
            app.input.completions.iter().any(|c| c == "#main"),
            "completions should include #main, got: {:?}",
            app.input.completions
        );
        assert!(
            app.input.completions.iter().any(|c| c == "#ops"),
            "completions should include #ops"
        );
    }

    #[tokio::test]
    async fn ctrl_e_toggles_summary_mode() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        assert!(!app.dm_view.summary_mode, "summary_mode should default to false");

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)).await;
        assert!(app.dm_view.summary_mode, "Ctrl+E should enable summary_mode");

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)).await;
        assert!(!app.dm_view.summary_mode, "Ctrl+E again should disable summary_mode");
    }

    #[test]
    fn summary_mode_does_not_affect_streaming() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.dm_view.summary_mode = true;

        app.dm_view.start_streaming_message("alice");
        let update = serde_json::json!({ "content": { "text": "streaming text" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &update);

        assert!(
            app.dm_view.streaming_messages.contains_key("alice"),
            "streaming should work in summary mode"
        );
    }

    #[tokio::test]
    async fn send_message_pushes_to_input_history() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        app.input.insert_str("hello world");
        let text = app.input.take();
        app.handle_input(&text).await;

        assert_eq!(app.input.history_len(), 1);
    }

    #[test]
    fn up_arrow_single_line_triggers_history() {
        let mut app = make_app();
        app.input.history_push("previous message");

        assert!(!app.input.is_multiline());
        assert!(app.input.history_up());
        assert_eq!(app.input.text, "previous message");
    }

    #[test]
    fn up_arrow_multiline_does_not_trigger_history() {
        let mut app = make_app();
        app.input.history_push("old");
        app.input.insert_str("line1\nline2");

        assert!(app.input.is_multiline());
        assert!(app.input.move_up());
        assert_eq!(app.input.text, "line1\nline2");
    }

    #[test]
    fn down_arrow_after_history_up_restores() {
        let mut app = make_app();
        app.input.history_push("msg1");
        app.input.history_push("msg2");

        app.input.history_up();
        app.input.history_up();
        assert_eq!(app.input.text, "msg1");

        app.input.history_down();
        assert_eq!(app.input.text, "msg2");

        app.input.history_down();
        assert_eq!(app.input.text, "");
    }

    #[test]
    fn split_node_panel_render_includes_role_and_timestamp() {
        use ratatui::buffer::Buffer;

        let mut app = make_app();
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node {
                node_id: "n1".into(),
                node_name: "alice".into(),
            },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });

        let chunk = node_update_chunk("Hello from assistant");
        app.handle_node_update("n1", "alice", &chunk);
        let end_detail = serde_json::json!({
            "update": { "sessionUpdate": "agent_message_end" }
        });
        app.handle_node_update("n1", "alice", &end_detail);

        let area = Rect::new(0, 0, 60, 20);
        let mut buf = Buffer::empty(area);
        let panel = &mut app.split_panels[0];
        channel_view::render_text_panel(
            &format!("@{}", "alice"),
            &panel.node_buffer,
            &mut panel.panel_state,
            true,
            area,
            &mut buf,
        );

        let rendered: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_default())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        let inner_lines: Vec<&str> = rendered.lines().skip(1).collect();
        let inner_text = inner_lines.join("\n");

        let has_timestamp = regex::Regex::new(r"\d{2}:\d{2}:\d{2}")
            .unwrap()
            .is_match(&inner_text);
        assert!(
            has_timestamp,
            "split panel should show timestamp like DM view, but inner content:\n{}",
            inner_text
        );
    }

    #[test]
    fn node_log_routes_to_split_panel_node_buffer() {
        let mut app = make_app();

        app.active_channel = Some("ch1".into());
        app.view_mode = ViewMode::Channel { channel_id: "ch1".into() };
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node {
                node_id: "n1".into(),
                node_name: "context-guardian".into(),
            },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });

        let detail = serde_json::json!({
            "update": {
                "sessionUpdate": "node_log",
                "entries": [
                    {
                        "level": "info",
                        "message": "context window compacted",
                        "ts": "2026-04-02T10:30:45.123Z"
                    }
                ]
            }
        });

        app.handle_node_update("n1", "context-guardian", &detail);

        assert!(
            !app.split_panels[0].node_buffer.is_empty(),
            "Bug 5a: node_log event should populate split panel node_buffer, \
             but it was empty. node_log is only routed to DM view, not split panels."
        );
        assert!(
            app.split_panels[0].node_buffer.contains("context window compacted"),
            "Split panel node_buffer should contain the log message"
        );
    }

    #[tokio::test]
    async fn enter_on_section_header_toggles_collapse() {
        let mut app = make_app();
        app.channels = vec![ChannelDisplay {
            id: "ch1".into(),
            name: Some("main".into()),
            node_count: 2,
            members: Vec::new(),
            unread: 0,
        }];
        app.agents = vec![
            AgentDisplay {
                name: "ai-1".into(), status: "idle".into(), activity: None,
                adapter: Some("claude".into()), model: None, node_id: "n1".into(),
                transport: "stdio".into(), capabilities: vec![],
                usage: None, tool_call_name: None, tool_call_started: None, waiting_for: None,
            },
            AgentDisplay {
                name: "ai-2".into(), status: "idle".into(), activity: None,
                adapter: Some("claude".into()), model: None, node_id: "n2".into(),
                transport: "stdio".into(), capabilities: vec![],
                usage: None, tool_call_name: None, tool_call_started: None, waiting_for: None,
            },
        ];

        assert_eq!(app.status_bar.nav_count(&app.channels, &app.agents), 4);
        assert!(!app.status_bar.collapsed_sections.contains("AI Agents"));

        app.status_bar.selected_nav = 1;

        app.confirm_selected_navigation().await;
        assert!(app.status_bar.collapsed_sections.contains("AI Agents"),
                "Enter on SectionHeader should collapse the section");

        assert_eq!(app.status_bar.nav_count(&app.channels, &app.agents), 2);

        app.confirm_selected_navigation().await;
        assert!(!app.status_bar.collapsed_sections.contains("AI Agents"),
                "Enter again should expand the section");
        assert_eq!(app.status_bar.nav_count(&app.channels, &app.agents), 4);
    }

    // ── Render tests (Phase 3) ──────────────────────────────────

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        let mut text = String::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                let cell = &buf[(x, y)];
                text.push_str(cell.symbol());
            }
        }
        text
    }

    fn buffer_text_compact(buf: &ratatui::buffer::Buffer) -> String {
        buffer_text(buf).split_whitespace().collect::<Vec<_>>().join("")
    }

    #[test]
    fn render_empty_channel_shows_messages_border() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();

        terminal.draw(|f| app.render(f)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Messages"), "buffer should contain 'Messages' title");
    }

    #[test]
    fn render_channel_with_messages() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        app.channel_view.push_system("hello world test msg");

        terminal.draw(|f| app.render(f)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("hello world test msg"), "buffer should contain pushed message");
    }

    #[test]
    fn render_dm_view_shows_agent_name() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };
        app.dm_view = DmView::new("alice");

        terminal.draw(|f| app.render(f)).unwrap();

        let text = buffer_text_compact(terminal.backend().buffer());
        assert!(text.contains("与alice的对话"), "buffer should contain agent name in DM title");
    }

    #[test]
    fn render_dm_responding_shows_indicator() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };
        app.dm_view = DmView::new("alice");
        app.dm_view.is_responding = true;

        terminal.draw(|f| app.render(f)).unwrap();

        let text = buffer_text_compact(terminal.backend().buffer());
        assert!(text.contains("回复中..."), "buffer should show responding indicator");
    }

    #[test]
    fn render_dm_ready_shows_ready() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };
        app.dm_view = DmView::new("alice");
        app.dm_view.is_responding = false;

        terminal.draw(|f| app.render(f)).unwrap();

        let text = buffer_text_compact(terminal.backend().buffer());
        assert!(text.contains("就绪"), "buffer should show ready indicator");
    }

    #[test]
    fn render_sidebar_hidden_uses_full_width() {
        let backend = TestBackend::new(80, 24);
        let mut terminal_visible = Terminal::new(backend).unwrap();
        let mut app_visible = make_app();
        app_visible.sidebar_visible = true;
        terminal_visible.draw(|f| app_visible.render(f)).unwrap();

        let backend2 = TestBackend::new(80, 24);
        let mut terminal_hidden = Terminal::new(backend2).unwrap();
        let mut app_hidden = make_app();
        app_hidden.sidebar_visible = false;
        terminal_hidden.draw(|f| app_hidden.render(f)).unwrap();

        let buf_visible = terminal_visible.backend().buffer();
        let buf_hidden = terminal_hidden.backend().buffer();

        let find_messages_x = |buf: &ratatui::buffer::Buffer| -> Option<u16> {
            let area = buf.area;
            for y in area.y..area.y + area.height {
                let mut row = String::new();
                for x in area.x..area.x + area.width {
                    row.push_str(buf[(x, y)].symbol());
                }
                if let Some(pos) = row.find("Messages") {
                    return Some(pos as u16);
                }
            }
            None
        };

        let x_visible = find_messages_x(buf_visible).expect("Messages title with sidebar");
        let x_hidden = find_messages_x(buf_hidden).expect("Messages title without sidebar");
        assert!(x_hidden < x_visible, "Messages should start further left when sidebar is hidden (hidden={}, visible={})", x_hidden, x_visible);
    }

    #[test]
    fn render_input_area_exists() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = make_app();

        terminal.draw(|f| app.render(f)).unwrap();

        let buf = terminal.backend().buffer();
        let area = buf.area;
        let bottom_y = area.y + area.height - 1;
        let mut bottom_row = String::new();
        for x in area.x..area.x + area.width {
            bottom_row.push_str(buf[(x, bottom_y)].symbol());
        }
        assert!(
            bottom_row.contains('─') || bottom_row.contains('└') || bottom_row.contains('┘'),
            "bottom row should contain input area border characters, got: {}",
            bottom_row
        );
    }
}

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use futures_util::StreamExt;
use nerve_tui_core::NerveClient;
use nerve_tui_protocol::*;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::Frame;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::components::messages::ChannelPanelState;
use crate::components::*;
use crate::layout::AppLayout;

/// Check if `content` contains an @mention of `name` as a whole token.
/// Mirrors server-side `parseMentions` in router.ts: @name must be preceded by
/// whitespace (or start of string) and followed by whitespace, punctuation, or EOF.
fn mentions_name(content: &str, name: &str) -> bool {
    let padded = format!(" {}", content);
    let pattern = format!("@{}", name);
    for (i, _) in padded.match_indices(&pattern) {
        // Check preceding char is whitespace
        let before = padded.as_bytes().get(i.wrapping_sub(1)).copied().unwrap_or(b' ');
        if !before.is_ascii_whitespace() {
            continue;
        }
        // Check following char is whitespace, punctuation, or EOF
        let after_pos = i + pattern.len();
        match padded.as_bytes().get(after_pos) {
            None => return true, // EOF
            Some(c) if c.is_ascii_whitespace() => return true,
            Some(c) if b".,;:!?".contains(c) => return true,
            _ => continue, // e.g. @alice-dev when looking for @alice
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SplitFocus {
    Dm,
    Channel,
}

#[derive(Debug, Clone, PartialEq)]
enum SplitTarget {
    Channel,
    Node { node_id: String, node_name: String },
}

pub struct App {
    pub client: NerveClient,
    event_rx: mpsc::UnboundedReceiver<NerveEvent>,

    // UI components
    messages: MessagesView,
    status_bar: StatusBar,
    input: InputBox,

    // Data
    channels: Vec<ChannelDisplay>,
    agents: Vec<AgentDisplay>,
    active_channel: Option<String>,
    should_quit: bool,

    // DM mode
    active_dm: Option<DmState>,
    project_path: Option<String>,
    project_name: Option<String>,
    /// When false (default), only show channels/agents for project_path
    global_mode: bool,
    /// Sidebar visibility toggle (Ctrl+B)
    sidebar_visible: bool,
    /// Channel for background task errors (e.g. prompt failures)
    error_tx: mpsc::UnboundedSender<String>,
    error_rx: mpsc::UnboundedReceiver<String>,
    /// Cached archived channels from last /restore call
    archived_channels: Vec<Value>,

    /// Agents whose streaming buffer was already flushed by idle (to avoid double-persist)
    flushed_agents: HashSet<String>,

    // Split view
    split_view: bool,
    split_focus: SplitFocus,
    split_target: SplitTarget,
    /// Streaming text buffer for split node panel
    split_node_buffer: String,
    channel_panel_state: ChannelPanelState,
    /// Cached x-coordinate where the channel panel starts (for mouse hit-testing in split view).
    last_channel_panel_x: Option<u16>,
    /// Dirty flag — skip redraw if nothing changed since last frame.
    needs_redraw: bool,
}

impl App {
    pub fn new(client: NerveClient, event_rx: mpsc::UnboundedReceiver<NerveEvent>) -> Self {
        Self::new_with_project(client, event_rx, None)
    }

    pub fn new_with_project(
        client: NerveClient,
        event_rx: mpsc::UnboundedReceiver<NerveEvent>,
        project_path: Option<String>,
    ) -> Self {
        let (error_tx, error_rx) = mpsc::unbounded_channel();
        // Canonicalize project_path to normalize trailing slashes, `.`, `..`
        let project_path = project_path.map(|p| {
            std::fs::canonicalize(&p)
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or(p)
        });
        let project_name = project_path
            .as_deref()
            .and_then(Self::project_name_from_path);
        Self {
            client,
            event_rx,
            messages: MessagesView::new(),
            status_bar: StatusBar::new(),
            input: InputBox::new(),
            channels: Vec::new(),
            agents: Vec::new(),
            active_channel: None,
            should_quit: false,
            active_dm: None,
            project_path,
            project_name,
            global_mode: false,
            sidebar_visible: true,
            error_tx,
            error_rx,
            archived_channels: Vec::new(),
            flushed_agents: HashSet::new(),
            split_view: false,
            split_focus: SplitFocus::Dm,
            split_target: SplitTarget::Channel,
            split_node_buffer: String::new(),
            channel_panel_state: ChannelPanelState::new(),
            last_channel_panel_x: None,
            needs_redraw: true,
        }
    }

    /// Returns the cwd filter for API calls: project_path in project mode, None in global mode.
    fn cwd_filter(&self) -> Option<&str> {
        if self.global_mode {
            None
        } else {
            self.project_path.as_deref()
        }
    }

    fn project_name_from_path(path: &str) -> Option<String> {
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(|name| name.to_string())
            .or_else(|| {
                let trimmed = path.trim_end_matches('/');
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
    }

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
                        Event::Paste(text) => self.input.insert_str(&text),
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
                    self.messages.push_system(&err_msg);
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

    fn render(&mut self, frame: &mut Frame) {
        // Advance blink tick for streaming cursor
        self.messages.tick_blink();

        let area = frame.area();
        let in_split = self.split_view && self.active_dm.is_some();
        let input_inner_w = AppLayout::input_inner_width(area, self.sidebar_visible, in_split);
        let input_lines = self.input.visual_line_count(input_inner_w) + 2;
        let layout = AppLayout::build(area, input_lines, self.sidebar_visible, in_split);

        // Sidebar: channels + agents (skip when hidden)
        if self.sidebar_visible {
            self.status_bar.render(
                &self.channels,
                self.active_channel.as_deref(),
                &self.agents,
                self.active_dm.as_ref().map(|dm| dm.node_name.as_str()),
                self.project_name.as_deref(),
                self.global_mode,
                layout.sidebar,
                frame.buffer_mut(),
            );
        }

        // Messages (DM panel in split mode)
        self.messages.render(layout.messages, frame.buffer_mut());

        // Right panel (split view): channel or node output
        self.last_channel_panel_x = layout.channel_panel.map(|r| r.x);
        if let Some(panel_area) = layout.channel_panel {
            let focused = self.split_focus == SplitFocus::Channel;
            match &self.split_target {
                SplitTarget::Channel => {
                    let channel_name = self
                        .channels
                        .iter()
                        .find(|c| Some(&c.id) == self.active_channel.as_ref())
                        .map(|c| c.display_name())
                        .unwrap_or("channel");
                    self.messages.render_channel_panel(
                        channel_name,
                        &mut self.channel_panel_state,
                        focused,
                        panel_area,
                        frame.buffer_mut(),
                    );
                }
                SplitTarget::Node { node_name, .. } => {
                    self.messages.render_text_panel(
                        &format!("@{}", node_name),
                        &self.split_node_buffer,
                        &mut self.channel_panel_state,
                        focused,
                        panel_area,
                        frame.buffer_mut(),
                    );
                }
            }
        }

        // Input
        self.input.render(layout.input, frame.buffer_mut());
        self.input.render_popup(layout.input, frame.buffer_mut());

        // DM status indicator on input box border
        if let Some(ref dm) = self.active_dm {
            let status = if dm.is_responding {
                Span::styled(" 回复中... ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else {
                Span::styled(" 就绪 ", Style::default().fg(Color::Green))
            };
            let x = layout.input.right().saturating_sub(status.width() as u16 + 2);
            let y = layout.input.y;
            frame.buffer_mut().set_span(x, y, &status, status.width() as u16);
        }

        // Cursor
        let (cx, cy) = self.input.cursor_position(layout.input);
        frame.set_cursor_position((cx, cy));
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            // Ctrl+C: cancel active DM response if agent is responding
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.active_dm.as_ref().is_some_and(|dm| dm.is_responding) {
                    self.cancel_active_dm().await;
                }
            }

            // Quit
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }

            // Shift+Enter, Alt+Enter: insert newline
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.insert('\n');
            }

            // Ctrl+O: insert newline (universal fallback, no protocol needed)
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.insert('\n');
            }

            // Submit or confirm sidebar selection
            KeyCode::Enter => {
                if self.input.is_popup_visible() {
                    // Confirm popup selection (same as Tab)
                    self.input.tab();
                } else if !self.input.is_empty() {
                    let text = self.input.take();
                    self.handle_input(&text).await;
                } else {
                    self.confirm_selected_navigation().await;
                }
            }

            // Tab completion or confirm sidebar selection
            KeyCode::Tab => {
                if self.input.is_empty() {
                    self.confirm_selected_navigation().await;
                } else {
                    self.input.tab();
                }
            }
            KeyCode::BackTab => self.input.shift_tab(),

            // Split view: Ctrl+S toggle, Ctrl+W switch focus
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.active_dm.is_some() {
                    let has_target = self.active_channel.is_some()
                        || matches!(self.split_target, SplitTarget::Node { .. });
                    if has_target {
                        self.split_view = !self.split_view;
                        if self.split_view {
                            self.split_focus = SplitFocus::Dm;
                            self.channel_panel_state.snap_to_bottom();
                        } else {
                            self.split_focus = SplitFocus::Dm;
                        }
                    } else {
                        self.messages.push_system("需要先加入频道才能分屏");
                    }
                }
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.split_view && self.active_dm.is_some() {
                    self.split_focus = match self.split_focus {
                        SplitFocus::Dm => SplitFocus::Channel,
                        SplitFocus::Channel => SplitFocus::Dm,
                    };
                } else {
                    self.input.delete_word();
                }
            }

            // Up/Down: navigate within multi-line input, or scroll messages
            KeyCode::Up if key.modifiers.is_empty() => {
                if self.input.is_multiline() && self.input.move_up() {
                    // Moved cursor up within input
                } else if self.split_view && self.split_focus == SplitFocus::Channel {
                    self.channel_panel_state.scroll_up(1);
                } else {
                    self.messages.scroll_up(1);
                }
            }
            KeyCode::Down if key.modifiers.is_empty() => {
                if self.input.is_multiline() && self.input.move_down() {
                    // Moved cursor down within input
                } else if self.split_view && self.split_focus == SplitFocus::Channel {
                    self.channel_panel_state.scroll_down(1);
                } else {
                    self.messages.scroll_down(1);
                }
            }

            // Scroll messages (dispatched to focused panel in split mode)
            KeyCode::PageUp => {
                if self.split_view && self.split_focus == SplitFocus::Channel {
                    self.channel_panel_state.page_up();
                } else {
                    self.messages.page_up();
                }
            }
            KeyCode::PageDown => {
                if self.split_view && self.split_focus == SplitFocus::Channel {
                    self.channel_panel_state.page_down();
                } else {
                    self.messages.page_down();
                }
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.split_view && self.split_focus == SplitFocus::Channel {
                    self.channel_panel_state.scroll_down(1);
                } else {
                    self.messages.scroll_down(1);
                }
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.split_view && self.split_focus == SplitFocus::Channel {
                    self.channel_panel_state.scroll_down(10);
                } else {
                    self.messages.scroll_down(10);
                }
            }

            // Emacs keybindings (line-aware for multiline input)
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.move_line_start();
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.move_line_end();
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.kill_to_line_end();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.kill_to_line_start();
            }

            // Toggle sidebar
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.sidebar_visible = !self.sidebar_visible;
            }

            // Ctrl+G: toggle global/project mode
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.global_mode = !self.global_mode;
                let mode = if self.global_mode { "全局" } else { "项目" };
                self.messages.push_system(&format!("切换到{}模式", mode));
                self.refresh_agents().await;
                self.refresh_channels().await;
                // Validate active channel still visible
                if let Some(ref ch_id) = self.active_channel {
                    if !self.channels.iter().any(|c| c.id == *ch_id) {
                        self.active_channel = None;
                        if let Some(ch) = self.channels.first() {
                            let ch_id = ch.id.clone();
                            self.join_channel(&ch_id).await;
                        }
                    }
                }
                // Validate active DM agent still visible
                if let Some(ref dm) = self.active_dm {
                    if !self.agents.iter().any(|a| a.name == dm.node_name) {
                        self.exit_dm().await;
                    }
                }
            }

            // Ctrl+N/P: navigate popup candidates when visible, otherwise sidebar
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.input.is_popup_visible() {
                    self.input.select_next();
                } else {
                    self.status_bar
                        .select_next_item(&self.channels, &self.agents);
                }
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.input.is_popup_visible() {
                    self.input.select_prev();
                } else {
                    self.status_bar
                        .select_prev_item(&self.channels, &self.agents);
                }
            }

            // Esc: dismiss popup → switch split focus to DM → cancel/exit DM
            KeyCode::Esc => {
                if self.input.is_popup_visible() {
                    self.input.dismiss_popup();
                } else if self.split_view && self.split_focus == SplitFocus::Channel {
                    self.split_focus = SplitFocus::Dm;
                } else if self.active_dm.is_some() {
                    if self.active_dm.as_ref().is_some_and(|dm| dm.is_responding) {
                        self.cancel_active_dm().await;
                    } else {
                        self.exit_dm().await;
                    }
                }
            }

            // Shift+Left/Right: cycle agent filter tabs in channel mode
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if !self.messages.is_dm_mode() {
                    self.cycle_agent_filter(false);
                }
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if !self.messages.is_dm_mode() {
                    self.cycle_agent_filter(true);
                }
            }

            // Text editing
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                self.input.delete_word();
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            KeyCode::Char(c) => self.input.insert(c),
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self.split_view && self.is_mouse_in_channel_panel(mouse.column) {
                    self.channel_panel_state.scroll_up(3);
                } else {
                    self.messages.scroll_up(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.split_view && self.is_mouse_in_channel_panel(mouse.column) {
                    self.channel_panel_state.scroll_down(3);
                } else {
                    self.messages.scroll_down(3);
                }
            }
            _ => {}
        }
    }

    /// Check if mouse column falls within the channel panel (right half of split view).
    fn is_mouse_in_channel_panel(&self, column: u16) -> bool {
        self.last_channel_panel_x
            .is_some_and(|panel_x| column >= panel_x)
    }

    /// Cycle through agent filter tabs: None → agent1 → agent2 → ... → None
    fn cycle_agent_filter(&mut self, forward: bool) {
        let agent_names: Vec<String> = self.agents.iter().map(|a| a.name.clone()).collect();
        if agent_names.is_empty() {
            return;
        }
        let current_idx = self.messages.filter.as_ref()
            .and_then(|f| agent_names.iter().position(|n| n == f));

        let new_filter = if forward {
            match current_idx {
                None => Some(agent_names[0].clone()),
                Some(i) if i + 1 < agent_names.len() => Some(agent_names[i + 1].clone()),
                Some(_) => None, // wrap to "All"
            }
        } else {
            match current_idx {
                None => Some(agent_names[agent_names.len() - 1].clone()),
                Some(0) => None, // wrap to "All"
                Some(i) => Some(agent_names[i - 1].clone()),
            }
        };

        let label = new_filter.as_deref().unwrap_or("All");
        debug!(filter = label, "agent filter tab switched");
        self.messages.filter = new_filter;
        self.messages.snap_to_bottom();
    }

    fn sync_navigation_selection(&mut self) {
        self.messages.filter = None;
        self.status_bar.sync_to_context(
            &self.channels,
            self.active_channel.as_deref(),
            &self.agents,
            self.active_dm.as_ref().map(|dm| dm.node_name.as_str()),
        );
        self.messages.snap_to_bottom();
    }

    async fn confirm_selected_navigation(&mut self) {
        match self
            .status_bar
            .selected_target(&self.channels, &self.agents)
        {
            Some(NavigationTarget::Channel(idx)) => {
                if let Some(ch) = self.channels.get(idx) {
                    let ch_id = ch.id.clone();
                    if self.active_dm.is_some() {
                        self.exit_dm().await;
                    }
                    if self.active_channel.as_deref() != Some(&ch_id) {
                        self.join_channel(&ch_id).await;
                    } else {
                        self.sync_navigation_selection();
                    }
                }
            }
            Some(NavigationTarget::Agent(idx)) => {
                if let Some(agent) = self.agents.get(idx) {
                    let agent_name = agent.name.clone();
                    self.enter_dm(&agent_name).await;
                }
            }
            None => {}
        }
    }

    async fn handle_input(&mut self, text: &str) {
        if text.starts_with('/') {
            self.handle_command(text).await;
            return;
        }

        // DM mode: send via node.prompt
        if let Some(ref dm) = self.active_dm.clone() {
            if dm.is_responding {
                debug!(
                    "dm send blocked for {}: agent still responding",
                    dm.node_name
                );
                self.push_contextual_system("agent 正在回复，先等待完成或 Ctrl+C 取消");
                return;
            }

            debug!(
                "dm send to {}: {}",
                dm.node_name,
                &text[..text.len().min(50)]
            );

            // Flush any remaining streaming content before adding user message,
            // so user message appears after the agent's reply, not before streaming tail.
            let node_id_ref = dm.node_id.clone();
            let node_name_ref = dm.node_name.clone();
            self.flush_streaming_as_dm(&node_id_ref, &node_name_ref);

            // Add user message locally immediately
            let user_msg = DmMessage {
                role: "user".to_string(),
                content: text.to_string(),
                timestamp: chrono::Local::now().timestamp(),
            };
            self.messages.push_dm(&user_msg);
            if let Some(ref mut dm_state) = self.active_dm {
                dm_state.messages.push(user_msg);
            }

            // Send prompt in background — response comes via node.update
            let node_id = dm.node_id.clone();
            let content = text.to_string();
            let client_ws_tx = self.client.ws_tx_clone();
            let pending = self.client.pending_clone();
            let error_tx = self.error_tx.clone();
            tokio::spawn(async move {
                let client = NerveClient::from_parts(client_ws_tx, pending);
                if let Err(e) = client.node_prompt(&node_id, &content).await {
                    warn!("node.prompt failed: {}", e);
                    let _ = error_tx.send(format!("发送失败: {}", e));
                }
            });
            return;
        }

        // Channel mode: post to active channel
        if let Some(ref ch_id) = self.active_channel.clone() {
            match self.client.channel_post(ch_id, text).await {
                Ok(_) => {} // Message arrives via channel.message notification
                Err(e) => self.messages.push_system(&format!("发送失败: {}", e)),
            }
        } else {
            self.messages
                .push_system("未加入频道，用 /join 或 /create 先创建");
        }
    }

    // --- DM mode ---

    fn push_contextual_system(&mut self, content: &str) {
        if self.active_dm.is_some() {
            let dm_msg = DmMessage {
                role: "系统".to_string(),
                content: content.to_string(),
                timestamp: chrono::Local::now().timestamp(),
            };
            self.messages.push_dm(&dm_msg);
            if let Some(ref mut dm_state) = self.active_dm {
                dm_state.messages.push(dm_msg);
            }
        } else {
            self.messages.push_system(content);
        }
    }

    fn reset_dm_before_enter(&mut self, next_node_id: &str) -> Option<String> {
        let old_dm = self.active_dm.as_ref()?.clone();
        let same_node = old_dm.node_id == next_node_id;
        if same_node {
            debug!(
                "re-entering DM with {}, resetting local DM state",
                old_dm.node_name
            );
        } else {
            debug!("switching DM: unsubscribe old node {}", old_dm.node_id);
        }

        self.messages.exit_dm();
        self.active_dm = None;
        Some(old_dm.node_id)
    }

    async fn enter_dm(&mut self, agent_name: &str) {
        let agent = self.agents.iter().find(|a| a.name == agent_name);
        let Some(agent) = agent else {
            self.messages
                .push_system(&format!("找不到 agent: {}", agent_name));
            return;
        };
        let node_id = agent.node_id.clone();
        let node_name = agent.name.clone();

        // Reset local DM state and resubscribe, even when re-entering the same agent.
        if let Some(old_node_id) = self.reset_dm_before_enter(&node_id) {
            if let Err(e) = self.client.node_unsubscribe(&old_node_id).await {
                warn!("unsubscribe old DM failed: {}", e);
            }
        }

        debug!("entering DM with {} ({})", node_name, node_id);

        // Subscribe to node updates
        if let Err(e) = self.client.node_subscribe(&node_id).await {
            self.messages.push_system(&format!("subscribe 失败: {}", e));
            return;
        }

        self.active_dm = Some(DmState {
            node_id,
            node_name: node_name.clone(),
            messages: Vec::new(),
            streaming: None,
            is_responding: false,
        });
        self.messages.enter_dm(&node_name);

        // Initialize usage display from agent snapshot
        if let Some(agent) = self.agents.iter().find(|a| a.name == node_name) {
            if let Some((used, size, cost)) = agent.usage {
                self.messages.update_usage(used, size, cost);
            }
        }

        self.sync_navigation_selection();
    }

    async fn exit_dm(&mut self) {
        if let Some(dm) = self.active_dm.take() {
            debug!("exiting DM with {}", dm.node_name);
            if let Err(e) = self.client.node_unsubscribe(&dm.node_id).await {
                warn!("unsubscribe failed: {}", e);
            }
            // Clean up split node subscription
            if let SplitTarget::Node { ref node_id, .. } = self.split_target {
                let id = node_id.clone();
                let _ = self.client.node_unsubscribe(&id).await;
                self.split_target = SplitTarget::Channel;
            }
            self.messages.exit_dm();
            self.split_view = false;
            self.split_focus = SplitFocus::Dm;
            self.sync_navigation_selection();
        }
    }

    async fn cancel_active_dm(&mut self) {
        let Some((node_id, node_name)) = self
            .active_dm
            .as_ref()
            .map(|dm| (dm.node_id.clone(), dm.node_name.clone()))
        else {
            return;
        };

        debug!("cancelling active DM with {}", node_name);
        if let Err(e) = self.client.node_cancel(&node_id).await {
            self.push_contextual_system(&format!("取消失败: {}", e));
            return;
        }

        self.flush_streaming_as_dm(&node_id, &node_name);
        if let Some(ref mut dm_state) = self.active_dm {
            dm_state.is_responding = false;
        }
        self.push_contextual_system(&format!("已中断 {}", node_name));
    }

    async fn handle_command(&mut self, text: &str) {
        let parts: Vec<&str> = text.splitn(3, ' ').collect();
        let cmd = parts[0];

        match cmd {
            "/dm" => {
                if let Some(name) = parts.get(1) {
                    self.enter_dm(name).await;
                } else {
                    self.messages.push_system("用法: /dm <agent_name>");
                }
            }

            "/back" => {
                self.exit_dm().await;
            }

            "/clear" => {
                if let Some(ref dm) = self.active_dm {
                    let node_name = dm.node_name.clone();
                    match self.client.session_clear(&node_name).await {
                        Ok(_) => {
                            self.messages.clear_dm();
                            if let Some(ref mut dm_state) = self.active_dm {
                                dm_state.messages.clear();
                                dm_state.is_responding = false;
                            }
                            self.push_contextual_system(&format!(
                                "/clear — {} session 已清除",
                                node_name
                            ));
                        }
                        Err(e) => {
                            self.push_contextual_system(&format!("/clear — 清除失败: {}", e))
                        }
                    }
                } else {
                    self.push_contextual_system("/clear 仅在 DM 模式下可用");
                }
            }

            "/compact" => {
                if let Some(ref dm) = self.active_dm {
                    let node_name = dm.node_name.clone();
                    self.push_contextual_system(&format!(
                        "/compact — 正在压缩 {} 上下文...",
                        node_name
                    ));
                    match self.client.session_compact(&node_name).await {
                        Ok(_) => {
                            self.push_contextual_system(&format!(
                                "{} 上下文已压缩",
                                node_name
                            ));
                        }
                        Err(e) => {
                            self.push_contextual_system(&format!("压缩失败: {}", e))
                        }
                    }
                } else {
                    self.push_contextual_system("/compact 仅在 DM 模式下可用");
                }
            }

            "/create" => {
                let name = parts.get(1).copied();
                match self
                    .client
                    .channel_create(name, self.project_path.as_deref())
                    .await
                {
                    Ok(ch) => {
                        self.messages.push_system(&format!("频道已创建: {}", ch.id));
                        let ch_id = ch.id.clone();
                        self.refresh_channels().await;
                        self.join_channel(&ch_id).await;
                    }
                    Err(e) => self.messages.push_system(&format!("创建失败: {}", e)),
                }
            }

            "/join" => {
                if let Some(ch_id) = parts.get(1) {
                    self.join_channel(ch_id).await;
                } else {
                    let channels = self.client.channel_list(self.cwd_filter()).await.unwrap_or_default();
                    if let Some(ch) = channels.first() {
                        let ch_id = ch.id.clone();
                        self.join_channel(&ch_id).await;
                    } else {
                        self.messages.push_system("没有可用频道，用 /create 创建");
                    }
                }
            }

            "/add" => {
                if parts.len() < 2 {
                    self.messages.push_system("用法: /add <adapter> [name]");
                    return;
                }
                let adapter = parts[1];
                let name = parts.get(2).copied();
                match self
                    .client
                    .node_spawn(adapter, name, self.project_path.as_deref())
                    .await
                {
                    Ok(node) => {
                        self.messages
                            .push_system(&format!("已启动: {} ({})", node.name, node.node_id));
                        if let Some(ref ch_id) = self.active_channel.clone() {
                            if let Err(e) = self
                                .client
                                .channel_add_node(ch_id, &node.node_id, Some(&node.name))
                                .await
                            {
                                self.messages.push_system(&format!("加入频道失败: {}", e));
                            }
                        }
                        self.refresh_agents().await;
                    }
                    Err(e) => self.messages.push_system(&format!("启动失败: {}", e)),
                }
            }

            "/remove" | "/stop" => {
                if let Some(name_or_id) = parts.get(1) {
                    if let Some(agent) = self
                        .agents
                        .iter()
                        .find(|a| a.name == *name_or_id || a.node_id == *name_or_id)
                    {
                        let nid = agent.node_id.clone();
                        match self.client.node_stop(&nid).await {
                            Ok(_) => {
                                self.messages
                                    .push_system(&format!("已停止: {}", name_or_id));
                                self.refresh_agents().await;
                            }
                            Err(e) => self.messages.push_system(&format!("停止失败: {}", e)),
                        }
                    } else {
                        self.messages
                            .push_system(&format!("找不到: {}", name_or_id));
                    }
                }
            }

            "/cancel" => {
                if let Some(name_or_id) = parts.get(1) {
                    if let Some(agent) = self
                        .agents
                        .iter()
                        .find(|a| a.name == *name_or_id || a.node_id == *name_or_id)
                    {
                        let nid = agent.node_id.clone();
                        match self.client.node_cancel(&nid).await {
                            Ok(_) => self
                                .messages
                                .push_system(&format!("已取消: {}", name_or_id)),
                            Err(e) => self.messages.push_system(&format!("取消失败: {}", e)),
                        }
                    }
                }
            }

            "/list" => {
                self.refresh_agents().await;
                if self.agents.is_empty() {
                    self.messages.push_system("没有 agent");
                } else {
                    for a in &self.agents {
                        self.messages.push_system(&format!(
                            "  {} [{}] {}",
                            a.name,
                            a.status,
                            a.adapter.as_deref().unwrap_or("")
                        ));
                    }
                }
            }

            "/channels" => {
                self.refresh_channels().await;
                if self.channels.is_empty() {
                    self.messages.push_system("没有频道");
                } else {
                    for ch in &self.channels {
                        let active = if self.active_channel.as_deref() == Some(&ch.id) {
                            " ← 当前"
                        } else {
                            ""
                        };
                        self.messages.push_system(&format!(
                            "  {} ({} 节点){}",
                            ch.display_name(),
                            ch.node_count,
                            active
                        ));
                    }
                }
            }

            "/restore" => {
                if let Some(arg) = parts.get(1) {
                    // /restore <number> or /restore <channelId>
                    let channel_id = if let Ok(idx) = arg.parse::<usize>() {
                        if idx == 0 {
                            None
                        } else {
                            self.archived_channels
                                .get(idx - 1)
                                .and_then(|ch| ch.get("id").and_then(|v| v.as_str()))
                                .map(String::from)
                        }
                    } else {
                        Some(arg.to_string())
                    };
                    if let Some(ch_id) = channel_id {
                        match self.client.channel_restore(&ch_id).await {
                            Ok(ch) => {
                                let restored_id = ch.id.clone();
                                let name = ch.name.as_deref().unwrap_or(&restored_id);
                                self.messages
                                    .push_system(&format!("频道已恢复: {}", name));
                                self.archived_channels.clear();
                                self.refresh_channels().await;
                                self.join_channel(&restored_id).await;
                            }
                            Err(e) => self.messages.push_system(&format!("恢复失败: {}", e)),
                        }
                    } else {
                        self.messages.push_system("无效序号，先用 /restore 查看列表");
                    }
                } else {
                    // /restore with no args: list archived channels
                    match self.client.channel_list_archived(self.cwd_filter()).await {
                        Ok(channels) => {
                            if channels.is_empty() {
                                self.archived_channels.clear();
                                self.messages.push_system("没有归档频道");
                            } else {
                                self.messages.push_system("归档频道:");
                                for (i, ch) in channels.iter().enumerate() {
                                    let id = ch
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?");
                                    let name = ch
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let display = if name.is_empty() {
                                        id.to_string()
                                    } else {
                                        format!("{} ({})", name, id)
                                    };
                                    // Show agents info
                                    let agents_str = if let Some(agents) =
                                        ch.get("agents").and_then(|v| v.as_array())
                                    {
                                        let names: Vec<&str> = agents
                                            .iter()
                                            .filter_map(|a| {
                                                a.get("name").and_then(|v| v.as_str())
                                            })
                                            .collect();
                                        if names.is_empty() {
                                            String::new()
                                        } else {
                                            format!(" [{}]", names.join(", "))
                                        }
                                    } else {
                                        String::new()
                                    };
                                    self.messages.push_system(&format!(
                                        "  {}. {}{}",
                                        i + 1,
                                        display,
                                        agents_str
                                    ));
                                }
                                self.messages
                                    .push_system("用 /restore <序号> 恢复频道");
                                self.archived_channels = channels;
                            }
                        }
                        Err(e) => {
                            self.archived_channels.clear();
                            self.messages
                                .push_system(&format!("获取归档列表失败: {}", e));
                        }
                    }
                }
            }

            "/help" => {
                self.messages.push_system("命令:");
                self.messages
                    .push_system("  /create [name]        创建频道");
                self.messages
                    .push_system("  /join [id]            加入频道");
                self.messages
                    .push_system("  /channels             列出频道");
                self.messages
                    .push_system("  /add <adapter> [name] 启动 agent");
                self.messages
                    .push_system("  /stop <name>          停止 agent");
                self.messages
                    .push_system("  /cancel <name>        取消 agent 任务");
                self.messages
                    .push_system("  /list                 列出 agents");
                self.messages
                    .push_system("  /restore [n]          恢复归档频道");
                self.messages
                    .push_system("  /clear                清除 DM session");
                self.messages
                    .push_system("  /compact              压缩 DM 上下文");
                self.messages
                    .push_system("  /split [@agent]       分屏(频道或agent输出)");
                self.messages
                    .push_system("  /dm <name>            与 agent 1v1 对话");
                self.messages
                    .push_system("  /back                 退出 DM 回频道");
                self.messages.push_system("  /help                 帮助");
                self.messages.push_system("快捷键:");
                self.messages.push_system("  Enter       发送消息 / 确认选择");
                self.messages.push_system("  Tab         补全 / 确认选择");
                self.messages.push_system("  Ctrl+O      输入框换行");
                self.messages.push_system("  Ctrl+C      中断当前 DM 回复");
                self.messages.push_system("  Esc         DM回复中=取消，否则退出DM");
                self.messages.push_system("  Ctrl+N/P    侧边栏导航 下/上");
                self.messages.push_system("  Ctrl+J/K    滚动消息 下/上（1行）");
                self.messages.push_system("  Ctrl+D/U    滚动消息 下/上（10行）");
                self.messages.push_system("  PgDn/PgUp   翻页（20行）");
                self.messages.push_system("  Ctrl+G      切换全局/项目模式");
                self.messages.push_system("  Ctrl+Q      退出");
            }

            "/split" => {
                let arg = parts.get(1).copied().unwrap_or("");
                if arg.starts_with('@') {
                    // /split @agent-name: show agent output in right panel
                    let agent_name = &arg[1..];
                    if let Some(agent) = self.agents.iter().find(|a| a.name == agent_name) {
                        let node_id = agent.node_id.clone();
                        let node_name = agent.name.clone();
                        // Unsubscribe from previous split node if any
                        if let SplitTarget::Node { node_id: old_id, .. } = &self.split_target {
                            let old_id = old_id.clone();
                            let _ = self.client.node_unsubscribe(&old_id).await;
                        }
                        // Subscribe to new target node
                        if let Err(e) = self.client.node_subscribe(&node_id).await {
                            self.push_contextual_system(&format!("subscribe 失败: {}", e));
                        } else {
                            self.split_target = SplitTarget::Node { node_id, node_name: node_name.clone() };
                            self.split_node_buffer.clear();
                            self.split_view = true;
                            self.split_focus = SplitFocus::Dm;
                            self.channel_panel_state.snap_to_bottom();
                            self.push_contextual_system(&format!("分屏查看 @{}", node_name));
                        }
                    } else {
                        self.push_contextual_system(&format!("找不到 agent: {}", agent_name));
                    }
                } else if self.active_dm.is_some() {
                    if self.split_view && arg.is_empty() {
                        // Toggle off
                        // Unsubscribe from split node if any
                        if let SplitTarget::Node { node_id: old_id, .. } = &self.split_target {
                            let old_id = old_id.clone();
                            let _ = self.client.node_unsubscribe(&old_id).await;
                        }
                        self.split_view = false;
                        self.split_target = SplitTarget::Channel;
                        self.split_focus = SplitFocus::Dm;
                    } else if self.active_channel.is_some() || !arg.is_empty() {
                        // Unsubscribe from previous split node if switching to channel
                        if let SplitTarget::Node { node_id: old_id, .. } = &self.split_target {
                            let old_id = old_id.clone();
                            let _ = self.client.node_unsubscribe(&old_id).await;
                        }
                        self.split_target = SplitTarget::Channel;
                        self.split_view = !self.split_view;
                        if self.split_view {
                            self.split_focus = SplitFocus::Dm;
                            self.channel_panel_state.snap_to_bottom();
                        } else {
                            self.split_focus = SplitFocus::Dm;
                        }
                    } else {
                        self.push_contextual_system("需要先加入频道才能分屏");
                    }
                } else if arg.is_empty() {
                    self.push_contextual_system("用法: /split [@agent] 或在 DM 中 /split");
                }
            }

            "/quit" | "/q" => {
                self.should_quit = true;
            }

            _ => {
                self.messages.push_system(&format!("未知命令: {}", cmd));
            }
        }
    }

    async fn handle_nerve_event(&mut self, event: NerveEvent) {
        debug!("nerve event: {}", event.kind());

        match event {
            NerveEvent::ChannelMessage {
                channel_id,
                message,
            } => {
                // Only show messages that @mention me or that I sent.
                // Token-level match: @name must be preceded by whitespace and followed
                // by whitespace, punctuation, or EOF (mirrors server router.ts:parseMentions).
                let dominated = message.from == self.client.node_name
                    || mentions_name(&message.content, &self.client.node_name);
                if dominated {
                    let is_active = self.active_channel.as_deref() == Some(&channel_id);
                    if is_active {
                        let is_agent = self.agents.iter().any(|a| a.name == message.from);
                        self.messages.push(&message, is_agent);
                    } else {
                        // Cache message for non-active channel + bump unread
                        self.messages.push_to_channel(&channel_id, &message);
                        // Update sidebar unread badge
                        if let Some(ch) = self.channels.iter_mut().find(|c| c.id == channel_id) {
                            ch.unread = self.messages.unread_count(&channel_id);
                        }
                    }
                }
            }

            NerveEvent::ChannelMention { channel_id, message } => {
                // Dedup: active channel mentions already shown via ChannelMessage handler.
                // Non-active channel mentions always show (even in DM mode).
                let is_active = self.active_channel.as_deref() == Some(&channel_id);
                if !is_active {
                    let is_agent = self.agents.iter().any(|a| a.name == message.from);
                    self.messages.push(&message, is_agent);
                }
            }

            NerveEvent::NodeJoined { node_name, .. } => {
                if self.active_dm.is_none() {
                    self.messages
                        .push_system(&format!("{} 加入频道", node_name));
                }
                self.refresh_agents().await;
                self.refresh_channels().await;
            }

            NerveEvent::NodeLeft { node_name, .. } => {
                if self.active_dm.is_none() {
                    self.messages
                        .push_system(&format!("{} 离开频道", node_name));
                }
                self.refresh_agents().await;
                self.refresh_channels().await;
            }

            NerveEvent::NodeUpdate {
                node_id,
                name,
                detail,
            } => {
                self.handle_node_update(&node_id, &name, &detail);
            }

            NerveEvent::NodeStatusChanged {
                node_id,
                name,
                status,
                activity,
            } => {
                let found = self.agents.iter_mut().find(|a| a.node_id == node_id || a.name == name);
                if let Some(agent) = found {
                    debug!("NodeStatusChanged: {} status={} activity={:?}", name, status, activity);
                    agent.status = status.clone();
                    agent.activity = activity;
                    // Clear tool call display when agent goes idle
                    if status == "idle" {
                        agent.tool_call_name = None;
                        agent.tool_call_started = None;
                        agent.waiting_for = None;
                    }
                } else {
                    debug!("NodeStatusChanged: {} not found in agents list (len={})", name, self.agents.len());
                }
                // When agent goes idle, flush any pending streaming buffer as DM message
                if status == "idle" {
                    self.flush_streaming_as_dm(&node_id, &name);
                } else if let Some(ref mut dm_state) = self.active_dm {
                    if dm_state.node_id == node_id && status == "busy" {
                        dm_state.is_responding = true;
                    }
                }
            }

            NerveEvent::ChannelCreated {
                channel_id, name, ..
            } => {
                // Refresh first so we can check if this channel is in our filtered view
                self.refresh_channels().await;
                if self.channels.iter().any(|c| c.id == channel_id) {
                    let label = name.as_deref().unwrap_or("unnamed");
                    self.messages
                        .push_system(&format!("频道 {} 已创建", label));
                }
            }

            NerveEvent::ChannelClosed {
                channel_id, name, ..
            } => {
                // Check if the closed channel was visible before refresh
                let was_visible = self.channels.iter().any(|c| c.id == channel_id);
                let was_active =
                    self.active_channel.as_deref() == Some(channel_id.as_str());

                self.refresh_channels().await;

                if was_visible {
                    let label = name.as_deref().unwrap_or("unnamed");
                    self.messages
                        .push_system(&format!("频道 {} 已关闭", label));
                }

                // If the closed channel was active, fall back
                if was_active {
                    self.active_channel = None;
                    if let Some(ch) = self.channels.first() {
                        let ch_id = ch.id.clone();
                        self.join_channel(&ch_id).await;
                    }
                    self.sync_navigation_selection();
                }
            }

            NerveEvent::NodeRegistered {
                ref name,
                ..
            } => {
                info!("NodeRegistered: name={}", name);
                // Show in both channel and DM views
                self.messages
                    .push_system(&format!("{} 已注册", name));
                if self.active_dm.is_some() {
                    self.messages.push_dm_system(&format!("{} 已注册", name));
                }
                self.refresh_agents().await;
                info!(
                    "NodeRegistered: after refresh, agents count={}, names=[{}]",
                    self.agents.len(),
                    self.agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
                );
            }

            NerveEvent::NodeStopped { node_id, name } => {
                // If we're in a DM with this node, flush streaming and exit DM
                self.flush_streaming_as_dm(&node_id, &name);
                if self
                    .active_dm
                    .as_ref()
                    .map_or(false, |dm| dm.node_id == node_id)
                {
                    self.active_dm = None;
                    self.messages.exit_dm();
                    self.messages
                        .push_system(&format!("{} 已停止", name));
                } else if self.agents.iter().any(|a| a.node_id == node_id) {
                    self.messages
                        .push_system(&format!("{} 已停止", name));
                }
                // Clean up split panel if targeting the stopped node
                if matches!(&self.split_target, SplitTarget::Node { node_id: sid, .. } if sid == &node_id) {
                    let _ = self.client.node_unsubscribe(&node_id).await;
                    self.split_target = SplitTarget::Channel;
                    self.split_node_buffer.clear();
                    self.split_view = false;
                }
                // Remove from agents list immediately
                self.agents.retain(|a| a.node_id != node_id);
                self.update_completions();
                self.sync_navigation_selection();
            }

            NerveEvent::Disconnected => {
                self.messages.push_system("⚠ 与 nerve 断开连接");
            }
        }
    }

    /// Flush streaming buffer as a DM message when agent goes idle (no explicit end event).
    fn flush_streaming_as_dm(&mut self, node_id: &str, name: &str) {
        let in_dm = self
            .active_dm
            .as_ref()
            .map_or(false, |dm| dm.node_id == node_id);
        if !in_dm {
            return;
        }

        // Extract content and clear streaming BEFORE pushing to dm_lines
        // to prevent any render frame from seeing both.
        let content = self.messages.streaming.iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c.clone())
            .unwrap_or_default();
        self.messages.streaming.retain(|(n, _)| n != name);
        self.messages.take_streaming_message(name);

        if !content.is_empty() {
            debug!(
                "flush_streaming_as_dm: {} went idle, persisting {} chars",
                name,
                content.len()
            );
            let dm_msg = DmMessage {
                role: "assistant".to_string(),
                content,
                timestamp: chrono::Local::now().timestamp(),
            };
            self.messages.push_dm(&dm_msg);
            if let Some(ref mut dm_state) = self.active_dm {
                dm_state.messages.push(dm_msg);
            }
        }
        self.flushed_agents.insert(name.to_string());
        if let Some(ref mut dm_state) = self.active_dm {
            if dm_state.node_id == node_id {
                dm_state.is_responding = false;
            }
        }
    }

    fn handle_node_update(&mut self, node_id: &str, name: &str, detail: &serde_json::Value) {
        let is_dm_active = self.active_dm.is_some();
        let in_dm = self
            .active_dm
            .as_ref()
            .map_or(false, |dm| dm.node_id == node_id);

        // Route updates to split node buffer if target matches
        let is_split_node = matches!(&self.split_target, SplitTarget::Node { node_id: sid, .. } if sid == node_id);
        if is_split_node {
            if let Some(update) = detail.get("update") {
                let kind = update.get("sessionUpdate").and_then(|v| v.as_str());
                if kind == Some("agent_message_chunk") {
                    if let Some(text) = update.get("content").and_then(|c| c.get("text")).and_then(|v| v.as_str()) {
                        self.split_node_buffer.push_str(text);
                    }
                } else if kind == Some("agent_message_end") || kind == Some("stop_reason") {
                    self.split_node_buffer.push('\n');
                }
            }
        }

        // In DM mode, ignore streaming from non-active nodes
        if is_dm_active && !in_dm {
            debug!(
                "node.update from {} ignored (DM active for different node)",
                name
            );
            return;
        }

        if let Some(update) = detail.get("update") {
            let kind = update.get("sessionUpdate").and_then(|v| v.as_str());
            debug!(
                "node.update from {}: kind={:?} in_dm={} raw={}",
                name,
                kind,
                in_dm,
                serde_json::to_string(update).unwrap_or_default()
            );

            match kind {
                Some("agent_message_chunk") => {
                    // New pipeline: structured ContentBlock
                    self.messages.apply_streaming_event(name, "agent_message_chunk", update);

                    let text = update
                        .get("content")
                        .and_then(|c| c.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if text.is_empty() {
                        debug!(
                            "agent_message_chunk from {} has empty text, raw: {:?}",
                            name, update
                        );
                    }
                    let buf_len_before = self.messages.streaming
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, c)| c.len())
                        .unwrap_or(0);
                    let chunk_preview: String = text.chars().take(20).collect();
                    info!(
                        "CHUNK from={} len={} preview={:?} buf_before={}",
                        name, text.len(), chunk_preview, buf_len_before
                    );
                    if let Some(entry) = self.messages.streaming.iter_mut().find(|(n, _)| n == name)
                    {
                        // Dedup: skip if buffer already ends with this exact chunk
                        if !text.is_empty() && !entry.1.ends_with(text) {
                            entry.1.push_str(text);
                        } else if text.is_empty() {
                            // empty chunk, skip
                        } else {
                            debug!("DEDUP: skipping duplicate chunk from {}: {:?}", name, &text[..text.len().min(20)]);
                        }
                    } else {
                        self.messages
                            .streaming
                            .push((name.to_string(), text.to_string()));
                    }
                    // Note: do NOT set is_responding here — chunks arrive during
                    // replay too (historical data). Only NodeStatusChanged("busy")
                    // should set is_responding = true.
                }
                Some("agent_message_start") => {
                    // New pipeline: start structured message
                    self.messages.start_streaming_message(name);

                    let old_buf_len = self.messages.streaming
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, c)| c.len());
                    info!(
                        "MSG_START from={} old_buf={:?}",
                        name, old_buf_len
                    );
                    debug!(
                        "agent_message_start from {}, clearing streaming buffer",
                        name
                    );
                    self.messages.streaming.retain(|(n, _)| n != name);
                    self.messages
                        .streaming
                        .push((name.to_string(), String::new()));
                    self.flushed_agents.remove(name);
                }
                Some("agent_message_end") => {
                    // New pipeline: finalize structured message
                    self.messages.apply_streaming_event(name, "agent_message_end", update);
                    self.messages.take_streaming_message(name);

                    // If idle already flushed this agent's streaming buffer, skip to
                    // avoid persisting the same message twice.
                    let already_flushed = self.flushed_agents.remove(name);

                    // Extract and clear streaming content BEFORE pushing to dm_lines
                    // to prevent duplicate rendering.
                    let streaming_content = self
                        .messages
                        .streaming
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, c)| c.clone())
                        .unwrap_or_default();
                    self.messages.streaming.retain(|(n, _)| n != name);

                    // Some agents include full content in the end event
                    let end_content = update
                        .get("content")
                        .and_then(|c| c.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    let streaming_len = streaming_content.len();
                    let end_len = end_content.len();

                    let final_content = if !streaming_content.is_empty() {
                        streaming_content
                    } else if !already_flushed && !end_content.is_empty() {
                        end_content.to_string()
                    } else {
                        String::new()
                    };

                    debug!(
                        "agent_message_end from {}: in_dm={} streaming_buf={} end_content={} final={} already_flushed={}",
                        name, in_dm, streaming_len, end_len, final_content.len(), already_flushed
                    );

                    if in_dm && !final_content.is_empty() {
                        debug!(
                            "persisting DM message from {}: {} chars",
                            name,
                            final_content.len()
                        );
                        let dm_msg = DmMessage {
                            role: "assistant".to_string(),
                            content: final_content,
                            timestamp: chrono::Local::now().timestamp(),
                        };
                        self.messages.push_dm(&dm_msg);
                        if let Some(ref mut dm_state) = self.active_dm {
                            dm_state.messages.push(dm_msg);
                        }
                    } else if in_dm {
                        debug!("agent_message_end from {} but no content to persist (already_flushed={})", name, already_flushed);
                    }
                    if let Some(ref mut dm_state) = self.active_dm {
                        if dm_state.node_id == node_id {
                            dm_state.is_responding = false;
                        }
                    }
                }
                Some("agent_thought_chunk") => {
                    // New pipeline: structured thinking block
                    self.messages.apply_streaming_event(name, "agent_thought_chunk", update);

                    let text = update
                        .get("content")
                        .and_then(|c| c.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !text.is_empty() {
                        // Old pipeline: append [thinking] marker to streaming buffer
                        if let Some(entry) = self.messages.streaming.iter_mut().find(|(n, _)| n == name) {
                            // Only add [thinking] prefix once at the start of a thinking block
                            if !entry.1.ends_with("[thinking] ") && !entry.1.ends_with(text) {
                                if entry.1.is_empty() || entry.1.ends_with('\n') {
                                    entry.1.push_str("[thinking] ");
                                }
                                entry.1.push_str(text);
                            }
                        } else {
                            self.messages.streaming.push((name.to_string(), format!("[thinking] {}", text)));
                        }
                    }
                }
                Some("agent_thought_end") => {
                    // New pipeline: mark thinking block finished
                    self.messages.apply_streaming_event(name, "agent_thought_end", update);

                    // Old pipeline: add newline after thinking block
                    if let Some(entry) = self.messages.streaming.iter_mut().find(|(n, _)| n == name) {
                        if !entry.1.is_empty() && !entry.1.ends_with('\n') {
                            entry.1.push('\n');
                        }
                    }
                }
                Some("user_message") => {
                    // Replay of user's prompt (from subscribe buffer)
                    if in_dm {
                        // Flush any pending agent streaming as a complete message
                        // (replay has no idle/end signals between turns)
                        self.flush_streaming_as_dm(node_id, name);

                        let text = update
                            .get("content")
                            .and_then(|c| c.get("text"))
                            .or_else(|| update.get("content").and_then(|c| c.as_str().map(|_| c)))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !text.is_empty() {
                            let dm_msg = DmMessage {
                                role: "user".to_string(),
                                content: text.to_string(),
                                timestamp: chrono::Local::now().timestamp(),
                            };
                            self.messages.push_dm(&dm_msg);
                            if let Some(ref mut dm_state) = self.active_dm {
                                dm_state.messages.push(dm_msg);
                            }
                        }
                    }
                }
                Some("tool_call") => {
                    // New pipeline: structured tool_call block
                    self.messages.apply_streaming_event(name, "tool_call", update);

                    // Update sidebar tool call display
                    // Support both nested (toolCall: {...}) and flat ACP format (toolCallId, _meta, title, ...)
                    let tc = update.get("toolCall").or_else(|| update.get("tool_call"));
                    let (tool_name, desc, raw_input) = if let Some(tc) = tc {
                        // Legacy nested
                        let tn = tc.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                        let d = tc.get("description").and_then(|v| v.as_str()).unwrap_or("");
                        let ri = tc.get("input").cloned();
                        (tn.to_string(), d.to_string(), ri)
                    } else {
                        // Flat ACP format
                        let tn = update.pointer("/_meta/claudeCode/toolName")
                            .and_then(|v| v.as_str())
                            .unwrap_or_else(|| update.get("title").and_then(|v| v.as_str()).unwrap_or("unknown"));
                        let d = update.get("title").and_then(|v| v.as_str()).unwrap_or("");
                        let ri = update.get("rawInput").cloned();
                        (tn.to_string(), d.to_string(), ri)
                    };
                    let mut formatted = format!("\n[tool:{}]", tool_name);
                    if !desc.is_empty() {
                        formatted.push(' ');
                        formatted.push_str(&desc);
                    }
                    if let Some(input) = raw_input.as_ref().and_then(|v| v.as_object()) {
                        for (key, val) in input {
                            let val_str = match val.as_str() {
                                Some(s) if s.len() > 120 => format!("{}…", &s[..120]),
                                Some(s) => s.to_string(),
                                None => {
                                    let j = serde_json::to_string(val).unwrap_or_default();
                                    if j.len() > 120 { format!("{}…", &j[..120]) } else { j }
                                }
                            };
                            formatted.push_str(&format!("\n  {}: {}", key, val_str));
                        }
                    }
                    if let Some(entry) = self.messages.streaming.iter_mut().find(|(n, _)| n == name) {
                        entry.1.push_str(&formatted);
                    } else {
                        self.messages.streaming.push((name.to_string(), formatted));
                    }

                    // Update sidebar: show current tool call name + start timer
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.name == name) {
                        agent.tool_call_name = Some(tool_name.clone());
                        agent.tool_call_started = Some(std::time::Instant::now());
                        debug!(agent = name, tool = %tool_name, "sidebar: tool_call started");
                    }
                }
                Some("tool_call_update") => {
                    // New pipeline: structured tool_call_update
                    self.messages.apply_streaming_event(name, "tool_call_update", update);

                    // Support both nested and flat ACP format
                    let tcu = update.get("toolCallUpdate").or_else(|| update.get("tool_call_update"));
                    let (status, result_text) = if let Some(tcu) = tcu {
                        let s = tcu.get("status").and_then(|v| v.as_str()).unwrap_or("");
                        let rt = tcu.get("result")
                            .or_else(|| tcu.get("output"))
                            .and_then(|v| v.get("value").or(Some(v)));
                        let text = match rt {
                            Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
                            Some(v) => serde_json::to_string_pretty(v).unwrap_or_default(),
                            None => String::new(),
                        };
                        (s.to_string(), text)
                    } else {
                        // Flat ACP format
                        let s = update.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let text = update.get("content")
                            .and_then(|v| {
                                if v.is_string() { v.as_str().map(|s| s.to_string()) }
                                else if let Some(arr) = v.as_array() {
                                    let texts: Vec<String> = arr.iter().filter_map(|item| {
                                        item.get("content").and_then(|c| c.get("text")).and_then(|t| t.as_str()).map(|s| s.to_string())
                                    }).collect();
                                    if texts.is_empty() { None } else { Some(texts.join("\n")) }
                                } else { None }
                            })
                            .unwrap_or_default();
                        (s, text)
                    };
                    if status == "completed" || status == "failed" {
                        // Clear sidebar tool call display
                        if let Some(agent) = self.agents.iter_mut().find(|a| a.name == name) {
                            agent.tool_call_name = None;
                            agent.tool_call_started = None;
                        }
                        let marker = if status == "completed" { "✓" } else { "✗" };
                        let mut formatted = format!("\n[tool_result:{}]", marker);
                        let lines: Vec<&str> = result_text.lines().collect();
                        for (i, line) in lines.iter().enumerate() {
                            if i >= 3 {
                                formatted.push_str(&format!("\n  … {} 行已省略", lines.len() - 3));
                                break;
                            }
                            let display = if line.len() > 150 { &line[..150] } else { line };
                            formatted.push_str(&format!("\n  {}", display));
                        }
                        if let Some(entry) = self.messages.streaming.iter_mut().find(|(n, _)| n == name) {
                            entry.1.push_str(&formatted);
                        }
                    }
                }
                Some("usage_update") => {
                    let used = update.get("used").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let size = update.get("size").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    // claude-agent-acp sends cost as { amount: number, currency: "USD" }
                    // Fall back to plain number for compatibility
                    let cost = update.get("cost")
                        .and_then(|v| {
                            v.get("amount").and_then(|a| a.as_f64())
                                .or_else(|| v.as_f64())
                        })
                        .unwrap_or(0.0);
                    // Update agent cache for DM re-entry
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.node_id == node_id) {
                        agent.usage = Some((used, size, cost));
                    }
                    if in_dm {
                        self.messages.update_usage(used, size, cost);
                    }
                }
                _ => {
                    debug!("node.update from {} unhandled: {:?}", name, detail);
                }
            }
        } else {
            debug!(
                "node.update from {} has no 'update' field: {:?}",
                name, detail
            );
        }
    }

    async fn join_channel(&mut self, channel_id: &str) {
        // Save current channel messages to cache before switching
        if let Some(ref old_id) = self.active_channel.clone() {
            if old_id != channel_id {
                self.messages.save_channel(old_id);
            }
        }

        match self.client.channel_join(channel_id).await {
            Ok(_) => {
                self.active_channel = Some(channel_id.to_string());
                self.sync_navigation_selection();

                // Try to load from cache first
                if self.messages.load_channel(channel_id) {
                    self.messages
                        .push_system(&format!("已切换频道: {}", channel_id));
                } else {
                    // No cache — fetch from server
                    self.messages.clear();
                    self.messages
                        .push_system(&format!("已加入频道: {}", channel_id));
                    match self.client.channel_history(channel_id, Some(50)).await {
                        Ok(msgs) => {
                            for msg in &msgs {
                                let is_agent =
                                    self.agents.iter().any(|a| a.name == msg.from);
                                self.messages.push(msg, is_agent);
                            }
                        }
                        Err(e) => warn!("load history failed: {}", e),
                    }
                }

                // Refresh agents for this channel
                self.refresh_agents().await;
            }
            Err(e) => {
                self.messages.push_system(&format!("加入失败: {}", e));
            }
        }
    }

    async fn refresh_channels(&mut self) {
        match self.client.channel_list(self.cwd_filter()).await {
            Ok(list) => {
                self.channels = list
                    .into_iter()
                    .map(|ch| {
                        let members: Vec<MemberDisplay> = ch
                            .nodes
                            .values()
                            .map(|node_id| MemberDisplay {
                                node_id: node_id.clone(),
                            })
                            .collect();
                        let unread = self.messages.unread_count(&ch.id);
                        ChannelDisplay {
                            id: ch.id,
                            name: ch.name,
                            node_count: ch.nodes.len(),
                            members,
                            unread,
                        }
                    })
                    .collect();
                self.sync_navigation_selection();
            }
            Err(e) => warn!("refresh channels failed: {}", e),
        }
    }

    async fn refresh_agents(&mut self) {
        let cwd = self.cwd_filter().map(|s| s.to_string());
        debug!("refresh_agents: cwd_filter={:?}", cwd);
        match self.client.node_list(cwd.as_deref()).await {
            Ok(nodes) => {
                debug!("refresh_agents: got {} nodes from server", nodes.len());
                for n in &nodes {
                    debug!("  node: {} transport={} status={}", n.name, n.transport, n.status);
                }
                self.agents = nodes
                    .into_iter()
                    .filter(|n| n.status != "stopped")
                    .map(|n| AgentDisplay {
                        name: n.name,
                        status: n.status,
                        activity: n.activity,
                        adapter: n.adapter,
                        node_id: n.id,
                        transport: n.transport,
                        capabilities: n.capabilities,
                        usage: n.usage.map(|u| (u.token_used, u.token_size, u.cost)),
                        tool_call_name: None,
                        tool_call_started: None,
                        waiting_for: None,
                    })
                    .collect();
                self.update_completions();
                self.sync_navigation_selection();
            }
            Err(e) => warn!("refresh agents failed: {}", e),
        }
    }

    fn update_completions(&mut self) {
        let mut completions: Vec<String> = vec![
            "/create".into(),
            "/join".into(),
            "/channels".into(),
            "/add".into(),
            "/stop".into(),
            "/cancel".into(),
            "/list".into(),
            "/dm".into(),
            "/back".into(),
            "/help".into(),
            "/restore".into(),
            "/clear".into(),
            "/compact".into(),
            "/split".into(),
            "/quit".into(),
        ];
        for agent in &self.agents {
            completions.push(format!("@{}", agent.name));
            completions.push(agent.name.clone());
        }
        for a in &["claude", "c1", "c2", "codex", "gemini", "mock"] {
            completions.push(a.to_string());
        }
        self.input.completions = completions;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn make_app() -> App {
        let (ws_tx, _ws_rx) = mpsc::unbounded_channel::<String>();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let client = NerveClient::from_parts(ws_tx, pending);
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        App::new(client, event_rx)
    }

    #[test]
    fn project_name_uses_last_path_segment() {
        assert_eq!(
            App::project_name_from_path("/tmp/demo-project"),
            Some("demo-project".to_string())
        );
        assert_eq!(
            App::project_name_from_path("/tmp/demo-project/"),
            Some("demo-project".to_string())
        );
    }

    #[test]
    fn new_with_project_sets_project_context() {
        let (ws_tx, _ws_rx) = mpsc::unbounded_channel::<String>();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let client = NerveClient::from_parts(ws_tx, pending);
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        let app = App::new_with_project(client, event_rx, Some("/tmp/demo-project".into()));

        assert_eq!(app.project_path.as_deref(), Some("/tmp/demo-project"));
        assert_eq!(app.project_name.as_deref(), Some("demo-project"));
    }

    #[tokio::test]
    async fn handle_input_blocks_while_dm_is_responding() {
        let mut app = make_app();
        app.active_dm = Some(DmState {
            node_id: "n1".into(),
            node_name: "alice".into(),
            messages: Vec::new(),
            streaming: None,
            is_responding: true,
        });
        app.messages.enter_dm("alice");

        app.handle_input("hello").await;

        let dm = app.active_dm.as_ref().unwrap();
        assert_eq!(dm.messages.len(), 1);
        assert_eq!(dm.messages[0].role, "系统");
        assert!(dm.messages[0].content.contains("agent 正在回复"));
    }

    #[test]
    fn flush_streaming_as_dm_clears_responding_flag() {
        let mut app = make_app();
        app.active_dm = Some(DmState {
            node_id: "n1".into(),
            node_name: "alice".into(),
            messages: Vec::new(),
            streaming: None,
            is_responding: true,
        });
        app.messages.enter_dm("alice");
        app.messages
            .streaming
            .push(("alice".to_string(), "partial".to_string()));

        app.flush_streaming_as_dm("n1", "alice");

        let dm = app.active_dm.as_ref().unwrap();
        assert!(!dm.is_responding);
        assert_eq!(dm.messages.len(), 1);
        assert_eq!(dm.messages[0].content, "partial");
    }

    #[test]
    fn reset_dm_before_enter_reinitializes_same_agent_dm() {
        let mut app = make_app();
        app.active_dm = Some(DmState {
            node_id: "n1".into(),
            node_name: "alice".into(),
            messages: vec![DmMessage {
                role: "assistant".into(),
                content: "stale".into(),
                timestamp: 0,
            }],
            streaming: None,
            is_responding: true,
        });
        app.messages.enter_dm("alice");
        app.messages
            .streaming
            .push(("alice".to_string(), "partial".to_string()));

        let old_node_id = app.reset_dm_before_enter("n1");

        assert_eq!(old_node_id.as_deref(), Some("n1"));
        assert!(app.active_dm.is_none());
        assert!(!app.messages.is_dm_mode());
        assert!(app.messages.streaming.is_empty());
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
        app.active_dm = Some(DmState {
            node_id: "n2".into(),
            node_name: "bob".into(),
            messages: Vec::new(),
            streaming: None,
            is_responding: false,
        });

        app.sync_navigation_selection();

        assert_eq!(app.status_bar.selected_nav, 2);
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

        // Simulate ChannelClosed for active channel
        app.handle_nerve_event(NerveEvent::ChannelClosed {
            channel_id: "ch1".into(),
            name: Some("main".into()),
        })
        .await;

        // active_channel should be cleared (refresh_channels returns empty since mock client)
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

        // Closing ch2 should NOT affect active_channel
        app.handle_nerve_event(NerveEvent::ChannelClosed {
            channel_id: "ch2".into(),
            name: Some("ops".into()),
        })
        .await;

        assert_eq!(app.active_channel.as_deref(), Some("ch1"));
    }

    #[test]
    fn cwd_filter_respects_global_mode() {
        let (ws_tx, _ws_rx) = mpsc::unbounded_channel::<String>();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let client = NerveClient::from_parts(ws_tx, pending);
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        let mut app = App::new_with_project(client, event_rx, Some("/tmp/project".into()));

        // Default: project mode
        assert_eq!(app.cwd_filter(), Some("/tmp/project"));

        // Global mode
        app.global_mode = true;
        assert!(app.cwd_filter().is_none());

        // Back to project mode
        app.global_mode = false;
        assert_eq!(app.cwd_filter(), Some("/tmp/project"));
    }

    #[test]
    fn cwd_filter_none_without_project_path() {
        let app = make_app();
        assert!(app.cwd_filter().is_none());
    }

    // --- mentions_name tests ---

    #[test]
    fn mentions_name_exact_match() {
        assert!(mentions_name("hello @alice how are you", "alice"));
        assert!(mentions_name("@alice", "alice"));
        assert!(mentions_name("hey @alice.", "alice")); // trailing punctuation
        assert!(mentions_name("@alice, @bob", "alice"));
    }

    #[test]
    fn mentions_name_no_prefix_match() {
        // @alice should NOT match @alice-dev
        assert!(!mentions_name("hello @alice-dev", "alice"));
        assert!(!mentions_name("@alice_admin is here", "alice"));
        assert!(!mentions_name("@alicex", "alice"));
    }

    #[test]
    fn mentions_name_no_false_positive() {
        assert!(!mentions_name("hello alice", "alice")); // no @ prefix
        assert!(!mentions_name("email@alice.com", "alice")); // preceded by non-whitespace
        assert!(!mentions_name("", "alice"));
    }

    // --- ChannelMention in DM mode tests ---

    #[tokio::test]
    async fn dm_mode_does_not_swallow_other_channel_mention() {
        let mut app = make_app();
        // Simulate being in DM mode
        app.active_dm = Some(DmState {
            node_id: "n1".into(),
            node_name: "bob".into(),
            messages: Vec::new(),
            streaming: None,
            is_responding: false,
        });
        app.messages.enter_dm("bob");
        // Active channel is ch1
        app.active_channel = Some("ch1".into());

        let initial_count = app.messages.line_count();

        // Mention from a DIFFERENT channel (ch2) should still show
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
            app.messages.line_count(),
            initial_count + 1,
            "non-active channel mention should display even in DM mode"
        );
    }

    #[tokio::test]
    async fn active_channel_mention_deduped() {
        let mut app = make_app();
        app.active_channel = Some("ch1".into());

        let initial_count = app.messages.line_count();

        // Mention from the ACTIVE channel should be skipped (already shown via ChannelMessage)
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
            app.messages.line_count(),
            initial_count,
            "active channel mention should be deduped"
        );
    }
}

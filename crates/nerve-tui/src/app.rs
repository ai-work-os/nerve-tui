use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use futures_util::StreamExt;
use nerve_tui_core::NerveClient;
use nerve_tui_protocol::*;
use ratatui::Frame;
use serde_json::Value;
use std::path::Path;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::components::messages::ChannelPanelState;
use crate::components::*;
use crate::layout::AppLayout;

#[derive(Debug, Clone, Copy, PartialEq)]
enum SplitFocus {
    Dm,
    Channel,
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

    // Split view
    split_view: bool,
    split_focus: SplitFocus,
    channel_panel_state: ChannelPanelState,
    /// Cached x-coordinate where the channel panel starts (for mouse hit-testing in split view).
    last_channel_panel_x: Option<u16>,
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
            split_view: false,
            split_focus: SplitFocus::Dm,
            channel_panel_state: ChannelPanelState::new(),
            last_channel_panel_x: None,
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
            terminal.draw(|frame| self.render(frame))?;

            if self.should_quit {
                break;
            }

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
                }
                Some(event) = self.event_rx.recv() => {
                    self.handle_nerve_event(event).await;
                }
                Some(err_msg) = self.error_rx.recv() => {
                    self.messages.push_system(&err_msg);
                }
                _ = redraw_interval.tick() => {}
            }
        }

        Ok(())
    }

    fn render(&mut self, frame: &mut Frame) {
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

        // Channel panel (right side of split view)
        self.last_channel_panel_x = layout.channel_panel.map(|r| r.x);
        if let Some(panel_area) = layout.channel_panel {
            let channel_name = self
                .channels
                .iter()
                .find(|c| Some(&c.id) == self.active_channel.as_ref())
                .map(|c| c.display_name())
                .unwrap_or("channel");
            let focused = self.split_focus == SplitFocus::Channel;
            self.messages.render_channel_panel(
                &channel_name,
                &mut self.channel_panel_state,
                focused,
                panel_area,
                frame.buffer_mut(),
            );
        }

        // Input
        self.input.render(layout.input, frame.buffer_mut());
        self.input.render_popup(layout.input, frame.buffer_mut());

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
                if !self.input.is_empty() {
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
                    if self.active_channel.is_some() {
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

            // Text editing
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
        self.sync_navigation_selection();
    }

    async fn exit_dm(&mut self) {
        if let Some(dm) = self.active_dm.take() {
            debug!("exiting DM with {}", dm.node_name);
            if let Err(e) = self.client.node_unsubscribe(&dm.node_id).await {
                warn!("unsubscribe failed: {}", e);
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
                    .push_system("  /split                切换分屏(DM+频道)");
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
                if self.active_dm.is_some() {
                    if self.active_channel.is_some() {
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
                } else {
                    self.push_contextual_system("需要先进入 DM 才能分屏");
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
            NerveEvent::ChannelMessage { message, .. } => {
                let is_agent = self.agents.iter().any(|a| a.name == message.from);
                self.messages.push(&message, is_agent);
            }

            NerveEvent::ChannelMention { message, .. } => {
                if self.active_dm.is_none() {
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
                if let Some(agent) = self.agents.iter_mut().find(|a| a.name == name) {
                    agent.status = status.clone();
                    agent.activity = activity;
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
                ref transport,
                ..
            } => {
                let is_agent = transport.as_deref() == Some("stdio");
                if is_agent {
                    if self.active_dm.is_none() {
                        self.messages
                            .push_system(&format!("{} 已注册", name));
                    }
                    self.refresh_agents().await;
                }
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

        if let Some((_n, content)) = self.messages.streaming.iter().find(|(n, _)| n == name) {
            if !content.is_empty() {
                debug!(
                    "flush_streaming_as_dm: {} went idle, persisting {} chars",
                    name,
                    content.len()
                );
                let dm_msg = DmMessage {
                    role: "assistant".to_string(),
                    content: content.clone(),
                    timestamp: chrono::Local::now().timestamp(),
                };
                self.messages.push_dm(&dm_msg);
                if let Some(ref mut dm_state) = self.active_dm {
                    dm_state.messages.push(dm_msg);
                }
            }
        }
        self.messages.streaming.retain(|(n, _)| n != name);
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
                    if let Some(entry) = self.messages.streaming.iter_mut().find(|(n, _)| n == name)
                    {
                        entry.1.push_str(text);
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
                    debug!(
                        "agent_message_start from {}, clearing streaming buffer",
                        name
                    );
                    self.messages.streaming.retain(|(n, _)| n != name);
                    self.messages
                        .streaming
                        .push((name.to_string(), String::new()));
                }
                Some("agent_message_end") => {
                    let streaming_content = self
                        .messages
                        .streaming
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, c)| c.clone())
                        .unwrap_or_default();

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
                    } else if !end_content.is_empty() {
                        end_content.to_string()
                    } else {
                        String::new()
                    };

                    debug!(
                        "agent_message_end from {}: in_dm={} streaming_buf={} end_content={} final={}",
                        name, in_dm, streaming_len, end_len, final_content.len()
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
                        debug!("agent_message_end from {} but no content to persist", name);
                    }
                    // In channel mode: final message arrives via channel.message
                    self.messages.streaming.retain(|(n, _)| n != name);
                    if let Some(ref mut dm_state) = self.active_dm {
                        if dm_state.node_id == node_id {
                            dm_state.is_responding = false;
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
                    let tc = update.get("toolCall").or_else(|| update.get("tool_call"));
                    if let Some(tc) = tc {
                        let tool_name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                        let desc = tc.get("description").and_then(|v| v.as_str()).unwrap_or("");
                        let mut formatted = format!("\n[tool:{}]", tool_name);
                        if !desc.is_empty() {
                            formatted.push(' ');
                            formatted.push_str(desc);
                        }
                        if let Some(input) = tc.get("input").and_then(|v| v.as_object()) {
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
                    }
                }
                Some("tool_call_update") => {
                    let tcu = update.get("toolCallUpdate").or_else(|| update.get("tool_call_update"));
                    if let Some(tcu) = tcu {
                        let status = tcu.get("status").and_then(|v| v.as_str()).unwrap_or("");
                        if status == "completed" || status == "failed" {
                            let result_val = tcu.get("result")
                                .or_else(|| tcu.get("output"))
                                .and_then(|v| v.get("value").or(Some(v)));
                            let result_text = match result_val {
                                Some(v) if v.is_string() => v.as_str().unwrap_or("").to_string(),
                                Some(v) => serde_json::to_string_pretty(v).unwrap_or_default(),
                                None => String::new(),
                            };
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
        match self.client.channel_join(channel_id).await {
            Ok(_) => {
                self.active_channel = Some(channel_id.to_string());
                self.messages.clear();
                self.messages
                    .push_system(&format!("已加入频道: {}", channel_id));
                self.sync_navigation_selection();

                // Load history
                match self.client.channel_history(channel_id, Some(50)).await {
                    Ok(msgs) => {
                        for msg in &msgs {
                            let is_agent = self.agents.iter().any(|a| a.name == msg.from);
                            self.messages.push(msg, is_agent);
                        }
                    }
                    Err(e) => warn!("load history failed: {}", e),
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
                        ChannelDisplay {
                            id: ch.id,
                            name: ch.name,
                            node_count: ch.nodes.len(),
                            members,
                        }
                    })
                    .collect();
                self.sync_navigation_selection();
            }
            Err(e) => warn!("refresh channels failed: {}", e),
        }
    }

    async fn refresh_agents(&mut self) {
        match self.client.node_list(self.cwd_filter()).await {
            Ok(nodes) => {
                self.agents = nodes
                    .into_iter()
                    .filter(|n| n.transport == "stdio")
                    .map(|n| AgentDisplay {
                        name: n.name,
                        status: n.status,
                        activity: None,
                        adapter: n.adapter,
                        node_id: n.id,
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
            },
            ChannelDisplay {
                id: "ch2".into(),
                name: Some("ops".into()),
                node_count: 1,
                members: Vec::new(),
            },
        ];
        app.agents = vec![AgentDisplay {
            name: "alice".into(),
            status: "idle".into(),
            activity: None,
            adapter: Some("claude".into()),
            node_id: "n1".into(),
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
        }];
        app.agents = vec![
            AgentDisplay {
                name: "alice".into(),
                status: "idle".into(),
                activity: None,
                adapter: Some("claude".into()),
                node_id: "n1".into(),
            },
            AgentDisplay {
                name: "bob".into(),
                status: "busy".into(),
                activity: None,
                adapter: Some("codex".into()),
                node_id: "n2".into(),
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
            },
            ChannelDisplay {
                id: "ch2".into(),
                name: Some("ops".into()),
                node_count: 1,
                members: Vec::new(),
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
            },
            ChannelDisplay {
                id: "ch2".into(),
                name: Some("ops".into()),
                node_count: 1,
                members: Vec::new(),
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
}

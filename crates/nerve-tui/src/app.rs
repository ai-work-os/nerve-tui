use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use futures_util::StreamExt;
use nerve_tui_core::NerveClient;
use nerve_tui_protocol::*;
use ratatui::Frame;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::components::*;
use crate::layout::AppLayout;

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
    /// Channel for background task errors (e.g. prompt failures)
    error_tx: mpsc::UnboundedSender<String>,
    error_rx: mpsc::UnboundedReceiver<String>,
}

impl App {
    pub fn new(client: NerveClient, event_rx: mpsc::UnboundedReceiver<NerveEvent>) -> Self {
        let (error_tx, error_rx) = mpsc::unbounded_channel();
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
            error_tx,
            error_rx,
        }
    }

    /// Initialize: fetch channels, nodes, join first channel if exists.
    pub async fn init(&mut self) -> Result<()> {
        self.refresh_channels().await;
        self.refresh_agents().await;

        // Auto-join first channel
        if let Some(ch) = self.channels.first() {
            let ch_id = ch.id.clone();
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
        let mut redraw_interval =
            tokio::time::interval(tokio::time::Duration::from_millis(100));

        loop {
            terminal.draw(|frame| self.render(frame))?;

            if self.should_quit {
                break;
            }

            tokio::select! {
                Some(Ok(evt)) = event_stream.next() => {
                    match evt {
                        Event::Key(key) => self.handle_key(key).await,
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
        let input_lines = self.input.visual_line_count(area.width.saturating_sub(22)) + 1;
        let layout = AppLayout::new(area, input_lines);

        // Sidebar: channels + agents
        self.status_bar.render(
            &self.channels,
            self.active_channel.as_deref(),
            &self.agents,
            self.active_dm.as_ref().map(|dm| dm.node_name.as_str()),
            layout.sidebar,
            frame.buffer_mut(),
        );

        // Messages
        self.messages.render(layout.messages, frame.buffer_mut());

        // Input
        self.input.render(layout.input, frame.buffer_mut());
        self.input.render_popup(layout.input, frame.buffer_mut());

        // Cursor
        let (cx, cy) = self.input.cursor_position(layout.input);
        frame.set_cursor_position((cx, cy));
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            // Quit
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }

            // Shift+Enter, Alt+Enter, or Ctrl+Enter: insert newline
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.input.insert('\n');
            }

            // Submit
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    let text = self.input.take();
                    self.handle_input(&text).await;
                }
            }

            // Tab completion
            KeyCode::Tab => self.input.tab(),
            KeyCode::BackTab => self.input.shift_tab(),

            // Scroll messages
            KeyCode::PageUp => self.messages.scroll_up(20),
            KeyCode::PageDown => self.messages.scroll_down(20),
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.messages.scroll_up(1);
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.messages.scroll_down(1);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.messages.scroll_up(10);
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.messages.scroll_down(10);
            }

            // Agent tab navigation (Ctrl+N/P)
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.status_bar.select_next_tab(&self.agents);
                self.sync_filter();
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.status_bar.select_prev_tab(&self.agents);
                self.sync_filter();
            }

            // Channel navigation (Ctrl+H/L) — only in channel mode
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.active_dm.is_none() {
                    self.status_bar.select_prev_channel(&self.channels);
                    self.switch_to_selected_channel().await;
                }
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.active_dm.is_none() {
                    self.status_bar.select_next_channel(&self.channels);
                    self.switch_to_selected_channel().await;
                }
            }

            // Esc: exit DM or dismiss popup
            KeyCode::Esc => {
                if self.active_dm.is_some() {
                    self.exit_dm().await;
                } else {
                    self.input.dismiss_popup();
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
            MouseEventKind::ScrollUp => self.messages.scroll_up(3),
            MouseEventKind::ScrollDown => self.messages.scroll_down(3),
            _ => {}
        }
    }

    /// Sync messages filter with current tab selection
    fn sync_filter(&mut self) {
        self.messages.filter = self.status_bar.current_filter(&self.agents);
        self.messages.snap_to_bottom();
    }

    /// Switch to the channel currently selected in sidebar
    async fn switch_to_selected_channel(&mut self) {
        let idx = self.status_bar.selected_channel;
        if let Some(ch) = self.channels.get(idx) {
            let ch_id = ch.id.clone();
            if self.active_channel.as_deref() != Some(&ch_id) {
                self.join_channel(&ch_id).await;
            }
        }
    }

    async fn handle_input(&mut self, text: &str) {
        if text.starts_with('/') {
            self.handle_command(text).await;
            return;
        }

        // DM mode: send via node.prompt
        if let Some(ref dm) = self.active_dm.clone() {
            debug!("dm send to {}: {}", dm.node_name, &text[..text.len().min(50)]);

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

    async fn enter_dm(&mut self, agent_name: &str) {
        let agent = self.agents.iter().find(|a| a.name == agent_name);
        let Some(agent) = agent else {
            self.messages
                .push_system(&format!("找不到 agent: {}", agent_name));
            return;
        };
        let node_id = agent.node_id.clone();
        let node_name = agent.name.clone();

        // Unsubscribe old DM if switching to a different node
        if let Some(ref old_dm) = self.active_dm {
            if old_dm.node_id == node_id {
                debug!("already in DM with {}, ignoring", node_name);
                return;
            }
            let old_node_id = old_dm.node_id.clone();
            debug!("switching DM: unsubscribe old node {}", old_node_id);
            if let Err(e) = self.client.node_unsubscribe(&old_node_id).await {
                warn!("unsubscribe old DM failed: {}", e);
            }
            self.messages.exit_dm();
            self.active_dm = None;
        }

        debug!("entering DM with {} ({})", node_name, node_id);

        // Subscribe to node updates
        if let Err(e) = self.client.node_subscribe(&node_id).await {
            self.messages
                .push_system(&format!("subscribe 失败: {}", e));
            return;
        }

        self.active_dm = Some(DmState {
            node_id,
            node_name: node_name.clone(),
            messages: Vec::new(),
            streaming: None,
        });
        self.messages.enter_dm(&node_name);
    }

    async fn exit_dm(&mut self) {
        if let Some(dm) = self.active_dm.take() {
            debug!("exiting DM with {}", dm.node_name);
            if let Err(e) = self.client.node_unsubscribe(&dm.node_id).await {
                warn!("unsubscribe failed: {}", e);
            }
            self.messages.exit_dm();
        }
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

            "/create" => {
                let name = parts.get(1).copied();
                match self.client.channel_create(name).await {
                    Ok(ch) => {
                        self.messages
                            .push_system(&format!("频道已创建: {}", ch.id));
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
                    let channels = self.client.channel_list().await.unwrap_or_default();
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
                match self.client.node_spawn(adapter, name, None).await {
                    Ok(node) => {
                        self.messages
                            .push_system(&format!("已启动: {} ({})", node.name, node.node_id));
                        if let Some(ref ch_id) = self.active_channel.clone() {
                            if let Err(e) = self
                                .client
                                .channel_add_node(ch_id, &node.node_id, Some(&node.name))
                                .await
                            {
                                self.messages
                                    .push_system(&format!("加入频道失败: {}", e));
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
                            Err(e) => {
                                self.messages.push_system(&format!("停止失败: {}", e))
                            }
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
                            Ok(_) => {
                                self.messages
                                    .push_system(&format!("已取消: {}", name_or_id))
                            }
                            Err(e) => {
                                self.messages.push_system(&format!("取消失败: {}", e))
                            }
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

            "/help" => {
                self.messages.push_system("命令:");
                self.messages.push_system("  /create [name]        创建频道");
                self.messages.push_system("  /join [id]            加入频道");
                self.messages.push_system("  /channels             列出频道");
                self.messages
                    .push_system("  /add <adapter> [name] 启动 agent");
                self.messages.push_system("  /stop <name>          停止 agent");
                self.messages
                    .push_system("  /cancel <name>        取消 agent 任务");
                self.messages.push_system("  /list                 列出 agents");
                self.messages
                    .push_system("  /dm <name>            与 agent 1v1 对话");
                self.messages
                    .push_system("  /back                 退出 DM 回频道");
                self.messages.push_system("  /help                 帮助");
                self.messages.push_system("快捷键:");
                self.messages.push_system("  Ctrl+N/P  切换 agent 过滤 tab");
                self.messages.push_system("  Ctrl+H/L  切换频道");
                self.messages.push_system("  Esc       退出 DM / 关闭补全");
                self.messages.push_system("  Ctrl+C/Q  退出");
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
                if self.active_dm.is_none() {
                    let is_agent = self.agents.iter().any(|a| a.name == message.from);
                    self.messages.push(&message, is_agent);
                }
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
                }
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
                        debug!("agent_message_chunk from {} has empty text, raw: {:?}", name, update);
                    }
                    if let Some(entry) = self
                        .messages
                        .streaming
                        .iter_mut()
                        .find(|(n, _)| n == name)
                    {
                        entry.1.push_str(text);
                    } else {
                        self.messages
                            .streaming
                            .push((name.to_string(), text.to_string()));
                    }
                }
                Some("agent_message_start") => {
                    debug!("agent_message_start from {}, clearing streaming buffer", name);
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
                }
                Some("user_message") => {
                    // Replay of user's prompt (from subscribe buffer)
                    if in_dm {
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

                // Update selected_channel to match
                if let Some(idx) = self.channels.iter().position(|c| c.id == channel_id) {
                    self.status_bar.selected_channel = idx;
                }

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
        match self.client.channel_list().await {
            Ok(list) => {
                self.channels = list
                    .into_iter()
                    .map(|ch| ChannelDisplay {
                        id: ch.id,
                        name: ch.name,
                        node_count: ch.nodes.len(),
                    })
                    .collect();
            }
            Err(e) => warn!("refresh channels failed: {}", e),
        }
    }

    async fn refresh_agents(&mut self) {
        match self.client.node_list().await {
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

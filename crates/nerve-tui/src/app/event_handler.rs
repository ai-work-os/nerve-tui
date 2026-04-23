use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use nerve_tui_core::Transport;
use nerve_tui_protocol::*;
use tracing::{debug, info, warn};

use super::app_state::{SplitFocus, SplitPanel, SplitTarget};
use super::App;
use crate::clipboard;
use crate::components::channel_view::ChannelPanelState;
use crate::components::dm_view::DmView;
use crate::components::*;

impl<T: Transport> App<T> {
    pub(crate) async fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            // Ctrl+C: cancel active DM response if agent is responding
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.is_dm_mode() && self.dm_view.is_responding {
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
                if self.is_dm_mode() {
                    let has_target = self.active_channel.is_some()
                        || self.split_panels.iter().any(|p| matches!(p.target, SplitTarget::Node { .. }));
                    if has_target {
                        if self.is_split() {
                            self.close_all_split_panels().await;
                        } else {
                            self.split_panels.push(SplitPanel {
                                target: SplitTarget::Channel,
                                node_buffer: String::new(),
                                node_msg_pending: false,
                                panel_state: ChannelPanelState::new(),
                            });
                            self.split_focus = SplitFocus::Dm;
                            self.split_panels[0].panel_state.snap_to_bottom();
                        }
                    } else {
                        self.push_system_to_active("需要先加入频道才能分屏");
                    }
                }
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.is_split() && self.is_dm_mode() {
                    self.split_focus = match self.split_focus {
                        SplitFocus::Dm => SplitFocus::Panel(0),
                        SplitFocus::Panel(i) => {
                            if i + 1 < self.split_panel_count() {
                                SplitFocus::Panel(i + 1)
                            } else {
                                SplitFocus::Dm
                            }
                        }
                    };
                } else {
                    self.input.delete_word();
                }
            }

            // Up/Down: multi-line cursor move > history browse > scroll messages
            KeyCode::Up if key.modifiers.is_empty() => {
                if self.input.is_multiline() && self.input.move_up() {
                    // Moved cursor up within input
                } else if !self.input.is_multiline() && self.input.history_up() {
                    // Browsing history in single-line mode
                } else if let Some(panel) = self.focused_panel_mut() {
                    panel.panel_state.scroll_up(1);
                } else {
                    self.scroll_active_up(1);
                }
            }
            KeyCode::Down if key.modifiers.is_empty() => {
                if self.input.is_multiline() && self.input.move_down() {
                    // Moved cursor down within input
                } else if !self.input.is_multiline() && self.input.history_down() {
                    // Browsing history in single-line mode
                } else if let Some(panel) = self.focused_panel_mut() {
                    panel.panel_state.scroll_down(1);
                } else {
                    self.scroll_active_down(1);
                }
            }

            // Scroll messages (dispatched to focused panel in split mode)
            KeyCode::PageUp => {
                if let Some(panel) = self.focused_panel_mut() {
                    panel.panel_state.page_up();
                } else {
                    self.page_active_up();
                }
            }
            KeyCode::PageDown => {
                if let Some(panel) = self.focused_panel_mut() {
                    panel.panel_state.page_down();
                } else {
                    self.page_active_down();
                }
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(panel) = self.focused_panel_mut() {
                    panel.panel_state.scroll_down(1);
                } else {
                    self.scroll_active_down(1);
                }
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(panel) = self.focused_panel_mut() {
                    panel.panel_state.scroll_down(10);
                } else {
                    self.scroll_active_down(10);
                }
            }

            // Emacs keybindings (line-aware for multiline input)
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.move_line_start();
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.is_dm_mode() {
                    self.dm_view.toggle_summary_mode();
                } else {
                    self.input.move_line_end();
                }
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.kill_to_line_end();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.kill_to_line_start();
            }

            // Ctrl+L: force full redraw (clear terminal + repaint)
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.force_clear = true;
                self.needs_redraw = true;
            }

            // Ctrl+V: check clipboard for image, fall back to text paste
            KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.handle_paste_from_key().await;
            }

            // Toggle sidebar
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.sidebar_visible = !self.sidebar_visible;
            }

            // Ctrl+G: toggle global/project mode
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.global_mode = !self.global_mode;
                let mode = if self.global_mode { "全局" } else { "项目" };
                self.push_system_to_active(&format!("切换到{}模式", mode));
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
                if let ViewMode::Dm { ref node_name, .. } = self.view_mode.clone() {
                    if !self.agents.iter().any(|a| a.name == *node_name) {
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
                } else if self.is_split() && matches!(self.split_focus, SplitFocus::Panel(_)) {
                    self.split_focus = SplitFocus::Dm;
                } else if self.is_dm_mode() {
                    if self.dm_view.is_responding {
                        self.cancel_active_dm().await;
                    } else {
                        self.exit_dm().await;
                    }
                }
            }

            // Shift+Left/Right: cycle agent filter tabs in channel mode
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if !self.is_dm_mode() {
                    self.cycle_agent_filter(false);
                }
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if !self.is_dm_mode() {
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

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if let Some(idx) = self.mouse_panel_index(mouse.column) {
                    self.split_panels[idx].panel_state.scroll_up(3);
                } else {
                    self.scroll_active_up(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(idx) = self.mouse_panel_index(mouse.column) {
                    self.split_panels[idx].panel_state.scroll_down(3);
                } else {
                    self.scroll_active_down(3);
                }
            }
            _ => {}
        }
    }

    /// Find which split panel the mouse column falls in (if any).
    pub(crate) fn mouse_panel_index(&self, column: u16) -> Option<usize> {
        if self.panel_x_boundaries.is_empty() {
            return None;
        }
        // Find the rightmost boundary that the column is >= to
        let mut result = None;
        for (i, &bx) in self.panel_x_boundaries.iter().enumerate() {
            if column >= bx {
                result = Some(i);
            }
        }
        result
    }

    /// Cycle through agent filter tabs: None → agent1 → agent2 → ... → None
    fn cycle_agent_filter(&mut self, forward: bool) {
        let agent_names: Vec<String> = self.agents.iter().map(|a| a.name.clone()).collect();
        if agent_names.is_empty() {
            return;
        }
        let current_idx = self.channel_view.filter.as_ref()
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
        self.channel_view.filter = new_filter;
        self.snap_active_to_bottom();
    }

    pub(crate) fn sync_navigation_selection(&mut self) {
        self.channel_view.filter = None;
        let dm_name = match &self.view_mode {
            ViewMode::Dm { ref node_name, .. } => Some(node_name.as_str()),
            _ => None,
        };
        self.status_bar.sync_to_context(
            &self.channels,
            self.active_channel.as_deref(),
            &self.agents,
            dm_name,
        );
        self.snap_active_to_bottom();
    }

    pub(crate) async fn confirm_selected_navigation(&mut self) {
        // Check if selected item is a section header → toggle collapse
        let items = self.status_bar.visible_items(&self.channels, &self.agents);
        if let Some(SidebarItem::SectionHeader(ref name)) = items.get(self.status_bar.selected_nav)
        {
            let name = name.clone();
            self.status_bar.toggle_section(&name);
            return;
        }

        match self
            .status_bar
            .selected_target(&self.channels, &self.agents)
        {
            Some(NavigationTarget::Channel(idx)) => {
                if let Some(ch) = self.channels.get(idx) {
                    let ch_id = ch.id.clone();
                    if self.is_dm_mode() {
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

    pub(crate) async fn handle_input(&mut self, text: &str) {
        // Push to input history (skip commands)
        if !text.starts_with('/') {
            self.input.history_push(text);
            self.input.history_reset();
        }

        if text.starts_with('/') {
            self.handle_command(text).await;
            return;
        }

        // DM mode: send via node.prompt
        if let ViewMode::Dm { ref node_id, ref node_name } = self.view_mode.clone() {
            if self.dm_view.is_responding {
                debug!(
                    "dm send blocked for {}: agent still responding",
                    node_name
                );
                self.push_contextual_system("agent 正在回复，先等待完成或 Ctrl+C 取消");
                return;
            }

            debug!(
                "dm send to {}: {}",
                node_name,
                &text[..{let mut e = text.len().min(50); while e > 0 && !text.is_char_boundary(e) { e -= 1; } e}]
            );

            // Flush any remaining streaming content before adding user message,
            // so user message appears after the agent's reply, not before streaming tail.
            let node_id_ref = node_id.clone();
            let node_name_ref = node_name.clone();
            self.flush_streaming_as_dm(&node_id_ref, &node_name_ref);

            // Add user message locally immediately
            let tagged = format!("{}: {}", self.client.node_name(), text);
            let user_msg = DmMessage {
                role: "user".to_string(),
                content: tagged,
                timestamp: chrono::Local::now().timestamp(),
            };
            self.dm_view.push(&user_msg);
            self.dm_view.dm_history.push(user_msg);

            // Determine transport: program nodes use node.message, AI nodes use node.prompt
            let is_program = self.agents.iter()
                .any(|a| a.node_id == *node_id && a.transport != "stdio");

            // Send in background — response comes via node.update or node.log
            let node_id = node_id.clone();
            let content = text.to_string();
            let client = self.client.clone();
            let error_tx = self.error_tx.clone();
            tokio::spawn(async move {
                let result = if is_program {
                    client.node_message(&node_id, &content).await
                } else {
                    client.node_prompt(&node_id, &content).await.map(|_| ())
                };
                if let Err(e) = result {
                    let method = if is_program { "node.message" } else { "node.prompt" };
                    warn!("{} failed: {}", method, e);
                    let _ = error_tx.send(format!("发送失败: {}", e));
                }
            });
            return;
        }

        // Channel mode: post to active channel
        if let Some(ref ch_id) = self.active_channel.clone() {
            match self.client.channel_post(ch_id, text).await {
                Ok(_) => {} // Message arrives via channel.message notification
                Err(e) => self.channel_view.push_system(&format!("发送失败: {}", e)),
            }
        } else {
            self.channel_view
                .push_system("未加入频道，用 /join 或 /create 先创建");
        }
    }

    // --- DM mode ---

    /// Push a system message to the currently active view (Channel or DM).
    pub(crate) fn push_system_to_active(&mut self, content: &str) {
        match &self.view_mode {
            ViewMode::Channel { .. } => self.channel_view.push_system(content),
            ViewMode::Dm { .. } => self.dm_view.push_system(content),
        }
    }

    pub(crate) fn push_contextual_system(&mut self, content: &str) {
        if self.is_dm_mode() {
            let dm_msg = DmMessage {
                role: "系统".to_string(),
                content: content.to_string(),
                timestamp: chrono::Local::now().timestamp(),
            };
            self.dm_view.push(&dm_msg);
            self.dm_view.dm_history.push(dm_msg);
        } else {
            self.channel_view.push_system(content);
        }
    }

    pub(crate) fn reset_dm_before_enter(&mut self, next_node_id: &str) -> Option<String> {
        let (old_node_id, old_node_name) = match &self.view_mode {
            ViewMode::Dm { node_id, node_name } => (node_id.clone(), node_name.clone()),
            _ => return None,
        };
        let same_node = old_node_id == next_node_id;
        if same_node {
            debug!(
                "re-entering DM with {}, resetting local DM state",
                old_node_name
            );
        } else {
            debug!("switching DM: unsubscribe old node {}", old_node_id);
        }

        self.dm_view.clear();
        self.view_mode = ViewMode::Channel {
            channel_id: self.active_channel.clone().unwrap_or_default(),
        };
        Some(old_node_id)
    }

    pub(crate) async fn enter_dm(&mut self, agent_name: &str) {
        let agent = self.agents.iter().find(|a| a.name == agent_name);
        let Some(agent) = agent else {
            self.channel_view
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
            self.channel_view.push_system(&format!("subscribe 失败: {}", e));
            return;
        }

        self.dm_view = DmView::new(&node_name);
        self.view_mode = ViewMode::Dm { node_id, node_name: node_name.clone() };

        // Initialize model + usage display from agent snapshot
        if let Some(agent) = self.agents.iter().find(|a| a.name == node_name) {
            let token_size = agent.usage.map(|(_, size, _)| size);
            self.dm_view.set_model_label(agent.model.as_deref(), token_size);
            if let Some((used, size, cost)) = agent.usage {
                self.dm_view.update_usage(used, size, cost);
            }
        }

        self.sync_navigation_selection();
    }

    pub(crate) async fn exit_dm(&mut self) {
        if let ViewMode::Dm { ref node_id, ref node_name } = self.view_mode.clone() {
            debug!("exiting DM with {}", node_name);
            // Only unsubscribe the DM node if no split panel is also watching it
            let split_has_node = self.split_panels.iter().any(|p| {
                matches!(&p.target, SplitTarget::Node { node_id: sid, .. } if sid == node_id)
            });
            if !split_has_node {
                if let Err(e) = self.client.node_unsubscribe(node_id).await {
                    warn!("unsubscribe failed: {}", e);
                }
            }
            // Preserve split panels — they are independent of the DM session
            self.dm_view.clear();
            self.view_mode = ViewMode::Channel {
                channel_id: self.active_channel.clone().unwrap_or_default(),
            };
            self.split_focus = SplitFocus::Dm;
            self.sync_navigation_selection();
        }
    }

    async fn cancel_active_dm(&mut self) {
        let (node_id, node_name) = match &self.view_mode {
            ViewMode::Dm { node_id, node_name } => (node_id.clone(), node_name.clone()),
            _ => return,
        };

        debug!("cancelling active DM with {}", node_name);
        if let Err(e) = self.client.node_cancel(&node_id).await {
            self.push_contextual_system(&format!("取消失败: {}", e));
            return;
        }

        self.flush_streaming_as_dm(&node_id, &node_name);
        self.dm_view.is_responding = false;
        self.push_contextual_system(&format!("已中断 {}", node_name));
    }

    /// Handle Ctrl+V key: check clipboard for image only.
    /// Text paste is handled separately by Event::Paste (bracketed paste).
    async fn handle_paste_from_key(&mut self) {
        if let Some(path) = clipboard::try_paste_image().await {
            let path_str = path.to_string_lossy().to_string();
            info!("clipboard image pasted via Ctrl+V: {}", path_str);
            self.input.insert_str(&format!("[截图: {}]", path_str));
            self.push_system_to_active(&format!("图片已保存: {}", path_str));
        }
        // No image — do nothing; text paste comes via Event::Paste
    }

    /// Handle bracketed paste event: check clipboard for image first, fall back to text.
    pub async fn handle_paste(&mut self, text: &str) {
        // Try clipboard image first (user may have copied an image)
        if let Some(path) = clipboard::try_paste_image().await {
            let path_str = path.to_string_lossy().to_string();
            info!("clipboard image pasted: {}", path_str);
            self.input.insert_str(&format!("[截图: {}]", path_str));
            self.push_system_to_active(&format!("图片已保存: {}", path_str));
            return;
        }
        // No image — insert text as before
        self.input.insert_str(text);
    }

    pub(crate) async fn handle_command(&mut self, text: &str) {
        let parts: Vec<&str> = text.splitn(3, ' ').collect();
        let cmd = parts[0];

        match cmd {
            "/dm" => {
                if let Some(name) = parts.get(1) {
                    self.enter_dm(name).await;
                } else {
                    self.push_system_to_active("用法: /dm <agent_name>");
                }
            }

            "/ch" => {
                let rest = text.strip_prefix("/ch ").map(str::trim);
                if let Some(name) = rest.filter(|s| !s.is_empty()) {
                    if let Some(ch) = self.channels.iter().find(|c| {
                        c.name.as_deref() == Some(name) || c.id == name
                    }) {
                        let ch_id = ch.id.clone();
                        if self.is_dm_mode() {
                            self.exit_dm().await;
                        }
                        self.join_channel(&ch_id).await;
                    } else {
                        self.push_system_to_active(&format!("找不到频道: {}", name));
                    }
                } else {
                    self.push_system_to_active("用法: /ch <channel_name>");
                }
            }

            "/back" => {
                self.exit_dm().await;
            }

            "/clear" => {
                if let ViewMode::Dm { ref node_name, .. } = self.view_mode.clone() {
                    match self.client.session_clear(&node_name).await {
                        Ok(_) => {
                            self.dm_view.clear();
                            self.dm_view.is_responding = false;
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
                if let ViewMode::Dm { ref node_name, .. } = self.view_mode.clone() {
                    let node_name = node_name.clone();
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
                        self.channel_view.push_system(&format!("频道已创建: {}", ch.id));
                        let ch_id = ch.id.clone();
                        self.refresh_channels().await;
                        self.join_channel(&ch_id).await;
                    }
                    Err(e) => self.channel_view.push_system(&format!("创建失败: {}", e)),
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
                        self.channel_view.push_system("没有可用频道，用 /create 创建");
                    }
                }
            }

            "/add" => {
                if parts.len() < 2 {
                    self.channel_view.push_system("用法: /add <adapter> [name]");
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
                        self.channel_view
                            .push_system(&format!("已启动: {} ({})", node.name, node.node_id));
                        if let Some(ref ch_id) = self.active_channel.clone() {
                            if let Err(e) = self
                                .client
                                .channel_add_node(ch_id, &node.node_id, Some(&node.name))
                                .await
                            {
                                self.channel_view.push_system(&format!("加入频道失败: {}", e));
                            }
                        }
                        self.refresh_agents().await;
                    }
                    Err(e) => self.channel_view.push_system(&format!("启动失败: {}", e)),
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
                                self.channel_view
                                    .push_system(&format!("已停止: {}", name_or_id));
                                self.refresh_agents().await;
                            }
                            Err(e) => self.channel_view.push_system(&format!("停止失败: {}", e)),
                        }
                    } else {
                        self.channel_view
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
                                .channel_view
                                .push_system(&format!("已取消: {}", name_or_id)),
                            Err(e) => self.channel_view.push_system(&format!("取消失败: {}", e)),
                        }
                    }
                }
            }

            "/list" => {
                self.refresh_agents().await;
                if self.agents.is_empty() {
                    self.channel_view.push_system("没有 agent");
                } else {
                    for a in &self.agents {
                        self.channel_view.push_system(&format!(
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
                    self.channel_view.push_system("没有频道");
                } else {
                    for ch in &self.channels {
                        let active = if self.active_channel.as_deref() == Some(&ch.id) {
                            " ← 当前"
                        } else {
                            ""
                        };
                        self.channel_view.push_system(&format!(
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
                                self.channel_view
                                    .push_system(&format!("频道已恢复: {}", name));
                                self.archived_channels.clear();
                                self.refresh_channels().await;
                                self.join_channel(&restored_id).await;
                            }
                            Err(e) => self.channel_view.push_system(&format!("恢复失败: {}", e)),
                        }
                    } else {
                        self.channel_view.push_system("无效序号，先用 /restore 查看列表");
                    }
                } else {
                    // /restore with no args: list archived channels
                    match self.client.channel_list_archived(self.cwd_filter()).await {
                        Ok(channels) => {
                            if channels.is_empty() {
                                self.archived_channels.clear();
                                self.channel_view.push_system("没有归档频道");
                            } else {
                                self.channel_view.push_system("归档频道:");
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
                                    self.channel_view.push_system(&format!(
                                        "  {}. {}{}",
                                        i + 1,
                                        display,
                                        agents_str
                                    ));
                                }
                                self.channel_view
                                    .push_system("用 /restore <序号> 恢复频道");
                                self.archived_channels = channels;
                            }
                        }
                        Err(e) => {
                            self.archived_channels.clear();
                            self.channel_view
                                .push_system(&format!("获取归档列表失败: {}", e));
                        }
                    }
                }
            }

            "/help" => {
                self.channel_view.push_system("命令:");
                self.channel_view
                    .push_system("  /create [name]        创建频道");
                self.channel_view
                    .push_system("  /join [id]            加入频道");
                self.channel_view
                    .push_system("  /channels             列出频道");
                self.channel_view
                    .push_system("  /add <adapter> [name] 启动 agent");
                self.channel_view
                    .push_system("  /stop <name>          停止 agent");
                self.channel_view
                    .push_system("  /cancel <name>        取消 agent 任务");
                self.channel_view
                    .push_system("  /list                 列出 agents");
                self.channel_view
                    .push_system("  /restore [n]          恢复归档频道");
                self.channel_view
                    .push_system("  /clear                清除 DM session");
                self.channel_view
                    .push_system("  /compact              压缩 DM 上下文");
                self.channel_view
                    .push_system("  /split [@agent]       分屏(频道或agent输出)");
                self.channel_view
                    .push_system("  /ch <name>            切换频道");
                self.channel_view
                    .push_system("  /dm <name>            与 agent 1v1 对话");
                self.channel_view
                    .push_system("  /back                 退出 DM 回频道");
                self.channel_view.push_system("  /help                 帮助");
                self.channel_view.push_system("快捷键:");
                self.channel_view.push_system("  Enter       发送消息 / 确认选择");
                self.channel_view.push_system("  Tab         补全 / 确认选择");
                self.channel_view.push_system("  Ctrl+O      输入框换行");
                self.channel_view.push_system("  Ctrl+C      中断当前 DM 回复");
                self.channel_view.push_system("  Esc         DM回复中=取消，否则退出DM");
                self.channel_view.push_system("  Ctrl+N/P    侧边栏导航 下/上");
                self.channel_view.push_system("  Ctrl+J/K    滚动消息 下/上（1行）");
                self.channel_view.push_system("  Ctrl+D/U    滚动消息 下/上（10行）");
                self.channel_view.push_system("  PgDn/PgUp   翻页（20行）");
                self.channel_view.push_system("  Ctrl+G      切换全局/项目模式");
                self.channel_view.push_system("  Ctrl+L      强制刷新重绘");
                self.channel_view.push_system("  Ctrl+Q      退出");
            }

            "/split" => {
                let arg = parts.get(1).copied().unwrap_or("");
                let arg2 = parts.get(2).copied().unwrap_or("");

                if arg == "close" && arg2 == "all" {
                    self.close_all_split_panels().await;
                } else if arg == "close" {
                    // /split close — remove focused panel
                    if let SplitFocus::Panel(i) = self.split_focus {
                        self.close_split_panel(i).await;
                    } else {
                        self.push_contextual_system("焦点不在面板上，用 Ctrl+W 切换焦点");
                    }
                } else if arg.starts_with('@') {
                    // /split @agent-name: show agent output in right panel
                    let agent_name = &arg[1..];

                    // Dedup: if agent already has a panel, just focus it
                    if let Some(idx) = self.split_panels.iter().position(|p| {
                        matches!(&p.target, SplitTarget::Node { node_name, .. } if node_name == agent_name)
                    }) {
                        self.split_focus = SplitFocus::Panel(idx);
                        self.push_contextual_system(&format!("已聚焦 @{}", agent_name));
                    } else if self.split_panels.len() >= 4 {
                        self.push_contextual_system("面板已满（最多 4 个）");
                    } else if let Some(agent) = self.agents.iter().find(|a| a.name == agent_name) {
                        let node_id = agent.node_id.clone();
                        let node_name = agent.name.clone();
                        // Subscribe to target node
                        if let Err(e) = self.client.node_subscribe(&node_id).await {
                            self.push_contextual_system(&format!("subscribe 失败: {}", e));
                        } else {
                            let mut new_panel = SplitPanel {
                                target: SplitTarget::Node { node_id, node_name: node_name.clone() },
                                node_buffer: String::new(),
                                node_msg_pending: false,
                                panel_state: ChannelPanelState::new(),
                            };
                            new_panel.panel_state.snap_to_bottom();
                            self.split_panels.push(new_panel);
                            self.split_focus = SplitFocus::Dm;
                            self.push_contextual_system(&format!("分屏查看 @{}", node_name));
                        }
                    } else {
                        self.push_contextual_system(&format!("找不到 agent: {}", agent_name));
                    }
                } else if arg.starts_with('#') {
                    // /split #channel — add a channel panel
                    let _channel_name = &arg[1..];
                    if self.split_panels.len() >= 4 {
                        self.push_contextual_system("面板已满（最多 4 个）");
                    } else {
                        let mut new_panel = SplitPanel {
                            target: SplitTarget::Channel,
                            node_buffer: String::new(),
                            node_msg_pending: false,
                            panel_state: ChannelPanelState::new(),
                        };
                        new_panel.panel_state.snap_to_bottom();
                        self.split_panels.push(new_panel);
                        self.split_focus = SplitFocus::Dm;
                    }
                } else if self.is_dm_mode() {
                    if self.is_split() && arg.is_empty() {
                        self.close_all_split_panels().await;
                    } else if self.active_channel.is_some() {
                        if self.split_panels.len() >= 4 {
                            self.push_contextual_system("面板已满（最多 4 个）");
                        } else {
                            let mut new_panel = SplitPanel {
                                target: SplitTarget::Channel,
                                node_buffer: String::new(),
                                node_msg_pending: false,
                                panel_state: ChannelPanelState::new(),
                            };
                            new_panel.panel_state.snap_to_bottom();
                            self.split_panels.push(new_panel);
                            self.split_focus = SplitFocus::Dm;
                        }
                    } else {
                        self.push_contextual_system("需要先加入频道才能分屏");
                    }
                } else if arg.is_empty() {
                    self.push_contextual_system("用法: /split [@agent|#channel|close|close all]");
                }
            }

            "/scene" => {
                if let Some(sub) = parts.get(1) {
                    match *sub {
                        "stop" => {
                            if let Some(name) = parts.get(2) {
                                match self.client.scene_stop(name).await {
                                    Ok(_) => self.push_contextual_system(&format!("场景已停止: {}", name)),
                                    Err(e) => self.push_contextual_system(&format!("停止失败: {}", e)),
                                }
                            } else {
                                self.push_contextual_system("用法: /scene stop <name>");
                            }
                        }
                        "list" => {
                            match self.client.scene_list().await {
                                Ok(scenes) => {
                                    if scenes.is_empty() {
                                        self.push_contextual_system("没有可用场景");
                                    } else {
                                        for s in &scenes {
                                            let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                                            let running = s.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
                                            let status = if running { " [运行中]" } else { "" };
                                            self.push_contextual_system(&format!("  {}{}", name, status));
                                        }
                                    }
                                }
                                Err(e) => self.push_contextual_system(&format!("获取失败: {}", e)),
                            }
                        }
                        name => {
                            // /scene <name> — start the scene
                            self.push_contextual_system(&format!("启动场景: {}...", name));
                            match self.client.scene_start(name, self.project_path.as_deref()).await {
                                Ok(result) => {
                                    let ch_id = result.get("channelId").and_then(|v| v.as_str());
                                    self.push_contextual_system(&format!("场景 {} 已启动", name));
                                    self.refresh_agents().await;
                                    self.refresh_channels().await;
                                    // Auto-join the scene's channel
                                    if let Some(ch_id) = ch_id {
                                        self.join_channel(ch_id).await;
                                    }
                                }
                                Err(e) => self.push_contextual_system(&format!("启动失败: {}", e)),
                            }
                        }
                    }
                } else {
                    // /scene with no args — list scenes
                    match self.client.scene_list().await {
                        Ok(scenes) => {
                            if scenes.is_empty() {
                                self.push_contextual_system("没有可用场景");
                            } else {
                                self.push_contextual_system("可用场景:");
                                for s in &scenes {
                                    let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                                    let running = s.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
                                    let status = if running { " [运行中]" } else { "" };
                                    self.push_contextual_system(&format!("  {}{}", name, status));
                                }
                            }
                        }
                        Err(e) => self.push_contextual_system(&format!("获取失败: {}", e)),
                    }
                }
            }

            "/quit" | "/q" => {
                self.should_quit = true;
            }

            _ => {
                self.channel_view.push_system(&format!("未知命令: {}", cmd));
            }
        }
    }

    pub(crate) async fn handle_nerve_event(&mut self, event: NerveEvent) {
        debug!("nerve event: {}", event.kind());

        match event {
            NerveEvent::ChannelMessage {
                channel_id,
                message,
            } => {
                let is_active = self.active_channel.as_deref() == Some(&channel_id);
                if is_active {
                    let is_agent = self.agents.iter().any(|a| a.name == message.from);
                    self.channel_view.push(&message, is_agent);
                } else {
                    // Cache message for non-active channel + bump unread
                    self.channel_view.push_to_channel(&channel_id, &message);
                    // Update sidebar unread badge
                    if let Some(ch) = self.channels.iter_mut().find(|c| c.id == channel_id) {
                        ch.unread = self.channel_view.unread_count(&channel_id);
                    }
                }
            }

            NerveEvent::ChannelMention { channel_id, message } => {
                // Dedup: active channel mentions already shown via ChannelMessage handler.
                // Non-active channel mentions always show (even in DM mode).
                let is_active = self.active_channel.as_deref() == Some(&channel_id);
                if !is_active {
                    let is_agent = self.agents.iter().any(|a| a.name == message.from);
                    self.channel_view.push(&message, is_agent);
                }
            }

            NerveEvent::NodeJoined { node_name, .. } => {
                if !self.is_dm_mode() {
                    self.channel_view
                        .push_system(&format!("{} 加入频道", node_name));
                }
                self.refresh_agents().await;
                self.refresh_channels().await;
            }

            NerveEvent::NodeLeft { node_name, .. } => {
                if !self.is_dm_mode() {
                    self.channel_view
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

            NerveEvent::MessageSnapshot {
                node_id,
                name: _name,
                messages,
            } => {
                // Only apply snapshots for the currently active DM view.
                // Snapshots for non-active nodes are ignored — when the user
                // enters that DM next, a fresh subscribe will deliver another
                // snapshot with up-to-date contents.
                if self.dm_node_id() == Some(node_id.as_str()) {
                    debug!(node_id = %node_id, count = messages.len(), "applying message_snapshot");
                    self.dm_view.replace_history(&messages);
                } else {
                    debug!(node_id = %node_id, "ignoring snapshot for non-active DM");
                }
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
                } else if self.dm_node_id() == Some(node_id.as_str()) && status == "busy" {
                    self.dm_view.is_responding = true;
                }
            }

            NerveEvent::ChannelCreated {
                channel_id, name, ..
            } => {
                // Refresh first so we can check if this channel is in our filtered view
                self.refresh_channels().await;
                if self.channels.iter().any(|c| c.id == channel_id) {
                    let label = name.as_deref().unwrap_or("unnamed");
                    self.channel_view
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
                    self.channel_view
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
                if self.dm_node_id() == Some(node_id.as_str()) {
                    self.dm_view.clear();
                    self.view_mode = ViewMode::Channel {
                        channel_id: self.active_channel.clone().unwrap_or_default(),
                    };
                    self.channel_view
                        .push_system(&format!("{} 已停止", name));
                } else if self.agents.iter().any(|a| a.node_id == node_id) {
                    self.channel_view
                        .push_system(&format!("{} 已停止", name));
                }
                // Clean up split panels targeting the stopped node
                let had_panel = self.split_panels.iter().any(|p| {
                    matches!(&p.target, SplitTarget::Node { node_id: sid, .. } if sid == &node_id)
                });
                if had_panel {
                    let _ = self.client.node_unsubscribe(&node_id).await;
                    self.split_panels.retain(|p| {
                        !matches!(&p.target, SplitTarget::Node { node_id: sid, .. } if sid == &node_id)
                    });
                    self.clamp_split_focus();
                }
                // Remove from agents list immediately
                self.agents.retain(|a| a.node_id != node_id);
                self.update_completions();
                self.sync_navigation_selection();
            }

            NerveEvent::Disconnected => {
                self.channel_view.push_system("⚠ 与 nerve 断开连接");
            }
        }
    }

    /// Flush streaming buffer as a DM message when agent goes idle (no explicit end event).
    pub(crate) fn flush_streaming_as_dm(&mut self, node_id: &str, name: &str) {
        let in_dm = self.dm_node_id() == Some(node_id);
        if !in_dm {
            return;
        }

        // Take structured message from streaming pipeline
        if let Some(msg) = self.dm_view.take_streaming_message(name) {
            if !msg.blocks.is_empty() {
                let content = crate::components::dm_view::blocks_to_text(&msg.blocks);
                debug!(
                    "flush_streaming_as_dm: {} persisting {} blocks, {} chars",
                    name, msg.blocks.len(), content.len()
                );
                let dm_msg = DmMessage {
                    role: "assistant".to_string(),
                    content,
                    timestamp: chrono::Local::now().timestamp(),
                };
                self.dm_view.push_with_blocks(&dm_msg, msg.blocks);
                self.dm_view.dm_history.push(dm_msg);
            }
        }
        self.dm_view.flushed_agents.insert(name.to_string());
        if self.dm_node_id() == Some(node_id) {
            self.dm_view.set_responding(false);
        }
    }

    pub(crate) fn handle_node_update(&mut self, node_id: &str, name: &str, detail: &serde_json::Value) {
        let in_dm = self.dm_node_id() == Some(node_id);

        // Route updates to split node buffers for all panels targeting this node
        for panel in &mut self.split_panels {
            let matches = matches!(&panel.target, SplitTarget::Node { node_id: sid, .. } if sid == node_id);
            if matches {
                if let Some(update) = detail.get("update") {
                    let kind = update.get("sessionUpdate").and_then(|v| v.as_str());
                    if kind == Some("agent_message_chunk") {
                        if let Some(text) = update.get("content").and_then(|c| c.get("text")).and_then(|v| v.as_str()) {
                            // Prepend role+timestamp header on first chunk of a new message
                            if !panel.node_msg_pending {
                                panel.node_msg_pending = true;
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                panel.node_buffer.push_str(&format!("assistant  {}\n", ts));
                            }
                            panel.node_buffer.push_str(text);
                        }
                    } else if kind == Some("agent_message_end") || kind == Some("stop_reason") {
                        panel.node_msg_pending = false;
                        panel.node_buffer.push('\n');
                    } else if kind == Some("node_log") {
                        if let Some(entries) = update.get("entries").and_then(|v| v.as_array()) {
                            for entry in entries {
                                let level = entry.get("level").and_then(|v| v.as_str()).unwrap_or("info");
                                let message = entry.get("message").and_then(|v| v.as_str()).unwrap_or("");
                                let ts_str = entry.get("ts").and_then(|v| v.as_str()).unwrap_or("");
                                let time_display = ts_str.get(11..19).unwrap_or("??:??:??");
                                panel.node_buffer.push_str(&format!("[{}] [{}] {}\n", time_display, level.to_uppercase(), message));
                            }
                        }
                    }
                }
            }
        }

        // Always update sidebar tool_call status regardless of DM mode
        if let Some(update) = detail.get("update") {
            let kind = update.get("sessionUpdate").and_then(|v| v.as_str());
            match kind {
                Some("tool_call") => {
                    let tc = update.get("toolCall").or_else(|| update.get("tool_call"));
                    let tool_name = if let Some(tc) = tc {
                        tc.get("name").and_then(|v| v.as_str()).unwrap_or("unknown")
                    } else if let Some(tn) = update.pointer("/_meta/claudeCode/toolName").and_then(|v| v.as_str()) {
                        tn
                    } else if let Some(title) = update.get("title").and_then(|v| v.as_str()) {
                        title.split(':').last().map(str::trim).unwrap_or(title)
                    } else {
                        "unknown"
                    };
                    info!(agent = name, tool = %tool_name, update = %update, "sidebar: tool_call raw");
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.name == name) {
                        agent.tool_call_name = Some(tool_name.to_string());
                        agent.tool_call_started = Some(std::time::Instant::now());
                    }
                }
                Some("tool_call_update") => {
                    let tcu = update.get("toolCallUpdate").or_else(|| update.get("tool_call_update"));
                    let status = if let Some(tcu) = tcu {
                        tcu.get("status").and_then(|v| v.as_str()).unwrap_or("")
                    } else {
                        update.get("status").and_then(|v| v.as_str()).unwrap_or("")
                    };
                    if status == "completed" || status == "failed" {
                        if let Some(agent) = self.agents.iter_mut().find(|a| a.name == name) {
                            agent.tool_call_name = None;
                            agent.tool_call_started = None;
                        }
                    }
                }
                _ => {}
            }
        }

        // Channel view: node.update should not render into message area.
        // Channel messages arrive via channel.message events only.
        // In DM mode: only process updates from the active DM node.
        if !in_dm {
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
                    self.dm_view.apply_streaming_event(name, "agent_message_chunk", update);
                }
                Some("agent_message_start") => {
                    debug!("agent_message_start from {}", name);
                    self.dm_view.start_streaming_message(name);
                    self.dm_view.flushed_agents.remove(name);
                }
                Some("agent_message_end") => {
                    // Live finalization path: take the streaming message's blocks
                    // and push as a completed DM message. Replay no longer goes
                    // through here — it's handled by NerveEvent::MessageSnapshot.
                    self.dm_view.apply_streaming_event(name, "agent_message_end", update);
                    let msg = self.dm_view.take_streaming_message(name);
                    self.dm_view.flushed_agents.remove(name);

                    let (final_content, final_blocks) = if let Some(m) = msg {
                        if !m.blocks.is_empty() {
                            let text = crate::components::dm_view::blocks_to_text(&m.blocks);
                            (text, m.blocks)
                        } else {
                            (String::new(), Vec::new())
                        }
                    } else {
                        (String::new(), Vec::new())
                    };

                    debug!(
                        "agent_message_end from {}: in_dm={} final={}",
                        name, in_dm, final_content.len()
                    );

                    if in_dm && !final_content.is_empty() {
                        let dm_msg = DmMessage {
                            role: "assistant".to_string(),
                            content: final_content,
                            timestamp: chrono::Local::now().timestamp(),
                        };
                        self.dm_view.push_with_blocks(&dm_msg, final_blocks);
                        self.dm_view.dm_history.push(dm_msg);
                    }
                    if self.dm_node_id() == Some(node_id) {
                        self.dm_view.set_responding(false);
                    }
                }
                Some("agent_thought_chunk") => {
                    self.dm_view.apply_streaming_event(name, "agent_thought_chunk", update);
                }
                Some("agent_thought_end") => {
                    self.dm_view.apply_streaming_event(name, "agent_thought_end", update);
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
                            self.dm_view.push(&dm_msg);
                            self.dm_view.dm_history.push(dm_msg);
                        }
                    }
                }
                Some("tool_call") => {
                    self.dm_view.apply_streaming_event(name, "tool_call", update);
                    // Sidebar update already handled above (before DM gate)
                }
                Some("tool_call_update") => {
                    self.dm_view.apply_streaming_event(name, "tool_call_update", update);
                    // Sidebar update already handled above (before DM gate)
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
                        self.dm_view.update_usage(used, size, cost);
                    }
                }
                Some("node_log") => {
                    if in_dm {
                        self.dm_view.push_log_entries(update);
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

    pub(crate) async fn join_channel(&mut self, channel_id: &str) {
        // Ensure DM is fully closed before entering channel view.
        // This guarantees node subscriptions are cleaned up — channel view
        // should only receive channel.message events, never node.update.
        if self.is_dm_mode() {
            self.exit_dm().await;
        }

        // Save current channel messages to cache before switching
        if let Some(ref old_id) = self.active_channel.clone() {
            if old_id != channel_id {
                self.channel_view.save_channel(old_id);
            }
        }

        match self.client.channel_join(channel_id).await {
            Ok(_) => {
                self.active_channel = Some(channel_id.to_string());
                if !self.is_dm_mode() {
                    self.view_mode = ViewMode::Channel { channel_id: channel_id.to_string() };
                }
                self.sync_navigation_selection();

                // Try to load from cache first
                if self.channel_view.load_channel(channel_id) {
                    self.channel_view
                        .push_system(&format!("已切换频道: {}", channel_id));
                } else {
                    // No cache — fetch from server
                    self.channel_view.clear();
                    self.channel_view
                        .push_system(&format!("已加入频道: {}", channel_id));
                    match self.client.channel_history(channel_id, Some(50)).await {
                        Ok(msgs) => {
                            for msg in &msgs {
                                let is_agent =
                                    self.agents.iter().any(|a| a.name == msg.from);
                                self.channel_view.push(msg, is_agent);
                            }
                        }
                        Err(e) => warn!("load history failed: {}", e),
                    }
                }

                // Refresh agents for this channel
                self.refresh_agents().await;
            }
            Err(e) => {
                self.channel_view.push_system(&format!("加入失败: {}", e));
            }
        }
    }

    pub(crate) async fn refresh_channels(&mut self) {
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
                        let unread = self.channel_view.unread_count(&ch.id);
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

    pub(crate) async fn refresh_agents(&mut self) {
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
                        model: n.model,
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

    pub(crate) fn update_completions(&mut self) {
        let mut completions: Vec<String> = vec![
            "/create".into(),
            "/join".into(),
            "/channels".into(),
            "/add".into(),
            "/stop".into(),
            "/cancel".into(),
            "/list".into(),
            "/ch".into(),
            "/dm".into(),
            "/back".into(),
            "/help".into(),
            "/restore".into(),
            "/clear".into(),
            "/compact".into(),
            "/split".into(),
            "/scene".into(),
            "/quit".into(),
        ];
        for agent in &self.agents {
            completions.push(format!("@{}", agent.name));
            completions.push(agent.name.clone());
        }
        for ch in &self.channels {
            if let Some(ref name) = ch.name {
                completions.push(format!("#{}", name));
            }
        }
        for a in &["claude", "c1", "c2", "codex", "gemini", "mock"] {
            completions.push(a.to_string());
        }
        self.input.completions = completions;
    }
}

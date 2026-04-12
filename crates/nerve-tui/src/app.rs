use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use futures_util::StreamExt;
use nerve_tui_core::Transport;
use nerve_tui_protocol::*;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::Frame;
use serde_json::Value;
use std::path::Path;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::clipboard;
use crate::components::channel_view::{self, ChannelPanelState, ChannelView};
use crate::components::dm_view::DmView;
use crate::components::*;
use crate::layout::AppLayout;

#[derive(Debug, Clone, Copy, PartialEq)]
enum SplitFocus {
    Dm,
    Panel(usize),
}

#[derive(Debug, Clone, PartialEq)]
enum SplitTarget {
    Channel,
    Node { node_id: String, node_name: String },
}

/// A single split panel with its own target, buffer, and scroll state.
#[derive(Debug, Clone, PartialEq)]
struct SplitPanel {
    target: SplitTarget,
    node_buffer: String,
    /// Whether an AI message is currently streaming (between first chunk and message_end).
    node_msg_pending: bool,
    panel_state: ChannelPanelState,
}

pub struct App<T: Transport> {
    pub client: T,
    event_rx: mpsc::UnboundedReceiver<NerveEvent>,

    // UI components — direct view fields (replaces MessagesView proxy)
    channel_view: ChannelView,
    dm_view: DmView,
    status_bar: StatusBar,
    input: InputBox,

    // Data
    channels: Vec<ChannelDisplay>,
    agents: Vec<AgentDisplay>,
    active_channel: Option<String>,
    should_quit: bool,

    /// Explicit view mode state machine.
    view_mode: ViewMode,

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
    split_panels: Vec<SplitPanel>,
    split_focus: SplitFocus,
    /// Cached x-coordinates of panel left boundaries (for mouse hit-testing in split view).
    panel_x_boundaries: Vec<u16>,
    /// Dirty flag — skip redraw if nothing changed since last frame.
    needs_redraw: bool,
    /// When true, clear terminal buffer before next draw (forces full repaint).
    force_clear: bool,
}

impl<T: Transport> App<T> {
    pub fn new(client: T, event_rx: mpsc::UnboundedReceiver<NerveEvent>) -> Self {
        Self::new_with_project(client, event_rx, None)
    }

    pub fn new_with_project(
        client: T,
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
            channel_view: ChannelView::new(),
            dm_view: DmView::inactive(),
            status_bar: StatusBar::new(),
            input: InputBox::new(),
            channels: Vec::new(),
            agents: Vec::new(),
            active_channel: None,
            should_quit: false,
            view_mode: ViewMode::Channel { channel_id: String::new() },
            project_path,
            project_name,
            global_mode: false,
            sidebar_visible: true,
            error_tx,
            error_rx,
            archived_channels: Vec::new(),
            split_panels: Vec::new(),
            split_focus: SplitFocus::Dm,
            panel_x_boundaries: Vec::new(),
            needs_redraw: true,
            force_clear: false,
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

    fn is_dm_mode(&self) -> bool {
        matches!(self.view_mode, ViewMode::Dm { .. })
    }

    fn is_split(&self) -> bool {
        !self.split_panels.is_empty()
    }

    fn split_panel_count(&self) -> usize {
        self.split_panels.len()
    }

    fn focused_panel_mut(&mut self) -> Option<&mut SplitPanel> {
        match self.split_focus {
            SplitFocus::Panel(i) => self.split_panels.get_mut(i),
            _ => None,
        }
    }

    /// Clamp split_focus to a valid index after panels are removed.
    fn clamp_split_focus(&mut self) {
        if let SplitFocus::Panel(i) = self.split_focus {
            if i >= self.split_panels.len() {
                self.split_focus = if self.split_panels.is_empty() {
                    SplitFocus::Dm
                } else {
                    SplitFocus::Panel(self.split_panels.len() - 1)
                };
            }
        }
    }

    /// Close all split panels, unsubscribing from any node targets.
    async fn close_all_split_panels(&mut self) {
        for panel in &self.split_panels {
            if let SplitTarget::Node { ref node_id, .. } = panel.target {
                let _ = self.client.node_unsubscribe(node_id).await;
            }
        }
        self.split_panels.clear();
        self.split_focus = SplitFocus::Dm;
    }

    /// Close a single split panel by index, unsubscribing if it targets a node.
    async fn close_split_panel(&mut self, index: usize) {
        if index < self.split_panels.len() {
            if let SplitTarget::Node { ref node_id, .. } = self.split_panels[index].target {
                let id = node_id.clone();
                let _ = self.client.node_unsubscribe(&id).await;
            }
            self.split_panels.remove(index);
            self.clamp_split_focus();
        }
    }

    fn dm_node_id(&self) -> Option<&str> {
        match &self.view_mode {
            ViewMode::Dm { node_id, .. } => Some(node_id.as_str()),
            _ => None,
        }
    }

    fn dm_node_name(&self) -> Option<&str> {
        match &self.view_mode {
            ViewMode::Dm { node_name, .. } => Some(node_name.as_str()),
            _ => None,
        }
    }

    /// Scroll the currently active view (DM or channel).
    fn scroll_active_up(&mut self, n: u16) {
        if self.is_dm_mode() {
            self.dm_view.scroll_up(n);
        } else {
            self.channel_view.scroll_up(n);
        }
    }

    fn scroll_active_down(&mut self, n: u16) {
        if self.is_dm_mode() {
            self.dm_view.scroll_down(n);
        } else {
            self.channel_view.scroll_down(n);
        }
    }

    fn page_active_up(&mut self) {
        if self.is_dm_mode() {
            self.dm_view.page_up();
        } else {
            self.channel_view.page_up();
        }
    }

    fn page_active_down(&mut self) {
        if self.is_dm_mode() {
            self.dm_view.page_down();
        } else {
            self.channel_view.page_down();
        }
    }

    fn snap_active_to_bottom(&mut self) {
        if self.is_dm_mode() {
            self.dm_view.snap_to_bottom();
        } else {
            self.channel_view.snap_to_bottom();
        }
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

    fn render(&mut self, frame: &mut Frame) {
        // Advance blink tick for streaming cursor
        self.dm_view.tick_blink();

        let area = frame.area();
        let panel_count = self.split_panels.len();
        let input_inner_w = AppLayout::input_inner_width(area, self.sidebar_visible, panel_count);
        let input_lines = self.input.visual_line_count(input_inner_w) + 2;
        let layout = AppLayout::build(area, input_lines, self.sidebar_visible, panel_count);

        // Sidebar: channels + agents (skip when hidden)
        if self.sidebar_visible {
            self.status_bar.render(
                &self.channels,
                self.active_channel.as_deref(),
                &self.agents,
                self.dm_node_name(),
                self.project_name.as_deref(),
                self.global_mode,
                layout.sidebar,
                frame.buffer_mut(),
            );
        }

        // Messages (DM panel in split mode)
        if self.is_dm_mode() {
            self.dm_view.render(layout.messages, frame.buffer_mut());
        } else {
            self.channel_view.render(layout.messages, frame.buffer_mut());
        }

        // Right panels (split view): channel or node output
        self.panel_x_boundaries.clear();
        for (i, panel_area) in layout.panels.iter().enumerate() {
            self.panel_x_boundaries.push(panel_area.x);
            if let Some(panel) = self.split_panels.get_mut(i) {
                let focused = self.split_focus == SplitFocus::Panel(i);
                match &panel.target {
                    SplitTarget::Channel => {
                        let channel_name = self
                            .channels
                            .iter()
                            .find(|c| Some(&c.id) == self.active_channel.as_ref())
                            .map(|c| c.display_name())
                            .unwrap_or("channel");
                        self.channel_view.render_panel(
                            channel_name,
                            &mut panel.panel_state,
                            focused,
                            *panel_area,
                            frame.buffer_mut(),
                        );
                    }
                    SplitTarget::Node { node_name, .. } => {
                        let title = format!("@{}", node_name);
                        let buf = &panel.node_buffer;
                        channel_view::render_text_panel(
                            &title,
                            buf,
                            &mut panel.panel_state,
                            focused,
                            *panel_area,
                            frame.buffer_mut(),
                        );
                    }
                }
            }
        }

        // Input
        self.input.render(layout.input, frame.buffer_mut());
        self.input.render_popup(layout.input, frame.buffer_mut());

        // DM status indicator on input box border
        if self.is_dm_mode() {
            let status = if self.dm_view.is_responding {
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

    fn handle_mouse(&mut self, mouse: MouseEvent) {
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
    fn mouse_panel_index(&self, column: u16) -> Option<usize> {
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

    fn sync_navigation_selection(&mut self) {
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

    async fn confirm_selected_navigation(&mut self) {
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

    async fn handle_input(&mut self, text: &str) {
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
            let user_msg = DmMessage {
                role: "user".to_string(),
                content: text.to_string(),
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
    /// Uses view_mode to route — will replace push_contextual_system in Step 4b.
    fn push_system_to_active(&mut self, content: &str) {
        match &self.view_mode {
            ViewMode::Channel { .. } => self.channel_view.push_system(content),
            ViewMode::Dm { .. } => self.dm_view.push_system(content),
        }
    }

    fn push_contextual_system(&mut self, content: &str) {
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

    fn reset_dm_before_enter(&mut self, next_node_id: &str) -> Option<String> {
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

    async fn enter_dm(&mut self, agent_name: &str) {
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

    async fn exit_dm(&mut self) {
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

    async fn handle_command(&mut self, text: &str) {
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

    async fn handle_nerve_event(&mut self, event: NerveEvent) {
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
    fn flush_streaming_as_dm(&mut self, node_id: &str, name: &str) {
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

    fn handle_node_update(&mut self, node_id: &str, name: &str, detail: &serde_json::Value) {
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

    async fn join_channel(&mut self, channel_id: &str) {
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

    fn update_completions(&mut self) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_tui_core::mock::MockTransport;
    use ratatui::layout::Rect;

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
        // Bug: after output finishes (flush), thinking/tool_call blocks should
        // default to collapsed (summary_mode=true), but currently they stay expanded.
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.dm_view.is_responding = true;
        app.dm_view.summary_mode = false; // starts expanded (during streaming)
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        // Simulate a streaming message with thinking + text blocks
        app.dm_view.start_streaming_message("alice");
        let think = serde_json::json!({ "content": { "text": "reasoning..." } });
        app.dm_view.apply_streaming_event("alice", "agent_thought_chunk", &think);
        let text = serde_json::json!({ "content": { "text": "answer" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &text);

        // Flush — output is done
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

        // visible: Channel(ch1), Channel(ch2), SectionHeader, Agent
        // ch2 is at visible index 1
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

        // visible: Channel(ch1), SectionHeader("AI Agents"), Agent(alice), Agent(bob)
        // bob is Agent(1) at visible index 3
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
        let transport = MockTransport::new("test-user");
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        let mut app = App::new_with_project(transport, event_rx, Some("/tmp/project".into()));

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

    // --- ChannelMention in DM mode tests ---

    #[tokio::test]
    async fn dm_mode_does_not_swallow_other_channel_mention() {
        let mut app = make_app();
        // Simulate being in DM mode
        app.dm_view = DmView::new("bob");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "bob".into() };
        // Active channel is ch1
        app.active_channel = Some("ch1".into());

        let initial_count = app.channel_view.line_count();

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
            app.channel_view.line_count(),
            initial_count,
            "active channel mention should be deduped"
        );
    }

    // --- Streaming pipeline unification tests ---

    #[test]
    fn flush_streaming_from_structured_message() {
        // flush should take content from streaming_messages, not the old Vec
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.dm_view.is_responding = true;
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        // Populate streaming_messages with structured blocks
        app.dm_view.start_streaming_message("alice");
        let update = serde_json::json!({ "content": { "text": "hello world" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &update);

        app.flush_streaming_as_dm("n1", "alice");

        assert!(!app.dm_view.is_responding);
        assert_eq!(app.dm_view.dm_history.len(), 1);
        assert!(app.dm_view.dm_history[0].content.contains("hello world"));
        // streaming_messages should be cleared
        assert!(!app.dm_view.streaming_messages.contains_key("alice"));
    }

    #[test]
    fn flush_empty_streaming_messages_no_panic() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        // No streaming_messages at all — should not panic or create empty dm_history
        app.flush_streaming_as_dm("n1", "alice");

        assert!(app.dm_view.dm_history.is_empty());
    }

    #[test]
    fn flush_structured_blocks_no_thinking_in_content() {
        // Thinking blocks should not appear in persisted dm_history.content
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        app.dm_view.start_streaming_message("alice");
        // Add thinking block
        let think = serde_json::json!({ "content": { "text": "let me think..." } });
        app.dm_view.apply_streaming_event("alice", "agent_thought_chunk", &think);
        // Add text block
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
        // Add a tool_call event
        let tc = serde_json::json!({
            "toolCall": { "name": "Read", "id": "tc1", "input": {} },
        });
        app.dm_view.apply_streaming_event("alice", "tool_call", &tc);
        // Mark completed
        let tcu = serde_json::json!({
            "toolCallUpdate": { "toolCallId": "tc1", "status": "completed", "result": { "value": "ok" } }
        });
        app.dm_view.apply_streaming_event("alice", "tool_call_update", &tcu);
        // Add text
        let text = serde_json::json!({ "content": { "text": "done" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &text);

        app.flush_streaming_as_dm("n1", "alice");

        assert_eq!(app.dm_view.dm_history.len(), 1);
        let content = &app.dm_view.dm_history[0].content;
        assert!(content.contains("done"));
        // Should have tool reference with status marker
        assert!(content.contains("[tool:Read"), "should have tool call in content");
    }

    #[test]
    fn flush_preserves_blocks_in_message_line() {
        // After flush, the MessageLine pushed to messages should have structured blocks
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
        // blocks should contain at least one Text block
        assert!(
            last_msg.blocks.iter().any(|b| matches!(b, ContentBlock::Text { .. })),
            "MessageLine should have structured Text blocks"
        );
    }

    // --- Ctrl+L force redraw tests ---

    #[tokio::test]
    async fn ctrl_l_sets_force_clear() {
        let mut app = make_app();
        app.force_clear = false;

        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)).await;

        assert!(app.force_clear, "Ctrl+L should set force_clear = true");
        assert!(app.needs_redraw, "Ctrl+L should also set needs_redraw = true");
    }

    // --- /ch channel switch tests ---

    #[test]
    fn ch_command_finds_channel_by_name() {
        // Test channel lookup logic without network (join_channel is async + needs server)
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

        // Find by name
        let found = channels.iter().find(|c| c.name.as_deref() == Some("ops") || c.id == "ops");
        assert_eq!(found.unwrap().id, "ch2");

        // Find by id
        let found = channels.iter().find(|c| c.name.as_deref() == Some("ch1") || c.id == "ch1");
        assert_eq!(found.unwrap().id, "ch1");

        // Not found
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

        // strip_prefix("/ch ") should yield "team sync"
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

        // Should show error message
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

        // First message
        app.dm_view.start_streaming_message("alice");
        let text = serde_json::json!({ "content": { "text": "old content" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &text);

        // New message_start should clear previous
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

        // Flush only alice
        app.flush_streaming_as_dm("n1", "alice");

        assert_eq!(app.dm_view.dm_history.len(), 1);
        assert!(app.dm_view.dm_history[0].content.contains("from alice"));
        // Bob's streaming should still be there
        assert!(app.dm_view.streaming_messages.contains_key("bob"));
        assert!(!app.dm_view.streaming_messages.contains_key("alice"));
    }

    // --- clipboard image paste tests ---

    #[test]
    fn paste_text_fallback_inserts_directly() {
        // When no clipboard image, text should be inserted via insert_str
        let mut app = make_app();
        // Directly test the text fallback path (skip clipboard check)
        app.input.insert_str("hello world");
        assert!(
            app.input.text.contains("hello world"),
            "text should be inserted: '{}'",
            app.input.text
        );
    }

    // --- tool_call name extraction tests ---

    #[test]
    fn tool_name_from_title_extracts_after_colon() {
        // Simulate ACP title format: "tool: Read"
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
        // MCP tools may have format "mcp: server_name: tool_name"
        let title = "mcp: nerve: nerve_post";
        let extracted = title.split(':').last().map(str::trim).unwrap_or(title);
        assert_eq!(extracted, "nerve_post");
    }

    // --- Split Step 1: SplitPanel data structure migration tests ---

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
        // Panel(0) should be the equivalent of the old SplitFocus::Channel
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

        // Mutate panel 0's state
        app.split_panels[0].panel_state.auto_scroll = false;
        app.split_panels[0].panel_state.scroll_offset = 42;

        // Panel 1 should be unaffected
        assert!(app.split_panels[1].panel_state.auto_scroll);
        assert_eq!(app.split_panels[1].panel_state.scroll_offset, u16::MAX);
    }

    // --- Split Step 3: render multi-panel logic tests ---

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
        let mut app = make_app();
        // Simulate 2 panels
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_panels.push(make_split_panel(SplitTarget::Node {
            node_id: "n1".into(),
            node_name: "alice".into(),
        }));

        // Build layout with 2 panels
        let area = Rect::new(0, 0, 120, 30);
        let layout = AppLayout::build(area, 3, true, 2);

        // Simulate what render() does: populate panel_x_boundaries
        app.panel_x_boundaries.clear();
        for panel_area in &layout.panels {
            app.panel_x_boundaries.push(panel_area.x);
        }

        assert_eq!(app.panel_x_boundaries.len(), 2);
        // Boundaries should be increasing (left to right)
        assert!(app.panel_x_boundaries[0] < app.panel_x_boundaries[1]);
    }

    #[test]
    fn panel_x_boundaries_matches_layout_panels_count() {
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

        // Focus on panel 1
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
        // No panels → always None
        assert!(app.mouse_panel_index(50).is_none());
        assert!(app.mouse_panel_index(0).is_none());
    }

    #[test]
    fn mouse_panel_index_single_panel() {
        let mut app = make_app();
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.panel_x_boundaries = vec![60]; // panel starts at x=60

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
        app.panel_x_boundaries = vec![40, 70]; // panel 0 at x=40, panel 1 at x=70

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

        // Write to each panel's buffer independently
        app.split_panels[0].node_buffer.push_str("alice output");
        app.split_panels[1].node_buffer.push_str("bob output");

        assert_eq!(app.split_panels[0].node_buffer, "alice output");
        assert_eq!(app.split_panels[1].node_buffer, "bob output");
        // Verify target identity
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

    // --- Split Step 4: keyboard interaction tests ---

    /// Helper: simulate the Ctrl+W focus cycling logic (extracted from handle_key).
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

        // One more round to confirm stable cycle
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

        // Simulate scroll on focused panel
        if let Some(panel) = app.focused_panel_mut() {
            panel.panel_state.scroll_up(5);
        }

        // Panel 1 should be scrolled
        assert_ne!(app.split_panels[1].panel_state.scroll_offset, u16::MAX);
        // Panel 0 should be untouched
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

        // Remove last panel (simulating node stop)
        app.split_panels.remove(2);

        // Focus should clamp: Panel(2) out of bounds → Panel(1) (last valid index)
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
        let mut app = make_app();
        // Enter DM mode so Ctrl+W path is exercised
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };
        // No split panels — Ctrl+W should delete word, not cycle focus
        app.input.insert_str("hello world");

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)).await;

        // "world" should be deleted
        assert!(!app.input.text.contains("world"), "Ctrl+W without panels should delete word, got: {}", app.input.text);
    }

    // --- Split Step 5: /split command extension tests ---

    /// Helper: set up app in DM mode with an active channel.
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
        // Test state change directly (mock client can't subscribe over WS)
        let mut app = make_dm_app();

        // Simulate what /split @alice does on successful subscribe:
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
        // Pre-populate with alice panel
        app.split_panels.push(SplitPanel {
            target: SplitTarget::Node { node_id: "n1".into(), node_name: "alice".into() },
            node_buffer: String::new(),
            node_msg_pending: false,
            panel_state: ChannelPanelState::new(),
        });
        app.split_panels.push(make_split_panel(SplitTarget::Channel));
        app.split_focus = SplitFocus::Dm;

        app.handle_command("/split @alice").await;

        // Should NOT add a new panel — still 2
        assert_eq!(app.split_panels.len(), 2, "duplicate @alice should not add panel");
        // Focus should move to existing alice panel (index 0)
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
        // Focus should clamp: Panel(1) removed, fall to Panel(0)
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

        // Split panels should survive DM exit
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

        // panel_count should include split panels even in channel mode
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

        // Split panels should survive DM-to-DM switch
        assert_eq!(app.split_panels.len(), 1, "split panels should persist after DM switch");
    }

    #[tokio::test]
    async fn split_panel_limit_rejects_fifth() {
        let mut app = make_dm_app();
        for _ in 0..4 {
            app.split_panels.push(make_split_panel(SplitTarget::Channel));
        }
        assert_eq!(app.split_panels.len(), 4);

        // Try adding a 5th panel — should be rejected
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

        // Should add a channel panel (may need new SplitTarget variant or field)
        assert_eq!(app.split_panels.len(), 1, "/split #ops should add a panel");
    }

    // --- Split Step 6: node_log event routing tests ---

    /// Build a node update detail JSON for agent_message_chunk.
    fn node_update_chunk(text: &str) -> serde_json::Value {
        serde_json::json!({
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "text": text }
            }
        })
    }

    /// Build a node update detail JSON for agent_message_end.
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

        // node_buffer now has a role+timestamp header before content
        assert!(app.split_panels[0].node_buffer.contains("hello from alice"));
        assert!(app.split_panels[0].node_buffer.starts_with("assistant"));
    }

    #[test]
    fn node_log_routes_to_both_panels_subscribing_same_node() {
        let mut app = make_app();
        // Two panels both subscribe to node A
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

        // Event for node n2 — no panel subscribes
        let detail = node_update_chunk("orphan log");
        app.handle_node_update("n2", "bob", &detail);

        // Panel 0 should be untouched
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

        // auto_scroll should still be true (not manually scrolled)
        assert!(app.split_panels[0].panel_state.auto_scroll);
    }

    // --- Task 4c: /ch channel name completion in app ---

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

    // --- Task 4d-1: Ctrl+L already tested at ctrl_l_sets_force_clear ---
    // (existing test at line ~2609 covers this)

    // --- Task 4d: summary mode toggle ---

    #[tokio::test]
    async fn ctrl_e_toggles_summary_mode() {
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

        // Start streaming — should work regardless of summary_mode
        app.dm_view.start_streaming_message("alice");
        let update = serde_json::json!({ "content": { "text": "streaming text" } });
        app.dm_view.apply_streaming_event("alice", "agent_message_chunk", &update);

        assert!(
            app.dm_view.streaming_messages.contains_key("alice"),
            "streaming should work in summary mode"
        );
    }

    // --- Task 4e: Input history integration tests ---

    #[tokio::test]
    async fn send_message_pushes_to_input_history() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };

        // Simulate typing and sending via handle_input (not manual history_push)
        app.input.insert_str("hello world");
        let text = app.input.take();
        app.handle_input(&text).await;

        // handle_input should internally call history_push
        assert_eq!(app.input.history_len(), 1);
    }

    #[test]
    fn up_arrow_single_line_triggers_history() {
        let mut app = make_app();
        app.input.history_push("previous message");

        // Single-line input, not multiline — Up should trigger history
        assert!(!app.input.is_multiline());
        assert!(app.input.history_up());
        assert_eq!(app.input.text, "previous message");
    }

    #[test]
    fn up_arrow_multiline_does_not_trigger_history() {
        let mut app = make_app();
        app.input.history_push("old");
        app.input.insert_str("line1\nline2");

        // Multiline — move_up should handle cursor, not history
        assert!(app.input.is_multiline());
        assert!(app.input.move_up()); // cursor moves within text
        assert_eq!(app.input.text, "line1\nline2"); // text unchanged
    }

    #[test]
    fn down_arrow_after_history_up_restores() {
        let mut app = make_app();
        app.input.history_push("msg1");
        app.input.history_push("msg2");

        app.input.history_up(); // "msg2"
        app.input.history_up(); // "msg1"
        assert_eq!(app.input.text, "msg1");

        app.input.history_down(); // "msg2"
        assert_eq!(app.input.text, "msg2");

        app.input.history_down(); // back to empty draft
        assert_eq!(app.input.text, "");
    }

    // --- Bug 5b: Split panel should render AI node messages with role labels and timestamps ---

    #[test]
    fn split_node_panel_render_includes_role_and_timestamp() {
        // Bug 5b: /split @node right panel only shows raw text,
        // missing role labels (user/assistant) and timestamps that DM view has.
        //
        // The split panel uses render_text_panel() which treats node_buffer as plain
        // text. It should render structured messages with role headers like DM view.
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

        // Simulate AI node producing a message via agent_message_chunk + agent_message_end
        let chunk = node_update_chunk("Hello from assistant");
        app.handle_node_update("n1", "alice", &chunk);
        let end_detail = serde_json::json!({
            "update": { "sessionUpdate": "agent_message_end" }
        });
        app.handle_node_update("n1", "alice", &end_detail);

        // Now render the split panel into a test buffer
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

        // Extract all text from the rendered buffer
        let rendered: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_default())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        // The rendered content (excluding the border title) should contain a role label
        // AND a timestamp, like DM view does. Currently render_text_panel just dumps raw
        // text, so this WILL FAIL (red).
        //
        // We check the inner content lines (skip first/last border rows) for role+timestamp.
        let inner_lines: Vec<&str> = rendered.lines().skip(1).collect();
        let inner_text = inner_lines.join("\n");

        // Must contain a timestamp pattern (HH:MM:SS) in the content area
        let has_timestamp = regex::Regex::new(r"\d{2}:\d{2}:\d{2}")
            .unwrap()
            .is_match(&inner_text);
        assert!(
            has_timestamp,
            "split panel should show timestamp like DM view, but inner content:\n{}",
            inner_text
        );
    }

    /// Bug 5a: node_log events should route to split panel's node_buffer,
    /// not just the DM view. Currently, node_log is only handled inside
    /// the `if in_dm` gate and never reaches split panels.
    #[test]
    fn node_log_routes_to_split_panel_node_buffer() {
        let mut app = make_app();

        // We are in channel view (NOT DM mode) with a split panel targeting node "n1"
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

        // Simulate a node_log event arriving for node "n1"
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

        // The split panel's node_buffer should contain the log entry
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

        // visible: Channel(0), SectionHeader("AI Agents"), Agent(0), Agent(1) = 4 items
        assert_eq!(app.status_bar.nav_count(&app.channels, &app.agents), 4);
        assert!(!app.status_bar.collapsed_sections.contains("AI Agents"));

        // Navigate to section header (index 1)
        app.status_bar.selected_nav = 1;

        // Pressing Enter on section header should toggle collapse
        app.confirm_selected_navigation().await;
        assert!(app.status_bar.collapsed_sections.contains("AI Agents"),
                "Enter on SectionHeader should collapse the section");

        // After collapse: Channel(0), SectionHeader("AI Agents") = 2 items
        assert_eq!(app.status_bar.nav_count(&app.channels, &app.agents), 2);

        // Toggle again to expand
        app.confirm_selected_navigation().await;
        assert!(!app.status_bar.collapsed_sections.contains("AI Agents"),
                "Enter again should expand the section");
        assert_eq!(app.status_bar.nav_count(&app.channels, &app.agents), 4);
    }

    // ── Render tests (Phase 3) ──────────────────────────────────

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Extract all text content from a ratatui Buffer as a single string.
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

    /// Like buffer_text but collapse all whitespace — useful for matching CJK text
    /// where full-width chars leave filler cells rendered as spaces.
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

        // When sidebar is hidden, the Messages border should start at x=0
        // When visible, it starts further right. We check that "Messages" appears
        // earlier (at a smaller x position) when sidebar is hidden.
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

        // The input area renders a bordered block at the bottom of the screen.
        // Check that the bottom rows contain border characters (─ or │).
        let buf = terminal.backend().buffer();
        let area = buf.area;
        let bottom_y = area.y + area.height - 1;
        let mut bottom_row = String::new();
        for x in area.x..area.x + area.width {
            bottom_row.push_str(buf[(x, bottom_y)].symbol());
        }
        // Input box border uses box-drawing characters
        assert!(
            bottom_row.contains('─') || bottom_row.contains('└') || bottom_row.contains('┘'),
            "bottom row should contain input area border characters, got: {}",
            bottom_row
        );
    }
}

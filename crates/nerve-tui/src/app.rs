use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use nerve_tui_core::Transport;
use crate::buffer::{BufferId, BufferContent, BufferPool, WindowLayout, Window, WindowFocus};
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

/// Per-panel rendering metadata (parallel to layout.panels).
#[derive(Debug, Clone)]
struct PanelMeta {
    /// Scroll/viewport state for rendering
    state: ChannelPanelState,
    /// Display label (node_name for Node panels, empty for Channel panels)
    label: String,
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

    // Split view — BufferPool + WindowLayout (replaces old split_panels/split_focus)
    buffer_pool: BufferPool,
    layout: WindowLayout,
    panel_meta: Vec<PanelMeta>,
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
            buffer_pool: BufferPool::new(),
            layout: WindowLayout::new(Window::new(BufferId::Channel { channel_id: String::new() }, 0)),
            panel_meta: Vec::new(),
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
        self.layout.panel_count() > 0
    }


    /// Add a split panel (Window + PanelMeta kept in sync).
    fn add_panel(&mut self, buffer_id: BufferId, label: String) {
        self.buffer_pool.get_or_create(buffer_id.clone());
        let window = Window::new(buffer_id, 0);
        self.layout.add_panel(window);
        let mut meta = PanelMeta { state: ChannelPanelState::new(), label };
        meta.state.snap_to_bottom();
        self.panel_meta.push(meta);
    }

    /// Remove a specific panel by index, keeping layout + meta in sync.
    fn remove_panel(&mut self, index: usize) {
        if index < self.layout.panel_count() {
            self.layout.remove_panel(index);
            self.panel_meta.remove(index);
        }
    }

    /// Remove all panels, reset focus to Primary.
    fn clear_panels(&mut self) {
        self.layout.clear_panels();
        self.panel_meta.clear();
    }

    /// Unsubscribe from all NodeLog/Dm panels, then clear.
    async fn unsubscribe_and_clear_panels(&mut self) {
        for panel in &self.layout.panels {
            match &panel.buffer_id {
                BufferId::NodeLog { ref node_id } | BufferId::Dm { ref node_id } => {
                    let _ = self.client.node_unsubscribe(node_id).await;
                }
                _ => {}
            }
        }
        self.clear_panels();
    }

    /// Get the focused panel's PanelMeta (for scroll operations).
    fn focused_panel_meta_mut(&mut self) -> Option<&mut PanelMeta> {
        match self.layout.focus {
            WindowFocus::Panel(i) => self.panel_meta.get_mut(i),
            _ => None,
        }
    }

    /// Get node_buffer text for a panel (read from buffer_pool).
    fn panel_node_buffer(&self, index: usize) -> &str {
        if let Some(panel) = self.layout.panels.get(index) {
            if let Some(entry) = self.buffer_pool.get(&panel.buffer_id) {
                if let BufferContent::NodeLog { ref text, .. } = entry.content {
                    return text.as_str();
                }
            }
        }
        ""
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

    /// Returns &DmView if the given node_id matches the current DM target.
    fn dm_view_for_node(&self, node_id: &str) -> Option<&DmView> {
        if self.dm_node_id() == Some(node_id) {
            Some(&self.dm_view)
        } else {
            None
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
        event_source: &mut impl crate::event_source::EventSource,
    ) -> Result<()> {
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
                result = event_source.next_event() => {
                    match result? {
                        Some(evt) => {
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
                        None => break, // event source exhausted
                    }
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
        let panel_count = if self.is_dm_mode() { self.layout.panel_count() } else { 0 };
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

        // Pre-build DM panel lines (&self phase, before mutable render)
        let dm_panel_lines: Vec<Option<Vec<ratatui::text::Line<'static>>>> = self.layout.panels.iter().enumerate().map(|(i, panel)| {
            match &panel.buffer_id {
                BufferId::Dm { ref node_id } => {
                    let width = layout.panels.get(i).map(|a| a.width.saturating_sub(2)).unwrap_or(80);
                    if let Some(dm) = self.dm_view_for_node(node_id) {
                        Some(dm.build_text(width))
                    } else {
                        // DM target mismatch
                        Some(vec![ratatui::text::Line::from(Span::styled(
                            "已切换到其他对话",
                            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                        ))])
                    }
                }
                _ => None,
            }
        }).collect();

        // Right panels (split view): channel, node output, or DM
        self.layout.panel_x_boundaries.clear();
        for (i, panel_area) in layout.panels.iter().enumerate() {
            self.layout.panel_x_boundaries.push(panel_area.x);
            if i < self.layout.panels.len() {
                let focused = self.layout.focus == WindowFocus::Panel(i);
                match &self.layout.panels[i].buffer_id {
                    BufferId::Channel { .. } => {
                        let channel_name = self
                            .channels
                            .iter()
                            .find(|c| Some(&c.id) == self.active_channel.as_ref())
                            .map(|c| c.display_name())
                            .unwrap_or("channel");
                        self.channel_view.render_panel(
                            channel_name,
                            &mut self.panel_meta[i].state,
                            focused,
                            *panel_area,
                            frame.buffer_mut(),
                        );
                    }
                    BufferId::NodeLog { .. } => {
                        let title = format!("@{}", self.panel_meta[i].label);
                        let buf = self.panel_node_buffer(i).to_string();
                        channel_view::render_text_panel(
                            &title,
                            &buf,
                            &mut self.panel_meta[i].state,
                            focused,
                            *panel_area,
                            frame.buffer_mut(),
                        );
                    }
                    BufferId::Dm { .. } => {
                        let title = format!("@{}", self.panel_meta[i].label);
                        if let Some(Some(lines)) = dm_panel_lines.get(i) {
                            channel_view::render_dm_panel(
                                &title,
                                lines.clone(),
                                &mut self.panel_meta[i].state,
                                focused,
                                *panel_area,
                                frame.buffer_mut(),
                            );
                        }
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
                        || self.layout.panels.iter().any(|p| matches!(p.buffer_id, BufferId::NodeLog { .. } | BufferId::Dm { .. }));
                    if has_target {
                        if self.is_split() {
                            self.unsubscribe_and_clear_panels().await;
                        } else {
                            self.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
                        }
                    } else {
                        self.push_system_to_active("需要先加入频道才能分屏");
                    }
                }
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.is_split() && self.is_dm_mode() {
                    self.layout.cycle_focus_forward();
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
                } else if let Some(panel) = self.focused_panel_meta_mut() {
                    panel.state.scroll_up(1);
                } else {
                    self.scroll_active_up(1);
                }
            }
            KeyCode::Down if key.modifiers.is_empty() => {
                if self.input.is_multiline() && self.input.move_down() {
                    // Moved cursor down within input
                } else if !self.input.is_multiline() && self.input.history_down() {
                    // Browsing history in single-line mode
                } else if let Some(panel) = self.focused_panel_meta_mut() {
                    panel.state.scroll_down(1);
                } else {
                    self.scroll_active_down(1);
                }
            }

            // Scroll messages (dispatched to focused panel in split mode)
            KeyCode::PageUp => {
                if let Some(panel) = self.focused_panel_meta_mut() {
                    panel.state.page_up();
                } else {
                    self.page_active_up();
                }
            }
            KeyCode::PageDown => {
                if let Some(panel) = self.focused_panel_meta_mut() {
                    panel.state.page_down();
                } else {
                    self.page_active_down();
                }
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(panel) = self.focused_panel_meta_mut() {
                    panel.state.scroll_down(1);
                } else {
                    self.scroll_active_down(1);
                }
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(panel) = self.focused_panel_meta_mut() {
                    panel.state.scroll_down(10);
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
                } else if self.is_split() && matches!(self.layout.focus, WindowFocus::Panel(_)) {
                    self.layout.focus = WindowFocus::Primary;
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
                    self.panel_meta[idx].state.scroll_up(3);
                } else {
                    self.scroll_active_up(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(idx) = self.mouse_panel_index(mouse.column) {
                    self.panel_meta[idx].state.scroll_down(3);
                } else {
                    self.scroll_active_down(3);
                }
            }
            _ => {}
        }
    }

    /// Find which split panel the mouse column falls in (if any).
    fn mouse_panel_index(&self, column: u16) -> Option<usize> {
        if self.layout.panel_x_boundaries.is_empty() {
            return None;
        }
        // Find the rightmost boundary that the column is >= to
        let mut result = None;
        for (i, &bx) in self.layout.panel_x_boundaries.iter().enumerate() {
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

        // Initialize usage and model display from agent snapshot
        if let Some(agent) = self.agents.iter().find(|a| a.name == node_name) {
            if let Some((used, size, cost)) = agent.usage {
                self.dm_view.update_usage(used, size, cost);
            }
            self.dm_view.update_model(agent.model.clone());
        }

        self.sync_navigation_selection();
    }

    async fn exit_dm(&mut self) {
        if let ViewMode::Dm { ref node_id, ref node_name } = self.view_mode.clone() {
            debug!("exiting DM with {}", node_name);
            if let Err(e) = self.client.node_unsubscribe(node_id).await {
                warn!("unsubscribe failed: {}", e);
            }
            // Clean up split node subscriptions
            self.unsubscribe_and_clear_panels().await;
            self.dm_view.clear();
            self.view_mode = ViewMode::Channel {
                channel_id: self.active_channel.clone().unwrap_or_default(),
            };
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
        self.dm_view.set_responding(false);
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
                            self.dm_view.set_responding(false);
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
                    // /split close all — remove all panels
                    self.unsubscribe_and_clear_panels().await;
                } else if arg == "close" {
                    // /split close — remove focused panel
                    if let WindowFocus::Panel(i) = self.layout.focus {
                        if i < self.layout.panel_count() {
                            match &self.layout.panels[i].buffer_id {
                                BufferId::NodeLog { ref node_id } | BufferId::Dm { ref node_id } => {
                                    let id = node_id.clone();
                                    let _ = self.client.node_unsubscribe(&id).await;
                                }
                                _ => {}
                            }
                            self.remove_panel(i);
                        }
                    } else {
                        self.push_contextual_system("焦点不在面板上，用 Ctrl+W 切换焦点");
                    }
                } else if arg.starts_with('@') {
                    // /split @agent-name: show agent output in right panel
                    let agent_name = &arg[1..];

                    // Dedup: if agent already has a panel, just focus it
                    if let Some(idx) = self.panel_meta.iter().position(|m| m.label == agent_name) {
                        self.layout.focus = WindowFocus::Panel(idx);
                        self.push_contextual_system(&format!("已聚焦 @{}", agent_name));
                    } else if self.layout.panel_count() >= 4 {
                        self.push_contextual_system("面板已满（最多 4 个）");
                    } else if let Some(agent) = self.agents.iter().find(|a| a.name == agent_name) {
                        let node_id = agent.node_id.clone();
                        let node_name = agent.name.clone();
                        // Subscribe to target node
                        if let Err(e) = self.client.node_subscribe(&node_id).await {
                            self.push_contextual_system(&format!("subscribe 失败: {}", e));
                        } else {
                            self.add_panel(BufferId::Dm { node_id }, node_name.clone());
                            self.layout.focus = WindowFocus::Primary;
                            self.push_contextual_system(&format!("分屏查看 @{}", node_name));
                        }
                    } else {
                        self.push_contextual_system(&format!("找不到 agent: {}", agent_name));
                    }
                } else if arg.starts_with('#') {
                    // /split #channel — add a channel panel
                    let _channel_name = &arg[1..];
                    if self.layout.panel_count() >= 4 {
                        self.push_contextual_system("面板已满（最多 4 个）");
                    } else {
                        self.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
                    }
                } else if self.is_dm_mode() {
                    if self.is_split() && arg.is_empty() {
                        // Toggle off — unsubscribe from split nodes
                        self.unsubscribe_and_clear_panels().await;
                    } else if self.active_channel.is_some() {
                        if self.layout.panel_count() >= 4 {
                            self.push_contextual_system("面板已满（最多 4 个）");
                        } else {
                            self.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
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
                // Only show messages that @mention me or that I sent.
                // Token-level match: @name must be preceded by whitespace and followed
                // by whitespace, punctuation, or EOF (mirrors server router.ts:parseMentions).
                let dominated = message.from == self.client.node_name()
                    || mentions_name(&message.content, self.client.node_name());
                if dominated {
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
                    self.dm_view.set_responding(true);
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
                // Clean up split panels targeting the stopped node (both NodeLog and Dm)
                let target_node_log = BufferId::NodeLog { node_id: node_id.clone() };
                let target_dm = BufferId::Dm { node_id: node_id.clone() };
                if self.layout.has_panel_for_buffer(&target_node_log) || self.layout.has_panel_for_buffer(&target_dm) {
                    let _ = self.client.node_unsubscribe(&node_id).await;
                    // Remove matching panels in reverse order to keep indices valid
                    let mut i = self.layout.panels.len();
                    while i > 0 {
                        i -= 1;
                        if self.layout.panels[i].buffer_id == target_node_log || self.layout.panels[i].buffer_id == target_dm {
                            self.remove_panel(i);
                        }
                    }
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
        let target_bid = BufferId::NodeLog { node_id: node_id.to_string() };
        if self.layout.has_panel_for_buffer(&target_bid) {
            if let Some(update) = detail.get("update") {
                let kind = update.get("sessionUpdate").and_then(|v| v.as_str());
                if kind == Some("agent_message_chunk") {
                    if let Some(text) = update.get("content").and_then(|c| c.get("text")).and_then(|v| v.as_str()) {
                        let entry = self.buffer_pool.get_or_create(target_bid.clone());
                        if let BufferContent::NodeLog { text: ref mut buf, ref mut pending } = entry.content {
                            if !*pending {
                                *pending = true;
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                buf.push_str(&format!("assistant  {}\n", ts));
                            }
                            buf.push_str(text);
                        }
                        entry.bump_version();
                    }
                } else if kind == Some("agent_message_end") || kind == Some("stop_reason") {
                    let entry = self.buffer_pool.get_or_create(target_bid.clone());
                    if let BufferContent::NodeLog { ref mut text, ref mut pending } = entry.content {
                        *pending = false;
                        text.push('\n');
                    }
                    entry.bump_version();
                } else if kind == Some("node_log") {
                    if let Some(entries) = update.get("entries").and_then(|v| v.as_array()) {
                        let buf_entry = self.buffer_pool.get_or_create(target_bid.clone());
                        if let BufferContent::NodeLog { ref mut text, .. } = buf_entry.content {
                            for log_entry in entries {
                                let level = log_entry.get("level").and_then(|v| v.as_str()).unwrap_or("info");
                                let message = log_entry.get("message").and_then(|v| v.as_str()).unwrap_or("");
                                let ts_str = log_entry.get("ts").and_then(|v| v.as_str()).unwrap_or("");
                                let time_display = ts_str.get(11..19).unwrap_or("??:??:??");
                                text.push_str(&format!("[{}] [{}] {}\n", time_display, level.to_uppercase(), message));
                            }
                        }
                        buf_entry.bump_version();
                    }
                }
            }
        }

        // Bump version for Dm panels so split panel scroll tracks new data
        let target_dm_bid = BufferId::Dm { node_id: node_id.to_string() };
        if self.layout.has_panel_for_buffer(&target_dm_bid) {
            if let Some(entry) = self.buffer_pool.get_mut(&target_dm_bid) {
                entry.bump_version();
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
                    self.dm_view.apply_streaming_event(name, "agent_message_end", update);
                    let msg = self.dm_view.take_streaming_message(name);
                    let already_flushed = self.dm_view.flushed_agents.remove(name);

                    // Fallback: some agents include full content in the end event
                    // (e.g. during replay when no chunks were sent)
                    let end_content = update
                        .get("content")
                        .and_then(|c| c.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    let (final_content, final_blocks) = if let Some(m) = msg {
                        if !m.blocks.is_empty() {
                            let text = crate::components::dm_view::blocks_to_text(&m.blocks);
                            (text, m.blocks)
                        } else if !already_flushed && !end_content.is_empty() {
                            let blocks = Message::content_to_blocks(end_content);
                            (end_content.to_string(), blocks)
                        } else {
                            (String::new(), Vec::new())
                        }
                    } else if !already_flushed && !end_content.is_empty() {
                        // No streaming_message at all (edge case: replay)
                        let blocks = Message::content_to_blocks(end_content);
                        (end_content.to_string(), blocks)
                    } else {
                        (String::new(), Vec::new())
                    };

                    debug!(
                        "agent_message_end from {}: in_dm={} final={} already_flushed={}",
                        name, in_dm, final_content.len(), already_flushed
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
use ratatui::layout::Rect;
    use std::sync::{Arc, Mutex as StdMutex};

    #[derive(Clone)]
    struct MockTransport {
        name: String,
        response: Arc<StdMutex<Value>>,
    }

    impl MockTransport {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                response: Arc::new(StdMutex::new(Value::Null)),
            }
        }
    }

    impl Transport for MockTransport {
        async fn request(&self, _method: &str, _params: Value) -> anyhow::Result<Value> {
            Ok(self.response.lock().unwrap().clone())
        }

        fn node_name(&self) -> &str {
            &self.name
        }
    }

    fn make_app() -> App<MockTransport> {
        let client = MockTransport::new("test-user");
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        App::new(client, event_rx)
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
        let client = MockTransport::new("test-user");
        let (_event_tx, event_rx) = mpsc::unbounded_channel();
        let app = App::new_with_project(client, event_rx, Some("/tmp/demo-project".into()));

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
        let client = MockTransport::new("test-user");
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

    // --- Split: BufferPool + WindowLayout migration tests ---

    #[test]
    fn is_split_false_when_no_panels() {
        let app = make_app();
        assert!(!app.is_split());
    }

    #[test]
    fn is_split_true_when_panels_exist() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        assert!(app.is_split());
    }

    #[test]
    fn focused_panel_meta_returns_some_when_focused() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.layout.focus = WindowFocus::Panel(0);

        let meta = app.focused_panel_meta_mut();
        assert!(meta.is_some());
    }

    #[test]
    fn focused_panel_meta_returns_none_when_primary() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.layout.focus = WindowFocus::Primary;

        assert!(app.focused_panel_meta_mut().is_none());
    }

    #[test]
    fn focused_panel_meta_returns_none_when_oob() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.layout.focus = WindowFocus::Panel(5);

        assert!(app.focused_panel_meta_mut().is_none());
    }

    #[test]
    fn panel_holds_channel_buffer_id() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        assert!(matches!(app.layout.panels[0].buffer_id, BufferId::Channel { .. }));
    }

    #[test]
    fn panel_holds_node_log_buffer_id() {
        let mut app = make_app();
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        assert!(matches!(app.layout.panels[0].buffer_id, BufferId::NodeLog { .. }));
        assert_eq!(app.panel_meta[0].label, "alice");
    }

    #[test]
    fn panels_have_independent_state() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());

        // Mutate panel 0's state
        app.panel_meta[0].state.auto_scroll = false;
        app.panel_meta[0].state.scroll_offset = 42;

        // Panel 1 should be unaffected
        assert!(app.panel_meta[1].state.auto_scroll);
        assert_eq!(app.panel_meta[1].state.scroll_offset, u16::MAX);
    }

    // --- Render multi-panel logic tests ---

    #[test]
    fn panel_x_boundaries_populated_from_layout() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());

        let area = Rect::new(0, 0, 120, 30);
        let app_layout = AppLayout::build(area, 3, true, 2);

        app.layout.panel_x_boundaries.clear();
        for panel_area in &app_layout.panels {
            app.layout.panel_x_boundaries.push(panel_area.x);
        }

        assert_eq!(app.layout.panel_x_boundaries.len(), 2);
        assert!(app.layout.panel_x_boundaries[0] < app.layout.panel_x_boundaries[1]);
    }

    #[test]
    fn panel_x_boundaries_matches_layout_panels_count() {
        let mut app = make_app();
        for _ in 0..3 {
            app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        }

        let area = Rect::new(0, 0, 120, 30);
        let app_layout = AppLayout::build(area, 3, true, 3);

        app.layout.panel_x_boundaries.clear();
        for panel_area in &app_layout.panels {
            app.layout.panel_x_boundaries.push(panel_area.x);
        }

        assert_eq!(app.layout.panel_x_boundaries.len(), app_layout.panels.len());
        assert_eq!(app.layout.panel_x_boundaries.len(), app.layout.panel_count());
    }

    #[test]
    fn focus_highlights_only_target_panel() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());

        app.layout.focus = WindowFocus::Panel(1);

        for i in 0..3 {
            let focused = app.layout.focus == WindowFocus::Panel(i);
            if i == 1 {
                assert!(focused, "panel 1 should be focused");
            } else {
                assert!(!focused, "panel {} should NOT be focused", i);
            }
        }
    }

    #[test]
    fn focus_primary_highlights_no_panel() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.layout.focus = WindowFocus::Primary;

        for i in 0..2 {
            assert!(
                app.layout.focus != WindowFocus::Panel(i),
                "panel {} should NOT be focused when Primary",
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
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.layout.panel_x_boundaries = vec![60];

        assert!(app.mouse_panel_index(59).is_none(), "before panel boundary");
        assert_eq!(app.mouse_panel_index(60), Some(0), "at panel boundary");
        assert_eq!(app.mouse_panel_index(100), Some(0), "inside panel");
    }

    #[test]
    fn mouse_panel_index_multi_panel() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        app.layout.panel_x_boundaries = vec![40, 70];

        assert!(app.mouse_panel_index(39).is_none(), "before any panel");
        assert_eq!(app.mouse_panel_index(40), Some(0), "at panel 0 boundary");
        assert_eq!(app.mouse_panel_index(55), Some(0), "inside panel 0");
        assert_eq!(app.mouse_panel_index(70), Some(1), "at panel 1 boundary");
        assert_eq!(app.mouse_panel_index(100), Some(1), "inside panel 1");
    }

    #[test]
    fn node_panel_uses_buffer_pool() {
        let mut app = make_app();
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        app.add_panel(BufferId::NodeLog { node_id: "n2".into() }, "bob".into());

        // Write to each buffer independently via buffer_pool
        let bid1 = BufferId::NodeLog { node_id: "n1".into() };
        let entry1 = app.buffer_pool.get_or_create(bid1);
        if let BufferContent::NodeLog { ref mut text, .. } = entry1.content {
            text.push_str("alice output");
        }

        let bid2 = BufferId::NodeLog { node_id: "n2".into() };
        let entry2 = app.buffer_pool.get_or_create(bid2);
        if let BufferContent::NodeLog { ref mut text, .. } = entry2.content {
            text.push_str("bob output");
        }

        assert!(app.panel_node_buffer(0).contains("alice output"));
        assert!(app.panel_node_buffer(1).contains("bob output"));
        assert_eq!(app.panel_meta[0].label, "alice");
        assert_eq!(app.panel_meta[1].label, "bob");
    }

    #[test]
    fn channel_panel_distinct_from_node() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());

        assert!(matches!(app.layout.panels[0].buffer_id, BufferId::Channel { .. }));
        assert!(matches!(app.layout.panels[1].buffer_id, BufferId::NodeLog { .. }));
    }

    // --- Keyboard interaction tests ---

    #[test]
    fn ctrl_w_single_panel_cycles() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.layout.focus = WindowFocus::Primary;

        app.layout.cycle_focus_forward();
        assert_eq!(app.layout.focus, WindowFocus::Panel(0));

        app.layout.cycle_focus_forward();
        assert_eq!(app.layout.focus, WindowFocus::Primary);

        app.layout.cycle_focus_forward();
        assert_eq!(app.layout.focus, WindowFocus::Panel(0));
    }

    #[test]
    fn ctrl_w_two_panels_cycles() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        app.layout.focus = WindowFocus::Primary;

        app.layout.cycle_focus_forward();
        assert_eq!(app.layout.focus, WindowFocus::Panel(0));

        app.layout.cycle_focus_forward();
        assert_eq!(app.layout.focus, WindowFocus::Panel(1));

        app.layout.cycle_focus_forward();
        assert_eq!(app.layout.focus, WindowFocus::Primary);
    }

    #[test]
    fn ctrl_w_three_panels_cycles_all() {
        let mut app = make_app();
        for _ in 0..3 {
            app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        }
        app.layout.focus = WindowFocus::Primary;

        let expected = [
            WindowFocus::Panel(0),
            WindowFocus::Panel(1),
            WindowFocus::Panel(2),
            WindowFocus::Primary,
        ];
        for exp in &expected {
            app.layout.cycle_focus_forward();
            assert_eq!(app.layout.focus, *exp);
        }
    }

    #[test]
    fn scroll_routes_to_focused_panel() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        app.layout.focus = WindowFocus::Panel(1);

        if let Some(meta) = app.focused_panel_meta_mut() {
            meta.state.scroll_up(5);
        }

        // Panel 1 should be scrolled
        assert_ne!(app.panel_meta[1].state.scroll_offset, u16::MAX);
        // Panel 0 should be untouched
        assert_eq!(app.panel_meta[0].state.scroll_offset, u16::MAX);
    }

    #[test]
    fn focus_fallback_after_panel_removal() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        app.add_panel(BufferId::NodeLog { node_id: "n2".into() }, "bob".into());
        app.layout.focus = WindowFocus::Panel(2);

        app.remove_panel(2);

        assert_eq!(app.layout.focus, WindowFocus::Panel(1));
    }

    #[test]
    fn focus_fallback_all_panels_removed() {
        let mut app = make_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.layout.focus = WindowFocus::Panel(0);

        app.clear_panels();

        assert_eq!(app.layout.focus, WindowFocus::Primary);
    }

    #[tokio::test]
    async fn ctrl_w_no_panels_acts_as_delete_word() {
        let mut app = make_app();
        app.dm_view = DmView::new("alice");
        app.view_mode = ViewMode::Dm { node_id: "n1".into(), node_name: "alice".into() };
        app.input.insert_str("hello world");

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)).await;

        assert!(!app.input.text.contains("world"), "Ctrl+W without panels should delete word, got: {}", app.input.text);
    }

    // --- /split command tests ---

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
    async fn split_no_arg_no_panels_adds_channel_panel() {
        let mut app = make_dm_app();
        assert!(!app.is_split());

        app.handle_command("/split").await;

        assert_eq!(app.layout.panel_count(), 1);
        assert!(matches!(app.layout.panels[0].buffer_id, BufferId::Channel { .. }));
    }

    #[tokio::test]
    async fn split_no_arg_with_panels_clears_all() {
        let mut app = make_dm_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        assert_eq!(app.layout.panel_count(), 2);

        app.handle_command("/split").await;

        assert!(!app.is_split());
        assert_eq!(app.layout.focus, WindowFocus::Primary);
    }

    #[test]
    fn split_at_agent_adds_node_panel() {
        let mut app = make_dm_app();

        // Simulate what /split @alice does on successful subscribe:
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        app.layout.focus = WindowFocus::Primary;

        assert_eq!(app.layout.panel_count(), 1);
        assert!(matches!(app.layout.panels[0].buffer_id, BufferId::NodeLog { .. }));
        assert_eq!(app.panel_meta[0].label, "alice");
        assert_eq!(app.layout.focus, WindowFocus::Primary);
    }

    #[tokio::test]
    async fn split_at_agent_dedup_focuses_existing() {
        let mut app = make_dm_app();
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.layout.focus = WindowFocus::Primary;

        app.handle_command("/split @alice").await;

        assert_eq!(app.layout.panel_count(), 2, "duplicate @alice should not add panel");
        assert_eq!(app.layout.focus, WindowFocus::Panel(0));
    }

    #[tokio::test]
    async fn split_close_removes_focused_panel() {
        let mut app = make_dm_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        app.layout.focus = WindowFocus::Panel(1);

        app.handle_command("/split close").await;

        assert_eq!(app.layout.panel_count(), 1, "focused panel should be removed");
        assert_eq!(app.layout.focus, WindowFocus::Panel(0));
    }

    #[tokio::test]
    async fn split_close_all_clears_everything() {
        let mut app = make_dm_app();
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        app.layout.focus = WindowFocus::Panel(2);

        app.handle_command("/split close all").await;

        assert!(!app.is_split());
        assert_eq!(app.layout.focus, WindowFocus::Primary);
    }

    #[tokio::test]
    async fn split_panel_limit_rejects_fifth() {
        let mut app = make_dm_app();
        for _ in 0..4 {
            app.add_panel(BufferId::Channel { channel_id: String::new() }, String::new());
        }
        assert_eq!(app.layout.panel_count(), 4);

        app.handle_command("/split #extra").await;

        assert_eq!(app.layout.panel_count(), 4, "should not exceed 4 panels");
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

        assert_eq!(app.layout.panel_count(), 1, "/split #ops should add a panel");
    }

    // --- node_log event routing tests ---

    fn node_update_chunk(text: &str) -> serde_json::Value {
        serde_json::json!({
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "text": text }
            }
        })
    }

    #[test]
    fn node_log_routes_to_matching_panel() {
        let mut app = make_app();
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());

        let detail = node_update_chunk("hello from alice");
        app.handle_node_update("n1", "alice", &detail);

        let buf = app.panel_node_buffer(0);
        assert!(buf.contains("hello from alice"));
        assert!(buf.starts_with("assistant"));
    }

    #[test]
    fn node_log_shared_buffer_for_same_node() {
        let mut app = make_app();
        // Two panels both subscribe to same node — they share the buffer_pool entry
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());

        let detail = node_update_chunk("broadcast");
        app.handle_node_update("n1", "alice", &detail);

        // Both panels read from same buffer
        assert!(app.panel_node_buffer(0).contains("broadcast"));
        assert!(app.panel_node_buffer(1).contains("broadcast"));
    }

    #[test]
    fn node_log_isolates_different_nodes() {
        let mut app = make_app();
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        app.add_panel(BufferId::NodeLog { node_id: "n2".into() }, "bob".into());

        let detail_a = node_update_chunk("alice output");
        app.handle_node_update("n1", "alice", &detail_a);

        let detail_b = node_update_chunk("bob output");
        app.handle_node_update("n2", "bob", &detail_b);

        assert!(app.panel_node_buffer(0).contains("alice output"));
        assert!(app.panel_node_buffer(1).contains("bob output"));
    }

    #[test]
    fn node_log_no_matching_panel_no_panic() {
        let mut app = make_app();
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());

        let detail = node_update_chunk("orphan log");
        app.handle_node_update("n2", "bob", &detail);

        assert!(app.panel_node_buffer(0).is_empty());
    }

    #[test]
    fn node_log_preserves_auto_scroll() {
        let mut app = make_app();
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());
        assert!(app.panel_meta[0].state.auto_scroll);

        let detail = node_update_chunk("some log");
        app.handle_node_update("n1", "alice", &detail);

        assert!(app.panel_meta[0].state.auto_scroll);
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
        use ratatui::buffer::Buffer;

        let mut app = make_app();
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "alice".into());

        let chunk = node_update_chunk("Hello from assistant");
        app.handle_node_update("n1", "alice", &chunk);
        let end_detail = serde_json::json!({
            "update": { "sessionUpdate": "agent_message_end" }
        });
        app.handle_node_update("n1", "alice", &end_detail);

        let area = Rect::new(0, 0, 60, 20);
        let mut buf = Buffer::empty(area);
        let node_buf = app.panel_node_buffer(0).to_string();
        channel_view::render_text_panel(
            "@alice",
            &node_buf,
            &mut app.panel_meta[0].state,
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
    fn node_log_routes_to_split_panel_buffer_pool() {
        let mut app = make_app();

        app.active_channel = Some("ch1".into());
        app.view_mode = ViewMode::Channel { channel_id: "ch1".into() };
        app.add_panel(BufferId::NodeLog { node_id: "n1".into() }, "context-guardian".into());

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
            !app.panel_node_buffer(0).is_empty(),
            "node_log event should populate buffer_pool node_buffer"
        );
        assert!(
            app.panel_node_buffer(0).contains("context window compacted"),
            "buffer_pool should contain the log message"
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

    // ========== Buffer/Window integration tests (Part 2) ==========

    mod buffer_window {
        use crate::buffer::*;

        #[test]
        fn buffer_pool_and_layout_coexist() {
            let pool = BufferPool::new();
            let primary_id = BufferId::Channel { channel_id: "ch1".into() };
            let primary = Window::new(primary_id.clone(), 0);
            let layout = WindowLayout::new(primary);

            // App 构造后应有 buffer_pool（空）和 layout（1 primary, 0 panels）
            assert!(pool.get(&primary_id).is_none());
            assert_eq!(layout.panel_count(), 0);
            assert_eq!(layout.focus, WindowFocus::Primary);
        }

        #[test]
        fn add_panel_increments_panel_count() {
            let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
            let mut layout = WindowLayout::new(primary);

            let p1 = Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0);
            layout.add_panel(p1);
            assert_eq!(layout.panel_count(), 1);

            let p2 = Window::new(BufferId::NodeLog { node_id: "n2".into() }, 0);
            layout.add_panel(p2);
            assert_eq!(layout.panel_count(), 2);
        }

        #[test]
        fn cycle_focus_forward_wraps_around() {
            let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
            let mut layout = WindowLayout::new(primary);
            layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0));
            layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n2".into() }, 0));

            assert_eq!(layout.focus, WindowFocus::Primary);
            layout.cycle_focus_forward();
            assert_eq!(layout.focus, WindowFocus::Panel(0));
            layout.cycle_focus_forward();
            assert_eq!(layout.focus, WindowFocus::Panel(1));
            layout.cycle_focus_forward();
            assert_eq!(layout.focus, WindowFocus::Primary); // wraps
        }

        #[test]
        fn remove_panel_clamps_focus() {
            let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
            let mut layout = WindowLayout::new(primary);
            layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0));
            layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n2".into() }, 0));

            // Focus on last panel
            layout.focus = WindowFocus::Panel(1);
            // Remove it
            layout.remove_panel(1);
            assert_eq!(layout.panel_count(), 1);
            // Focus should clamp to Panel(0)
            assert_eq!(layout.focus, WindowFocus::Panel(0));

            // Remove last remaining panel → focus back to Primary
            layout.remove_panel(0);
            assert_eq!(layout.panel_count(), 0);
            assert_eq!(layout.focus, WindowFocus::Primary);
        }

        #[test]
        fn panel_x_boundaries_matches_panel_count() {
            let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
            let mut layout = WindowLayout::new(primary);

            layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0));
            layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n2".into() }, 0));
            assert_eq!(layout.panel_x_boundaries.len(), layout.panel_count());

            layout.remove_panel(0);
            assert_eq!(layout.panel_x_boundaries.len(), layout.panel_count());
        }

        #[test]
        fn mouse_hit_test_via_panel_x_boundaries() {
            let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
            let mut layout = WindowLayout::new(primary);

            layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0));
            layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n2".into() }, 0));

            // Simulate render setting boundaries: panel 0 at x=40, panel 1 at x=80
            layout.panel_x_boundaries[0] = 40;
            layout.panel_x_boundaries[1] = 80;

            // Hit test: find which panel a mouse click at x falls into
            let hit = |x: u16| -> Option<usize> {
                layout.panel_x_boundaries.iter().rposition(|&bx| x >= bx)
            };

            assert_eq!(hit(39), None);     // before first panel → primary area
            assert_eq!(hit(40), Some(0));  // at panel 0 boundary
            assert_eq!(hit(60), Some(0));  // inside panel 0
            assert_eq!(hit(80), Some(1));  // at panel 1 boundary
            assert_eq!(hit(100), Some(1)); // inside panel 1
        }

        #[test]
        fn node_output_writes_to_buffer_pool_node_log() {
            let mut pool = BufferPool::new();
            let buf_id = BufferId::NodeLog { node_id: "n1".into() };

            // Simulate node output arriving
            let entry = pool.get_or_create(buf_id.clone());
            match &mut entry.content {
                BufferContent::NodeLog { text, pending } => {
                    text.push_str("line 1\n");
                    *pending = true;
                    entry.bump_version();
                }
                _ => panic!("expected NodeLog"),
            }

            // Verify buffer has the content
            let entry = pool.get(&buf_id).unwrap();
            assert_eq!(entry.content_version, 1);
            match &entry.content {
                BufferContent::NodeLog { text, pending } => {
                    assert_eq!(text, "line 1\n");
                    assert!(*pending);
                }
                _ => panic!("expected NodeLog"),
            }

            // Window detects version change
            let mut win = Window::new(buf_id.clone(), 0);
            assert!(win.check_content_version(entry.content_version));
            // Second check with same version → no change
            assert!(!win.check_content_version(entry.content_version));
        }
    }

    // ==========================================================
    // Buffer Unify: split Dm panel tests
    // Design: /split @agent creates BufferId::Dm, not NodeLog
    // See: notes/tasks/tui-split/buffer-unify-design.md
    // ==========================================================

    // RED (assertion failure): current code creates NodeLog, should create Dm
    #[tokio::test]
    async fn split_at_agent_creates_dm_panel() {
        let mut app = make_dm_app();
        app.handle_command("/split @alice").await;

        assert_eq!(app.layout.panel_count(), 1);
        assert!(
            matches!(app.layout.panels[0].buffer_id, BufferId::Dm { .. }),
            "/split @agent should create Dm panel, got: {:?}",
            app.layout.panels[0].buffer_id
        );
    }

    // RED: /split close should match BufferId::Dm for unsubscribe.
    // Current code only matches NodeLog. Panel is still removed (remove_panel
    // is type-agnostic), but unsubscribe is skipped for Dm panels.
    #[tokio::test]
    async fn split_close_handles_dm_panel() {
        let mut app = make_dm_app();
        app.add_panel(BufferId::Dm { node_id: "n1".into() }, "alice".into());
        app.layout.focus = WindowFocus::Panel(0);

        app.handle_command("/split close").await;

        assert_eq!(app.layout.panel_count(), 0);
        assert!(!app.layout.has_panel_for_buffer(&BufferId::Dm { node_id: "n1".into() }));
    }

    // RED: unsubscribe_and_clear_panels should handle both Dm and NodeLog panels.
    // Current code only matches NodeLog for unsubscribe.
    #[tokio::test]
    async fn unsubscribe_clear_handles_dm_panels() {
        let mut app = make_dm_app();
        app.add_panel(BufferId::Dm { node_id: "n1".into() }, "alice".into());
        app.add_panel(BufferId::NodeLog { node_id: "n2".into() }, "bob".into());
        assert_eq!(app.layout.panel_count(), 2);

        app.unsubscribe_and_clear_panels().await;

        assert_eq!(app.layout.panel_count(), 0);
        assert_eq!(app.layout.focus, WindowFocus::Primary);
    }

    // RED (compile error): dm_view_for_node() doesn't exist yet.
    // Tests that DM target mismatch is detected (user switched DM target,
    // residual split panel should show hint, not blank).
    #[test]
    fn dm_target_mismatch_returns_none() {
        let app = make_dm_app(); // DM with n1/alice
        // n1 matches current DM target → Some
        assert!(app.dm_view_for_node("n1").is_some());
        // n2 does not match → None (panel should show "已切换到其他对话")
        assert!(app.dm_view_for_node("n2").is_none());
    }

    // RED (assertion failure): handle_node_update only checks BufferId::NodeLog,
    // should also bump version for BufferId::Dm panels.
    #[test]
    fn node_update_bumps_dm_buffer_version() {
        let mut app = make_dm_app();
        app.add_panel(BufferId::Dm { node_id: "n1".into() }, "alice".into());

        let dm_bid = BufferId::Dm { node_id: "n1".into() };
        let version_before = app.buffer_pool.get(&dm_bid).unwrap().content_version;

        let detail = node_update_chunk("hello");
        app.handle_node_update("n1", "alice", &detail);

        let version_after = app.buffer_pool.get(&dm_bid).unwrap().content_version;
        assert!(
            version_after > version_before,
            "node_update should bump Dm buffer version for panel scroll tracking, was {} now {}",
            version_before, version_after
        );
    }

    #[tokio::test]
    async fn node_registered_should_not_show_in_channel_view() {
        let mut app = make_app();
        app.active_channel = Some("ch1".into());

        let initial_count = app.channel_view.line_count();

        app.handle_nerve_event(NerveEvent::NodeRegistered {
            node_id: "xxx".into(),
            name: "mc".into(),
            adapter: None,
            transport: Some("websocket".into()),
        })
        .await;

        // NodeRegistered is a global event — it should NOT push system messages into channel_view
        let has_registered_msg = app.channel_view.messages.iter().any(|m| {
            m.from == "系统" && m.content.contains("mc") && m.content.contains("已注册")
        });
        assert!(
            !has_registered_msg,
            "NodeRegistered should not appear in channel_view, but found '已注册' message"
        );
        assert_eq!(
            app.channel_view.line_count(),
            initial_count,
            "channel_view message count should not increase on NodeRegistered"
        );
    }
}

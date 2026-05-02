use nerve_tui_core::Transport;
use nerve_tui_protocol::*;
use serde_json::Value;
use std::path::Path;
use tokio::sync::mpsc;

use crate::components::channel_view::ChannelPanelState;
use crate::components::channel_view::ChannelView;
use crate::components::dm_view::DmView;
use crate::components::spinner::{BrailleSpinner, KnightRiderScanner};
use crate::components::*;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SplitFocus {
    Dm,
    Panel(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SplitTarget {
    Channel,
    Node { node_id: String, node_name: String },
}

/// A single split panel with its own target, buffer, and scroll state.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SplitPanel {
    pub target: SplitTarget,
    pub node_buffer: String,
    /// Whether an AI message is currently streaming (between first chunk and message_end).
    pub node_msg_pending: bool,
    pub panel_state: ChannelPanelState,
}

pub struct App<T: Transport> {
    pub client: T,
    pub(crate) event_rx: mpsc::UnboundedReceiver<NerveEvent>,

    // UI components — direct view fields (replaces MessagesView proxy)
    pub(crate) channel_view: ChannelView,
    pub(crate) dm_view: DmView,
    pub(crate) status_bar: StatusBar,
    pub(crate) input: InputBox,

    // Data
    pub(crate) channels: Vec<ChannelDisplay>,
    pub(crate) agents: Vec<AgentDisplay>,
    pub(crate) active_channel: Option<String>,
    pub(crate) should_quit: bool,

    /// Explicit view mode state machine.
    pub(crate) view_mode: ViewMode,

    pub(crate) project_path: Option<String>,
    pub(crate) project_name: Option<String>,
    /// When false (default), only show channels/agents for project_path
    pub(crate) global_mode: bool,
    /// Sidebar visibility toggle (Ctrl+B)
    pub(crate) sidebar_visible: bool,
    /// Channel for background task errors (e.g. prompt failures)
    pub(crate) error_tx: mpsc::UnboundedSender<String>,
    pub(crate) error_rx: mpsc::UnboundedReceiver<String>,
    /// Cached archived channels from last /restore call
    pub(crate) archived_channels: Vec<Value>,

    // Split view
    pub(crate) split_panels: Vec<SplitPanel>,
    pub(crate) split_focus: SplitFocus,
    /// Cached x-coordinates of panel left boundaries (for mouse hit-testing in split view).
    pub(crate) panel_x_boundaries: Vec<u16>,
    /// Dirty flag — skip redraw if nothing changed since last frame.
    pub(crate) needs_redraw: bool,
    /// When true, clear terminal buffer before next draw (forces full repaint).
    pub(crate) force_clear: bool,
    /// Spinner for tool pending / streaming cursor animation.
    pub(crate) spinner: BrailleSpinner,
    /// Knight Rider scanner animation for input metadata line during agent response.
    pub(crate) scanner: KnightRiderScanner,
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
            spinner: BrailleSpinner::new(),
            scanner: KnightRiderScanner::new(0),
        }
    }

    /// Returns the cwd filter for API calls: project_path in project mode, None in global mode.
    pub(crate) fn cwd_filter(&self) -> Option<&str> {
        if self.global_mode {
            None
        } else {
            self.project_path.as_deref()
        }
    }

    pub(crate) fn project_name_from_path(path: &str) -> Option<String> {
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

    pub(crate) fn is_dm_mode(&self) -> bool {
        matches!(self.view_mode, ViewMode::Dm { .. })
    }

    pub(crate) fn is_split(&self) -> bool {
        !self.split_panels.is_empty()
    }

    pub(crate) fn split_panel_count(&self) -> usize {
        self.split_panels.len()
    }

    pub(crate) fn focused_panel_mut(&mut self) -> Option<&mut SplitPanel> {
        match self.split_focus {
            SplitFocus::Panel(i) => self.split_panels.get_mut(i),
            _ => None,
        }
    }

    /// Clamp split_focus to a valid index after panels are removed.
    pub(crate) fn clamp_split_focus(&mut self) {
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
    pub(crate) async fn close_all_split_panels(&mut self) {
        for panel in &self.split_panels {
            if let SplitTarget::Node { ref node_id, .. } = panel.target {
                let _ = self.client.node_unsubscribe(node_id).await;
            }
        }
        self.split_panels.clear();
        self.split_focus = SplitFocus::Dm;
    }

    /// Close a single split panel by index, unsubscribing if it targets a node.
    pub(crate) async fn close_split_panel(&mut self, index: usize) {
        if index < self.split_panels.len() {
            if let SplitTarget::Node { ref node_id, .. } = self.split_panels[index].target {
                let id = node_id.clone();
                let _ = self.client.node_unsubscribe(&id).await;
            }
            self.split_panels.remove(index);
            self.clamp_split_focus();
        }
    }

    pub(crate) fn dm_node_id(&self) -> Option<&str> {
        match &self.view_mode {
            ViewMode::Dm { node_id, .. } => Some(node_id.as_str()),
            _ => None,
        }
    }

    pub(crate) fn dm_node_name(&self) -> Option<&str> {
        match &self.view_mode {
            ViewMode::Dm { node_name, .. } => Some(node_name.as_str()),
            _ => None,
        }
    }

    /// Scroll the currently active view (DM or channel).
    pub(crate) fn scroll_active_up(&mut self, n: u16) {
        if self.is_dm_mode() {
            self.dm_view.scroll_up(n);
        } else {
            self.channel_view.scroll_up(n);
        }
    }

    pub(crate) fn scroll_active_down(&mut self, n: u16) {
        if self.is_dm_mode() {
            self.dm_view.scroll_down(n);
        } else {
            self.channel_view.scroll_down(n);
        }
    }

    pub(crate) fn page_active_up(&mut self) {
        if self.is_dm_mode() {
            self.dm_view.page_up();
        } else {
            self.channel_view.page_up();
        }
    }

    pub(crate) fn page_active_down(&mut self) {
        if self.is_dm_mode() {
            self.dm_view.page_down();
        } else {
            self.channel_view.page_down();
        }
    }

    pub(crate) fn snap_active_to_bottom(&mut self) {
        if self.is_dm_mode() {
            self.dm_view.snap_to_bottom();
        } else {
            self.channel_view.snap_to_bottom();
        }
    }
}

pub mod block_renderer;
pub mod diff_view;
pub mod spinner;
pub mod channel_view;
pub mod dm_view;
pub mod input;
pub mod message_list;
pub mod messages;
pub mod render_cache;
pub mod status_bar;

pub use channel_view::ChannelPanelState;
pub use input::InputBox;
pub use status_bar::{AgentDisplay, ChannelDisplay, MemberDisplay, NavigationTarget, SidebarItem, StatusBar};

/// Explicit view mode state machine — replaces scattered `dm_view.is_some()` checks.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewMode {
    Channel { channel_id: String },
    Dm { node_id: String, node_name: String },
}

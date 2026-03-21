use ratatui::style::Color;

pub const BORDER: Color = Color::DarkGray;
pub const TITLE: Color = Color::White;
pub const SYSTEM_MSG: Color = Color::DarkGray;
pub const USER_MSG: Color = Color::White;
pub const AGENT_MSG: Color = Color::Cyan;
pub const MENTION: Color = Color::Yellow;
pub const TIMESTAMP: Color = Color::DarkGray;
pub const CHANNEL_ACTIVE: Color = Color::Cyan;
pub const CHANNEL_INACTIVE: Color = Color::DarkGray;

pub const STATUS_IDLE: Color = Color::DarkGray;
pub const STATUS_STREAMING: Color = Color::Green;
pub const STATUS_CONNECTING: Color = Color::Yellow;
pub const STATUS_DISCONNECTED: Color = Color::Red;
pub const STATUS_BUSY: Color = Color::Green;
pub const STATUS_ERROR: Color = Color::Red;

/// Rotating colors for different agents in messages
const AGENT_COLORS: [Color; 6] = [
    Color::Cyan,
    Color::Green,
    Color::Magenta,
    Color::Blue,
    Color::Yellow,
    Color::Red,
];

/// Get a stable color for an agent name
pub fn agent_color(name: &str) -> Color {
    if name == "系统" {
        return SYSTEM_MSG;
    }
    let hash: usize = name.bytes().fold(0usize, |acc, b| acc.wrapping_add(b as usize));
    AGENT_COLORS[hash % AGENT_COLORS.len()]
}

pub fn status_icon(status: &str) -> &'static str {
    match status {
        "idle" => "○",
        "busy" => "●",
        "connecting" => "◌",
        "error" => "✗",
        "stopped" => "✗",
        _ => "?",
    }
}

pub fn status_color(status: &str) -> Color {
    match status {
        "idle" => STATUS_IDLE,
        "busy" => STATUS_BUSY,
        "connecting" => STATUS_CONNECTING,
        "error" => STATUS_ERROR,
        "stopped" => STATUS_DISCONNECTED,
        "streaming" => STATUS_STREAMING,
        _ => BORDER,
    }
}

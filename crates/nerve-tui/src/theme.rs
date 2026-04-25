use ratatui::style::Color;

// --- 背景三级深度（暖白 Light） ---
pub const BG_L0: Color = Color::Rgb(0xf5, 0xf0, 0xe8); // #f5f0e8 页面背景（消息区）
pub const BG_L1: Color = Color::Rgb(0xeb, 0xe5, 0xda); // #ebe5da 面板（sidebar、代码块）
pub const BG_L2: Color = Color::Rgb(0xe0, 0xd8, 0xcc); // #e0d8cc 元素（输入框、用户消息）

// --- 文本 ---
pub const TEXT: Color = Color::Rgb(0x2d, 0x24, 0x18);     // #2d2418 主文本
pub const TEXT_MUTED: Color = Color::Rgb(0x8a, 0x7e, 0x6e); // #8a7e6e 淡文本/时间

// --- 边框 ---
pub const BORDER: Color = Color::Rgb(0xd4, 0xcb, 0xbe);     // #d4cbbe 普通边框
pub const BORDER_ACTIVE: Color = Color::Rgb(0xb8, 0xad, 0x9c); // #b8ad9c 活跃边框

// --- 强调色 ---
pub const PRIMARY: Color = Color::Rgb(0xc4, 0x65, 0x2a);   // #c4652a 橙棕
pub const SECONDARY: Color = Color::Rgb(0xa6, 0x7c, 0x52);  // #a67c52 驼色
pub const SUCCESS: Color = Color::Rgb(0x6b, 0x8f, 0x4a);    // #6b8f4a 橄榄绿
pub const ERROR: Color = Color::Rgb(0xc4, 0x40, 0x40);      // #c44040 暖红
pub const WARNING: Color = Color::Rgb(0xb8, 0x92, 0x2a);    // #b8922a 琥珀
pub const INFO: Color = Color::Rgb(0x6a, 0x8e, 0x8a);       // #6a8e8a 灰青

// --- 兼容别名（渐进迁移，后续 task 逐步替换调用方后删除） ---
pub const TITLE: Color = TEXT;
pub const SYSTEM_MSG: Color = TEXT_MUTED;
pub const USER_MSG: Color = TEXT;
pub const AGENT_MSG: Color = PRIMARY;
pub const MENTION: Color = WARNING;
pub const TIMESTAMP: Color = TEXT_MUTED;
pub const CHANNEL_ACTIVE: Color = PRIMARY;
pub const CHANNEL_INACTIVE: Color = TEXT_MUTED;
pub const STATUS_IDLE: Color = TEXT_MUTED;
pub const STATUS_STREAMING: Color = SUCCESS;
pub const STATUS_CONNECTING: Color = WARNING;
pub const STATUS_DISCONNECTED: Color = TEXT_MUTED;
pub const STATUS_BUSY: Color = SUCCESS;
pub const STATUS_ERROR: Color = ERROR;
pub const TOOL_NAME: Color = SECONDARY;
pub const TOOL_LABEL: Color = TEXT_MUTED;
pub const TOOL_KEY: Color = WARNING;
pub const TOOL_VALUE: Color = TEXT;

/// Agent identity colors for left border strips
const AGENT_COLORS: [Color; 6] = [
    Color::Rgb(0xc4, 0x65, 0x2a), // 橙棕
    Color::Rgb(0xa6, 0x7c, 0x52), // 驼色
    Color::Rgb(0x6b, 0x8f, 0x4a), // 橄榄绿
    Color::Rgb(0xb8, 0x92, 0x2a), // 琥珀
    Color::Rgb(0x6a, 0x8e, 0x8a), // 灰青
    Color::Rgb(0x9b, 0x6b, 0x6b), // 玫瑰灰
];

pub fn agent_color(name: &str) -> Color {
    if name == "系统" {
        return TEXT_MUTED;
    }
    let hash: usize = name
        .bytes()
        .fold(0usize, |acc, b| acc.wrapping_add(b as usize));
    AGENT_COLORS[hash % AGENT_COLORS.len()]
}

pub fn status_icon(status: &str) -> &'static str {
    match status {
        "idle" => "◉",
        "busy" | "streaming" => "●",
        "connecting" => "◌",
        "error" => "○",
        "stopped" => "○",
        _ => "○",
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn agent_color_returns_rgb() {
        let color = agent_color("claude");
        match color {
            Color::Rgb(_, _, _) => {}
            _ => panic!("agent_color should return RGB, got {:?}", color),
        }
    }

    #[test]
    fn agent_color_system_returns_muted_text() {
        assert_eq!(agent_color("系统"), TEXT_MUTED);
    }

    #[test]
    fn agent_color_stable_across_calls() {
        let c1 = agent_color("claude");
        let c2 = agent_color("claude");
        assert_eq!(c1, c2);
    }

    #[test]
    fn agent_color_different_names_can_differ() {
        let c1 = agent_color("claude");
        let c2 = agent_color("gemini");
        let _ = (c1, c2);
    }

    #[test]
    fn status_color_returns_rgb() {
        for status in &["idle", "streaming", "busy", "error", "connecting"] {
            match status_color(status) {
                Color::Rgb(_, _, _) => {}
                other => panic!("status_color({}) should return RGB, got {:?}", status, other),
            }
        }
    }

    #[test]
    fn background_levels_are_distinct() {
        assert_ne!(BG_L0, BG_L1);
        assert_ne!(BG_L1, BG_L2);
        assert_ne!(BG_L0, BG_L2);
    }
}

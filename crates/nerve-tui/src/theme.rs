use std::sync::{LazyLock, RwLock};

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

// ---------------------------------------------------------------------------
// Theme struct — wraps all colors for a named theme
// ---------------------------------------------------------------------------

pub struct Theme {
    pub background: Color,
    pub background_panel: Color,
    pub background_element: Color,
    pub background_menu: Color,
    pub text: Color,
    pub text_muted: Color,
    pub border: Color,
    pub border_active: Color,
    pub border_subtle: Color,
    pub primary: Color,
    pub secondary: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,
    pub markdown_text: Color,
    pub markdown_heading: Color,
    pub markdown_code: Color,
    pub markdown_code_block: Color,
    pub markdown_link: Color,
    pub markdown_block_quote: Color,
    pub markdown_list_item: Color,
    pub diff_added_bg: Color,
    pub diff_removed_bg: Color,
    pub diff_context_bg: Color,
    pub diff_highlight_added: Color,
    pub diff_highlight_removed: Color,
    pub diff_line_number: Color,
    pub agent_colors: Vec<Color>,
    pub syntect_theme_name: String,
}

impl Theme {
    pub fn warm_light() -> Self {
        Self {
            background:             Color::Rgb(0xf5, 0xf0, 0xe8),
            background_panel:       Color::Rgb(0xeb, 0xe5, 0xda),
            background_element:     Color::Rgb(0xe0, 0xd8, 0xcc),
            background_menu:        Color::Rgb(0xd4, 0xcb, 0xbe),
            text:                   Color::Rgb(0x2d, 0x24, 0x18),
            text_muted:             Color::Rgb(0x8a, 0x7e, 0x6e),
            border:                 Color::Rgb(0xd4, 0xcb, 0xbe),
            border_active:          Color::Rgb(0xb8, 0xad, 0x9c),
            border_subtle:          Color::Rgb(0xe0, 0xd8, 0xcc),
            primary:                Color::Rgb(0xc4, 0x65, 0x2a),
            secondary:              Color::Rgb(0xa6, 0x7c, 0x52),
            accent:                 Color::Rgb(0x9b, 0x6b, 0x6b),
            success:                Color::Rgb(0x6b, 0x8f, 0x4a),
            warning:                Color::Rgb(0xb8, 0x92, 0x2a),
            error:                  Color::Rgb(0xc4, 0x40, 0x40),
            info:                   Color::Rgb(0x6a, 0x8e, 0x8a),
            markdown_text:          Color::Rgb(0x2d, 0x24, 0x18),
            markdown_heading:       Color::Rgb(0xc4, 0x65, 0x2a),
            markdown_code:          Color::Rgb(0xb8, 0x92, 0x2a),
            markdown_code_block:    Color::Rgb(0xeb, 0xe5, 0xda),
            markdown_link:          Color::Rgb(0x6a, 0x8e, 0x8a),
            markdown_block_quote:   Color::Rgb(0x8a, 0x7e, 0x6e),
            markdown_list_item:     Color::Rgb(0x2d, 0x24, 0x18),
            diff_added_bg:          Color::Rgb(0xd5, 0xe8, 0xd0),
            diff_removed_bg:        Color::Rgb(0xf0, 0xd5, 0xd5),
            diff_context_bg:        Color::Rgb(0xf5, 0xf0, 0xe8),
            diff_highlight_added:   Color::Rgb(0x4d, 0xb3, 0x80),
            diff_highlight_removed: Color::Rgb(0xe0, 0x6c, 0x75),
            diff_line_number:       Color::Rgb(0x8a, 0x7e, 0x6e),
            agent_colors: vec![
                Color::Rgb(0xc4, 0x65, 0x2a),
                Color::Rgb(0xa6, 0x7c, 0x52),
                Color::Rgb(0x6b, 0x8f, 0x4a),
                Color::Rgb(0xb8, 0x92, 0x2a),
                Color::Rgb(0x6a, 0x8e, 0x8a),
                Color::Rgb(0x9b, 0x6b, 0x6b),
            ],
            syntect_theme_name: "base16-ocean.light".to_string(),
        }
    }

    pub fn opencode_dark() -> Self {
        Self {
            background:             Color::Rgb(0x0a, 0x0a, 0x0a),
            background_panel:       Color::Rgb(0x14, 0x14, 0x14),
            background_element:     Color::Rgb(0x1e, 0x1e, 0x1e),
            background_menu:        Color::Rgb(0x14, 0x14, 0x14),
            text:                   Color::Rgb(0xee, 0xee, 0xee),
            text_muted:             Color::Rgb(0x80, 0x80, 0x80),
            border:                 Color::Rgb(0x48, 0x48, 0x48),
            border_active:          Color::Rgb(0x60, 0x60, 0x60),
            border_subtle:          Color::Rgb(0x3c, 0x3c, 0x3c),
            primary:                Color::Rgb(0xfa, 0xb2, 0x83),
            secondary:              Color::Rgb(0x5c, 0x9c, 0xf5),
            accent:                 Color::Rgb(0x9d, 0x7c, 0xd8),
            success:                Color::Rgb(0x7f, 0xd8, 0x8f),
            warning:                Color::Rgb(0xf5, 0xa7, 0x42),
            error:                  Color::Rgb(0xe0, 0x6c, 0x75),
            info:                   Color::Rgb(0x56, 0xb6, 0xc2),
            markdown_text:          Color::Rgb(0xee, 0xee, 0xee),
            markdown_heading:       Color::Rgb(0xfa, 0xb2, 0x83),
            markdown_code:          Color::Rgb(0xf5, 0xa7, 0x42),
            markdown_code_block:    Color::Rgb(0x14, 0x14, 0x14),
            markdown_link:          Color::Rgb(0x56, 0xb6, 0xc2),
            markdown_block_quote:   Color::Rgb(0x80, 0x80, 0x80),
            markdown_list_item:     Color::Rgb(0xee, 0xee, 0xee),
            diff_added_bg:          Color::Rgb(0x20, 0x30, 0x3b),
            diff_removed_bg:        Color::Rgb(0x37, 0x22, 0x2c),
            diff_context_bg:        Color::Rgb(0x14, 0x14, 0x14),
            diff_highlight_added:   Color::Rgb(0xb8, 0xdb, 0x87),
            diff_highlight_removed: Color::Rgb(0xe2, 0x6a, 0x75),
            diff_line_number:       Color::Rgb(0x8f, 0x8f, 0x8f),
            agent_colors: vec![
                Color::Rgb(0xfa, 0xb2, 0x83),
                Color::Rgb(0x5c, 0x9c, 0xf5),
                Color::Rgb(0x7f, 0xd8, 0x8f),
                Color::Rgb(0xf5, 0xa7, 0x42),
                Color::Rgb(0x56, 0xb6, 0xc2),
                Color::Rgb(0x9d, 0x7c, 0xd8),
            ],
            syntect_theme_name: "base16-ocean.dark".to_string(),
        }
    }

    pub fn agent_color(&self, name: &str) -> Color {
        if name == "系统" {
            return self.text_muted;
        }
        let hash: usize = name
            .bytes()
            .fold(0usize, |acc, b| acc.wrapping_add(b as usize));
        self.agent_colors[hash % self.agent_colors.len()]
    }

    pub fn status_color(&self, status: &str) -> Color {
        match status {
            "idle" => self.text_muted,
            "busy" => self.success,
            "connecting" => self.warning,
            "error" => self.error,
            "stopped" => self.text_muted,
            "streaming" => self.success,
            _ => self.border,
        }
    }
}

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

// ---------------------------------------------------------------------------
// Global theme accessor
// ---------------------------------------------------------------------------

static CURRENT_THEME: LazyLock<RwLock<Theme>> =
    LazyLock::new(|| RwLock::new(Theme::warm_light()));

pub fn current() -> std::sync::RwLockReadGuard<'static, Theme> {
    CURRENT_THEME.read().unwrap()
}

pub fn set_theme(theme: Theme) {
    *CURRENT_THEME.write().unwrap() = theme;
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

    #[test]
    fn warm_light_has_distinct_backgrounds() {
        let t = Theme::warm_light();
        assert_ne!(t.background, t.background_panel);
        assert_ne!(t.background_panel, t.background_element);
    }

    #[test]
    fn opencode_dark_has_distinct_backgrounds() {
        let t = Theme::opencode_dark();
        assert_ne!(t.background, t.background_panel);
        assert_ne!(t.background_panel, t.background_element);
    }

    #[test]
    fn theme_agent_color_stable() {
        let t = Theme::warm_light();
        let c1 = t.agent_color("claude");
        let c2 = t.agent_color("claude");
        assert_eq!(c1, c2);
    }

    #[test]
    fn theme_agent_color_system_returns_muted() {
        let t = Theme::warm_light();
        assert_eq!(t.agent_color("系统"), t.text_muted);
    }

    #[test]
    fn theme_status_color_returns_rgb() {
        let t = Theme::warm_light();
        for status in &["idle", "streaming", "busy", "error"] {
            match t.status_color(status) {
                Color::Rgb(_, _, _) => {}
                other => panic!("status_color({}) returned {:?}", status, other),
            }
        }
    }

    #[test]
    fn set_and_get_theme() {
        set_theme(Theme::opencode_dark());
        let t = current();
        assert_eq!(t.background, Color::Rgb(0x0a, 0x0a, 0x0a));
        drop(t);
        set_theme(Theme::warm_light());
    }
}

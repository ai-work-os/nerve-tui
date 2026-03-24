use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub struct AppLayout {
    pub sidebar: Rect,
    pub messages: Rect,
    pub input: Rect,
    /// Right panel for split view (channel messages). None when not in split mode.
    pub channel_panel: Option<Rect>,
}

impl AppLayout {
    /// Calculate the inner width of the input box (excluding borders) for a given configuration.
    /// Use this to call visual_line_count consistently with the actual layout width.
    pub fn input_inner_width(area: Rect, sidebar_visible: bool, split_view: bool) -> u16 {
        let sidebar_width: u16 = if sidebar_visible { 20 } else { 0 };
        let main_w = area.width.saturating_sub(sidebar_width);
        let dm_w = if split_view { main_w / 2 } else { main_w };
        dm_w.saturating_sub(2) // borders
    }

    pub fn new(area: Rect, input_lines: u16) -> Self {
        Self::build(area, input_lines, true, false)
    }

    pub fn with_sidebar(area: Rect, input_lines: u16, sidebar_visible: bool) -> Self {
        Self::build(area, input_lines, sidebar_visible, false)
    }

    pub fn build(
        area: Rect,
        input_lines: u16,
        sidebar_visible: bool,
        split_view: bool,
    ) -> Self {
        let sidebar_width: u16 = if sidebar_visible { 20 } else { 0 };
        let max_input_height = 12.min(area.height / 3); // 10 content lines + 2 borders
        let input_height = input_lines.clamp(3, max_input_height.max(3));

        // Split horizontally: sidebar | main
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_width), Constraint::Min(20)])
            .split(area);

        if split_view {
            // Split main horizontally: left (DM) | right (channel)
            let split_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(h_chunks[1]);

            // Left side: messages + input
            let v_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(5), Constraint::Length(input_height)])
                .split(split_chunks[0]);

            Self {
                sidebar: h_chunks[0],
                messages: v_chunks[0],
                input: v_chunks[1],
                channel_panel: Some(split_chunks[1]),
            }
        } else {
            // Normal: messages + input
            let v_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(5), Constraint::Length(input_height)])
                .split(h_chunks[1]);

            Self {
                sidebar: h_chunks[0],
                messages: v_chunks[0],
                input: v_chunks[1],
                channel_panel: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_height_has_minimum_of_three_rows() {
        let area = Rect::new(0, 0, 120, 30);
        let layout = AppLayout::new(area, 1);
        assert_eq!(layout.input.height, 3); // 1 content + 2 borders
    }

    #[test]
    fn input_height_is_capped_to_ten_rows() {
        let area = Rect::new(0, 0, 120, 30);
        let layout = AppLayout::new(area, 20);
        assert_eq!(layout.input.height, 10); // area.height/3 = 10
    }

    #[test]
    fn split_view_creates_channel_panel() {
        let area = Rect::new(0, 0, 120, 30);
        let layout = AppLayout::build(area, 3, true, true);
        assert!(layout.channel_panel.is_some());
        let panel = layout.channel_panel.unwrap();
        assert!(panel.width > 0);
        assert!(panel.height > 0);
    }

    #[test]
    fn no_split_has_no_channel_panel() {
        let area = Rect::new(0, 0, 120, 30);
        let layout = AppLayout::build(area, 3, true, false);
        assert!(layout.channel_panel.is_none());
    }
}

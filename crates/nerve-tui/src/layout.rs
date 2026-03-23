use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub struct AppLayout {
    pub sidebar: Rect,
    pub messages: Rect,
    pub input: Rect,
}

impl AppLayout {
    pub fn new(area: Rect, input_lines: u16) -> Self {
        Self::with_sidebar(area, input_lines, true)
    }

    pub fn with_sidebar(area: Rect, input_lines: u16, sidebar_visible: bool) -> Self {
        let sidebar_width: u16 = if sidebar_visible { 20 } else { 0 };
        let max_input_height = 7.min(area.height / 3); // 5 content lines + 2 borders
        let input_height = input_lines.clamp(3, max_input_height.max(3));

        // Split horizontally: sidebar | main
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_width), Constraint::Min(20)])
            .split(area);

        // Split main vertically: messages | input
        let v_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(input_height)])
            .split(h_chunks[1]);

        Self {
            sidebar: h_chunks[0],
            messages: v_chunks[0],
            input: v_chunks[1],
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
    fn input_height_is_capped_to_seven_rows() {
        let area = Rect::new(0, 0, 120, 30);
        let layout = AppLayout::new(area, 20);
        assert_eq!(layout.input.height, 7); // 5 content + 2 borders
    }
}

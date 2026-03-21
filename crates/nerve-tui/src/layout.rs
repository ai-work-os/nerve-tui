use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub struct AppLayout {
    pub sidebar: Rect,
    pub messages: Rect,
    pub input: Rect,
}

impl AppLayout {
    pub fn new(area: Rect, input_lines: u16) -> Self {
        let sidebar_width: u16 = 20;
        let max_input_height = (area.height / 3).max(2);
        let input_height = input_lines.clamp(2, max_input_height);

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
    fn input_height_has_minimum_of_two_rows() {
        let area = Rect::new(0, 0, 120, 30);
        let layout = AppLayout::new(area, 1);
        assert_eq!(layout.input.height, 2);
    }

    #[test]
    fn input_height_is_capped_to_one_third_of_screen() {
        let area = Rect::new(0, 0, 120, 30);
        let layout = AppLayout::new(area, 20);
        assert_eq!(layout.input.height, 10);
    }
}

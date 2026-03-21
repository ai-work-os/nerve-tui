use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub struct AppLayout {
    pub sidebar: Rect,
    pub messages: Rect,
    pub input: Rect,
}

impl AppLayout {
    pub fn new(area: Rect, input_lines: u16) -> Self {
        let sidebar_width: u16 = 20;
        let input_height = input_lines.clamp(2, area.height / 3);

        // Split horizontally: sidebar | main
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(sidebar_width),
                Constraint::Min(20),
            ])
            .split(area);

        // Split main vertically: messages | input
        let v_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(input_height),
            ])
            .split(h_chunks[1]);

        Self {
            sidebar: h_chunks[0],
            messages: v_chunks[0],
            input: v_chunks[1],
        }
    }
}

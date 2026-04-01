use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub struct AppLayout {
    pub sidebar: Rect,
    pub messages: Rect,
    pub input: Rect,
    /// Right panels for split view. Empty when not in split mode.
    pub panels: Vec<Rect>,
}

/// DM width percentage based on panel count.
fn dm_pct(panel_count: usize) -> u16 {
    match panel_count {
        0 => 100,
        1 => 50,
        2 => 40,
        3 => 35,
        _ => 30,
    }
}

impl AppLayout {
    /// Calculate the inner width of the input box (excluding borders) for a given configuration.
    /// Use this to call visual_line_count consistently with the actual layout width.
    pub fn input_inner_width(area: Rect, sidebar_visible: bool, panel_count: usize) -> u16 {
        let sidebar_width: u16 = if sidebar_visible { 20 } else { 0 };
        let main_w = area.width.saturating_sub(sidebar_width);
        let mut dm_w = main_w * dm_pct(panel_count) / 100;
        // Match build(): shrink DM if panels need min width
        if panel_count > 0 {
            let n = panel_count as u16;
            let min_panels_total = 20 * n;
            if main_w.saturating_sub(dm_w) < min_panels_total && main_w > min_panels_total {
                dm_w = main_w - min_panels_total;
            }
        }
        dm_w.saturating_sub(2) // borders
    }

    pub fn new(area: Rect, input_lines: u16) -> Self {
        Self::build(area, input_lines, true, 0)
    }

    pub fn with_sidebar(area: Rect, input_lines: u16, sidebar_visible: bool) -> Self {
        Self::build(area, input_lines, sidebar_visible, 0)
    }

    pub fn build(
        area: Rect,
        input_lines: u16,
        sidebar_visible: bool,
        panel_count: usize,
    ) -> Self {
        let sidebar_width: u16 = if sidebar_visible { 20 } else { 0 };
        let max_input_height = 12.min(area.height / 3); // 10 content lines + 2 borders
        let input_height = input_lines.clamp(3, max_input_height.max(3));

        // Split horizontally: sidebar | main
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_width), Constraint::Min(20)])
            .split(area);

        if panel_count > 0 {
            let main_w = h_chunks[1].width;
            let n = panel_count as u16;
            let pct = dm_pct(panel_count);
            let mut dm_w = main_w * pct / 100;

            // Ensure each panel gets at least MIN_PANEL_W; shrink DM if needed
            const MIN_PANEL_W: u16 = 20;
            let min_panels_total = MIN_PANEL_W * n;
            if main_w.saturating_sub(dm_w) < min_panels_total && main_w > min_panels_total {
                dm_w = main_w - min_panels_total;
            }
            let remaining = main_w.saturating_sub(dm_w);

            // Distribute remaining evenly with round-robin for leftover pixels
            let base_w = remaining / n;
            let extra = (remaining % n) as usize;

            let mut constraints = vec![Constraint::Length(dm_w)];
            for i in 0..panel_count {
                let w = base_w + if i < extra { 1 } else { 0 };
                constraints.push(Constraint::Length(w));
            }

            let split_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(constraints)
                .split(h_chunks[1]);

            // Left side: messages + input
            let v_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(5), Constraint::Length(input_height)])
                .split(split_chunks[0]);

            let panels: Vec<Rect> = (1..=panel_count).map(|i| split_chunks[i]).collect();

            Self {
                sidebar: h_chunks[0],
                messages: v_chunks[0],
                input: v_chunks[1],
                panels,
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
                panels: Vec::new(),
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
        let layout = AppLayout::build(area, 3, true, 1);
        assert_eq!(layout.panels.len(), 1);
        let panel = layout.panels[0];
        assert!(panel.width > 0);
        assert!(panel.height > 0);
    }

    #[test]
    fn no_split_has_no_channel_panel() {
        let area = Rect::new(0, 0, 120, 30);
        let layout = AppLayout::build(area, 3, true, 0);
        assert!(layout.panels.is_empty());
    }

    // --- Split Step 2: multi-panel layout tests ---

    const TEST_AREA: Rect = Rect { x: 0, y: 0, width: 120, height: 30 };
    const SIDEBAR_W: u16 = 20;

    #[test]
    fn panel_count_0_no_panels_messages_fill() {
        let layout = AppLayout::build(TEST_AREA, 3, true, 0);
        assert!(layout.panels.is_empty());
        // messages should occupy full main width (area - sidebar)
        assert_eq!(layout.messages.width, TEST_AREA.width - SIDEBAR_W);
    }

    #[test]
    fn panel_count_1_single_panel_50_50() {
        let layout = AppLayout::build(TEST_AREA, 3, true, 1);
        assert_eq!(layout.panels.len(), 1);
        let main_w = TEST_AREA.width - SIDEBAR_W; // 100
        let dm_w = main_w * 50 / 100; // 50
        // Allow ±1 for rounding
        assert!((layout.messages.width as i16 - dm_w as i16).unsigned_abs() <= 1);
        assert!((layout.panels[0].width as i16 - (main_w - dm_w) as i16).unsigned_abs() <= 1);
    }

    #[test]
    fn panel_count_2_dm_40_percent() {
        let layout = AppLayout::build(TEST_AREA, 3, true, 2);
        assert_eq!(layout.panels.len(), 2);
        let main_w = TEST_AREA.width - SIDEBAR_W; // 100
        let dm_w = main_w * 40 / 100; // 40
        assert!((layout.messages.width as i16 - dm_w as i16).unsigned_abs() <= 1);
        // Two panels should roughly split the remaining 60
        let remaining = main_w - layout.messages.width;
        for panel in &layout.panels {
            assert!((panel.width as i16 - (remaining / 2) as i16).unsigned_abs() <= 1);
        }
    }

    #[test]
    fn panel_count_3_dm_35_percent() {
        let layout = AppLayout::build(TEST_AREA, 3, true, 3);
        assert_eq!(layout.panels.len(), 3);
        let main_w = TEST_AREA.width - SIDEBAR_W; // 100
        let dm_w = main_w * 35 / 100; // 35
        assert!((layout.messages.width as i16 - dm_w as i16).unsigned_abs() <= 1);
        let remaining = main_w - layout.messages.width;
        for panel in &layout.panels {
            assert!((panel.width as i16 - (remaining / 3) as i16).unsigned_abs() <= 1);
        }
    }

    #[test]
    fn panel_count_4_dm_30_percent() {
        let layout = AppLayout::build(TEST_AREA, 3, true, 4);
        assert_eq!(layout.panels.len(), 4);
        let main_w = TEST_AREA.width - SIDEBAR_W; // 100
        // DM may shrink below 30% to guarantee min panel width (20px each)
        let min_panels_total: u16 = 20 * 4; // 80
        let expected_dm = if main_w * 30 / 100 + min_panels_total > main_w {
            main_w - min_panels_total // 20
        } else {
            main_w * 30 / 100 // 30
        };
        assert!((layout.messages.width as i16 - expected_dm as i16).unsigned_abs() <= 1);
        let remaining = main_w - layout.messages.width;
        for panel in &layout.panels {
            assert!((panel.width as i16 - (remaining / 4) as i16).unsigned_abs() <= 1);
        }
    }

    #[test]
    fn input_inner_width_adapts_to_panel_count() {
        let area = TEST_AREA;
        let main_w = area.width - SIDEBAR_W; // 100

        let w0 = AppLayout::input_inner_width(area, true, 0);
        let w1 = AppLayout::input_inner_width(area, true, 1);
        let w2 = AppLayout::input_inner_width(area, true, 2);

        // panel_count=0: full width minus borders
        assert_eq!(w0, main_w - 2); // 98
        // panel_count=1: 50% minus borders
        assert_eq!(w1, main_w * 50 / 100 - 2); // 48
        // panel_count=2: 40% minus borders
        assert_eq!(w2, main_w * 40 / 100 - 2); // 38
        // More panels → narrower input
        assert!(w0 > w1);
        assert!(w1 > w2);
    }

    #[test]
    fn all_panels_have_minimum_width() {
        let min_panel_width: u16 = 20;
        for panel_count in 1..=4 {
            let layout = AppLayout::build(TEST_AREA, 3, true, panel_count);
            for (i, panel) in layout.panels.iter().enumerate() {
                assert!(
                    panel.width >= min_panel_width,
                    "panel {} with panel_count={} has width {} < {}",
                    i, panel_count, panel.width, min_panel_width
                );
            }
        }
    }

    #[test]
    fn panels_cover_full_width_no_gaps() {
        for panel_count in 1..=4 {
            let layout = AppLayout::build(TEST_AREA, 3, true, panel_count);
            let main_w = TEST_AREA.width - SIDEBAR_W;
            let total: u16 = layout.messages.width + layout.panels.iter().map(|p| p.width).sum::<u16>();
            // Allow ±1 for rounding
            assert!(
                (total as i16 - main_w as i16).unsigned_abs() <= 1,
                "panel_count={}: total width {} != main {}",
                panel_count, total, main_w
            );
        }
    }
}

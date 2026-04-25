use nerve_tui_core::Transport;
use ratatui::Frame;

use super::app_state::{SplitFocus, SplitTarget};
use super::App;
use crate::components::channel_view;
use crate::layout::AppLayout;
use crate::theme;

impl<T: Transport> App<T> {
    pub(crate) fn render(&mut self, frame: &mut Frame) {
        // Advance blink tick for streaming cursor
        self.dm_view.tick_blink();

        let area = frame.area();

        // Fill entire area with L0 background
        let buf = frame.buffer_mut();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_bg(theme::BG_L0);
                }
            }
        }

        let panel_count = self.split_panels.len();
        let input_inner_w = AppLayout::input_inner_width(area, self.sidebar_visible, panel_count);
        let input_lines = self.input.visual_line_count(input_inner_w) + 1; // +1 for top padding
        let layout = AppLayout::build(area, input_lines, self.sidebar_visible, panel_count);

        // Sidebar: channels + agents (skip when hidden)
        if self.sidebar_visible {
            self.status_bar.render(
                &self.channels,
                self.active_channel.as_deref(),
                &self.agents,
                self.dm_node_name(),
                self.project_name.as_deref(),
                self.global_mode,
                layout.sidebar,
                frame.buffer_mut(),
            );
        }

        // Messages (DM panel in split mode)
        if self.is_dm_mode() {
            self.dm_view.render(layout.messages, frame.buffer_mut());
        } else {
            self.channel_view.render(layout.messages, frame.buffer_mut());
        }

        // Right panels (split view): channel or node output
        self.panel_x_boundaries.clear();
        for (i, panel_area) in layout.panels.iter().enumerate() {
            self.panel_x_boundaries.push(panel_area.x);
            if let Some(panel) = self.split_panels.get_mut(i) {
                let focused = self.split_focus == SplitFocus::Panel(i);
                match &panel.target {
                    SplitTarget::Channel => {
                        let channel_name = self
                            .channels
                            .iter()
                            .find(|c| Some(&c.id) == self.active_channel.as_ref())
                            .map(|c| c.display_name())
                            .unwrap_or("channel");
                        self.channel_view.render_panel(
                            channel_name,
                            &mut panel.panel_state,
                            focused,
                            *panel_area,
                            frame.buffer_mut(),
                        );
                    }
                    SplitTarget::Node { node_name, .. } => {
                        let title = format!("@{}", node_name);
                        let buf = &panel.node_buffer;
                        channel_view::render_text_panel(
                            &title,
                            buf,
                            &mut panel.panel_state,
                            focused,
                            *panel_area,
                            frame.buffer_mut(),
                        );
                    }
                }
            }
        }

        // Build metadata text for input box
        let meta_left = if self.is_dm_mode() {
            let model = self.dm_view.model_label.as_deref().unwrap_or("");
            let status = if self.dm_view.is_responding { "回复中..." } else { "" };
            if status.is_empty() {
                model.to_string()
            } else {
                format!("{} · {}", model, status)
            }
        } else {
            String::new()
        };
        self.input.render_with_meta(layout.input, frame.buffer_mut(), &meta_left);
        self.input.render_popup(layout.input, frame.buffer_mut());

        // Cursor
        let (cx, cy) = self.input.cursor_position(layout.input);
        frame.set_cursor_position((cx, cy));
    }
}

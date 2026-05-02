use nerve_tui_core::Transport;
use ratatui::style::Style;
use ratatui::Frame;

use super::app_state::{SplitFocus, SplitTarget};
use super::App;
use crate::components::channel_view;
use crate::components::spinner::KnightRiderScanner;
use crate::layout::AppLayout;
use crate::theme;

impl<T: Transport> App<T> {
    pub(crate) fn render(&mut self, frame: &mut Frame) {
        // Advance blink tick for streaming cursor and spinner animation
        self.dm_view.tick_blink();
        self.spinner.advance();
        let spinner_frame = self.spinner.frame().to_string();
        self.dm_view.spinner_frame = spinner_frame;

        let area = frame.area();

        // Fill entire area with L0 background
        let bg = theme::current().background;
        let buf = frame.buffer_mut();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_bg(bg);
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

        // Build metadata text and agent color for input box
        let (meta_left, agent_c) = if self.is_dm_mode() {
            let t = theme::current();
            let agent_name = self.dm_view.agent_name();
            let model = self.dm_view.model_label.as_deref().unwrap_or("");
            let status = if self.dm_view.is_responding { "回复中..." } else { "" };
            let meta = if status.is_empty() {
                format!("{} · {}", agent_name, model)
            } else {
                format!("{} · {} · {}", agent_name, model, status)
            };
            let color = t.agent_color(agent_name);
            (meta, Some(color))
        } else {
            (String::new(), None)
        };
        self.input.render_with_meta(layout.input, frame.buffer_mut(), &meta_left, agent_c);
        self.input.render_popup(layout.input, frame.buffer_mut());

        // Knight Rider scanner overlay on metadata line when agent is responding
        if self.dm_view.is_responding {
            let scanner_width = layout.input.width.saturating_sub(4) as usize; // -4 for padding
            if self.scanner.width != scanner_width {
                self.scanner = KnightRiderScanner::new(scanner_width);
            }
            self.scanner.advance();

            let t = theme::current();
            let agent_c = t.agent_color(self.dm_view.agent_name());
            let meta_y = layout.input.y + layout.input.height - 1;
            let start_x = layout.input.x + 2; // after left border
            let end_x = layout.input.x + layout.input.width.saturating_sub(10); // leave room for "esc 中断"

            let spans = self.scanner.render_spans(agent_c);
            let buf = frame.buffer_mut();
            for (i, (ch, color)) in spans.iter().enumerate() {
                let x = start_x + i as u16;
                if x < end_x {
                    if let Some(cell) = buf.cell_mut((x, meta_y)) {
                        cell.set_char(*ch);
                        cell.set_fg(*color);
                        cell.set_bg(t.background_element);
                    }
                }
            }

            // "esc 中断" hint on the right
            let hint = "esc 中断";
            let hint_x = layout.input.x + layout.input.width.saturating_sub(10);
            buf.set_string(
                hint_x,
                meta_y,
                hint,
                Style::default().fg(t.text_muted).bg(t.background_element),
            );
        }

        // Cursor
        let (cx, cy) = self.input.cursor_position_with_border(layout.input, agent_c.is_some());
        frame.set_cursor_position((cx, cy));
    }
}

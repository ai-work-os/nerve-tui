use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

use crate::theme;

#[derive(Debug, Clone)]
pub struct AgentDisplay {
    pub name: String,
    pub status: String,
    pub activity: Option<String>,
    pub adapter: Option<String>,
    pub node_id: String,
}

#[derive(Debug, Clone)]
pub struct ChannelDisplay {
    pub id: String,
    pub name: Option<String>,
    pub node_count: usize,
}

impl ChannelDisplay {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

pub struct StatusBar {
    /// Selected agent tab: 0 = "全部", 1+ = agent index
    pub selected_tab: usize,
    /// Selected channel index
    pub selected_channel: usize,
}

impl StatusBar {
    pub fn new() -> Self {
        Self {
            selected_tab: 0,
            selected_channel: 0,
        }
    }

    /// Tab count = 1 (全部) + agents.len()
    pub fn tab_count(agents: &[AgentDisplay]) -> usize {
        1 + agents.len()
    }

    pub fn select_next_tab(&mut self, agents: &[AgentDisplay]) {
        let total = Self::tab_count(agents);
        if total > 0 {
            self.selected_tab = (self.selected_tab + 1) % total;
        }
    }

    pub fn select_prev_tab(&mut self, agents: &[AgentDisplay]) {
        let total = Self::tab_count(agents);
        if total > 0 {
            self.selected_tab = if self.selected_tab == 0 {
                total - 1
            } else {
                self.selected_tab - 1
            };
        }
    }

    pub fn select_next_channel(&mut self, channels: &[ChannelDisplay]) {
        if !channels.is_empty() {
            self.selected_channel = (self.selected_channel + 1) % channels.len();
        }
    }

    pub fn select_prev_channel(&mut self, channels: &[ChannelDisplay]) {
        if !channels.is_empty() {
            self.selected_channel = if self.selected_channel == 0 {
                channels.len() - 1
            } else {
                self.selected_channel - 1
            };
        }
    }

    /// Get the current filter name: None = show all, Some(name) = filter by agent
    pub fn current_filter(&self, agents: &[AgentDisplay]) -> Option<String> {
        if self.selected_tab == 0 {
            None
        } else {
            agents.get(self.selected_tab - 1).map(|a| a.name.clone())
        }
    }

    pub fn render(
        &self,
        channels: &[ChannelDisplay],
        active_channel: Option<&str>,
        agents: &[AgentDisplay],
        active_dm: Option<&str>,
        area: Rect,
        buf: &mut Buffer,
    ) {
        let block = Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(theme::BORDER));

        let inner = block.inner(area);
        block.render(area, buf);

        let mut lines: Vec<Line> = Vec::new();

        // --- Channels section ---
        lines.push(Line::from(Span::styled(
            "频道",
            Style::default()
                .fg(theme::TITLE)
                .add_modifier(Modifier::BOLD),
        )));

        if channels.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (无)",
                Style::default().fg(theme::SYSTEM_MSG),
            )));
        } else {
            for (i, ch) in channels.iter().enumerate() {
                let is_active = active_channel.map_or(false, |id| id == ch.id);
                let is_selected = i == self.selected_channel;
                let marker = if is_selected { "▸" } else { " " };

                let name_color = if is_active {
                    theme::CHANNEL_ACTIVE
                } else {
                    theme::CHANNEL_INACTIVE
                };
                let name_style = if is_active {
                    Style::default()
                        .fg(name_color)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(name_color)
                };

                let display = ch.display_name();
                // Truncate channel name to fit
                let max_w = inner.width.saturating_sub(4) as usize;
                let truncated: String = if display.len() > max_w {
                    format!("{}…", &display[..max_w.saturating_sub(1)])
                } else {
                    display.to_string()
                };

                lines.push(Line::from(vec![
                    Span::raw(format!("{} ", marker)),
                    Span::styled(truncated, name_style),
                    Span::styled(
                        format!(" ({})", ch.node_count),
                        Style::default().fg(theme::TIMESTAMP),
                    ),
                ]));
            }
        }

        // --- DM indicator ---
        if let Some(dm_name) = active_dm {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "DM",
                Style::default()
                    .fg(theme::TITLE)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(vec![
                Span::styled("▸ ", Style::default().fg(theme::MENTION)),
                Span::styled(
                    dm_name.to_string(),
                    Style::default()
                        .fg(theme::MENTION)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }

        // --- Separator ---
        lines.push(Line::from(""));

        // --- Agent tabs section ---
        lines.push(Line::from(Span::styled(
            "消息",
            Style::default()
                .fg(theme::TITLE)
                .add_modifier(Modifier::BOLD),
        )));

        // Tab 0: "全部"
        {
            let is_selected = self.selected_tab == 0;
            let marker = if is_selected { "▸" } else { " " };
            let style = if is_selected {
                Style::default()
                    .fg(theme::TITLE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::SYSTEM_MSG)
            };
            lines.push(Line::from(vec![
                Span::raw(format!("{} ", marker)),
                Span::styled("全部", style),
            ]));
        }

        // Agent tabs
        for (i, agent) in agents.iter().enumerate() {
            let tab_idx = i + 1;
            let is_selected = self.selected_tab == tab_idx;
            let icon = theme::status_icon(&agent.status);
            let color = theme::status_color(&agent.status);

            let marker = if is_selected { "▸" } else { " " };
            let name_style = if is_selected {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };

            // Line 1: marker + icon + name
            lines.push(Line::from(vec![
                Span::raw(format!("{} ", marker)),
                Span::styled(format!("{} ", icon), Style::default().fg(color)),
                Span::styled(agent.name.clone(), name_style),
            ]));

            // Line 2: activity/adapter detail
            if let Some(ref activity) = agent.activity {
                lines.push(Line::from(Span::styled(
                    format!("    {}", activity),
                    Style::default().fg(theme::TIMESTAMP),
                )));
            } else if let Some(ref adapter) = agent.adapter {
                lines.push(Line::from(Span::styled(
                    format!("    {}", adapter),
                    Style::default().fg(theme::TIMESTAMP),
                )));
            }
        }

        Paragraph::new(lines).render(inner, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_agents(n: usize) -> Vec<AgentDisplay> {
        (0..n)
            .map(|i| AgentDisplay {
                name: format!("agent-{}", i),
                status: "idle".to_string(),
                activity: None,
                adapter: Some("claude".to_string()),
                node_id: format!("n{}", i),
            })
            .collect()
    }

    fn make_channels(n: usize) -> Vec<ChannelDisplay> {
        (0..n)
            .map(|i| ChannelDisplay {
                id: format!("ch{}", i),
                name: Some(format!("channel-{}", i)),
                node_count: i + 1,
            })
            .collect()
    }

    #[test]
    fn new_starts_at_zero() {
        let bar = StatusBar::new();
        assert_eq!(bar.selected_tab, 0);
        assert_eq!(bar.selected_channel, 0);
    }

    #[test]
    fn tab_count_includes_all() {
        let agents = make_agents(3);
        assert_eq!(StatusBar::tab_count(&agents), 4); // 全部 + 3 agents
        assert_eq!(StatusBar::tab_count(&[]), 1); // just 全部
    }

    #[test]
    fn select_next_tab_wraps() {
        let mut bar = StatusBar::new();
        let agents = make_agents(2);
        bar.select_next_tab(&agents); // 0 -> 1
        assert_eq!(bar.selected_tab, 1);
        bar.select_next_tab(&agents); // 1 -> 2
        assert_eq!(bar.selected_tab, 2);
        bar.select_next_tab(&agents); // 2 -> 0 (wrap)
        assert_eq!(bar.selected_tab, 0);
    }

    #[test]
    fn select_prev_tab_wraps() {
        let mut bar = StatusBar::new();
        let agents = make_agents(2);
        bar.select_prev_tab(&agents); // 0 -> 2 (wrap)
        assert_eq!(bar.selected_tab, 2);
        bar.select_prev_tab(&agents); // 2 -> 1
        assert_eq!(bar.selected_tab, 1);
    }

    #[test]
    fn select_channel_wraps() {
        let mut bar = StatusBar::new();
        let channels = make_channels(2);
        bar.select_next_channel(&channels); // 0 -> 1
        assert_eq!(bar.selected_channel, 1);
        bar.select_next_channel(&channels); // 1 -> 0
        assert_eq!(bar.selected_channel, 0);

        bar.select_prev_channel(&channels); // 0 -> 1
        assert_eq!(bar.selected_channel, 1);
    }

    #[test]
    fn current_filter() {
        let mut bar = StatusBar::new();
        let agents = make_agents(2);
        assert_eq!(bar.current_filter(&agents), None); // tab 0 = all

        bar.selected_tab = 1;
        assert_eq!(bar.current_filter(&agents), Some("agent-0".to_string()));

        bar.selected_tab = 2;
        assert_eq!(bar.current_filter(&agents), Some("agent-1".to_string()));

        bar.selected_tab = 99; // out of range
        assert_eq!(bar.current_filter(&agents), None);
    }

    #[test]
    fn select_with_zero_total_is_noop() {
        let mut bar = StatusBar::new();
        bar.select_next_channel(&[]);
        assert_eq!(bar.selected_channel, 0);
        bar.select_prev_channel(&[]);
        assert_eq!(bar.selected_channel, 0);
    }

    #[test]
    fn channel_display_name() {
        let ch_named = ChannelDisplay {
            id: "ch1".into(),
            name: Some("main".into()),
            node_count: 2,
        };
        assert_eq!(ch_named.display_name(), "main");

        let ch_unnamed = ChannelDisplay {
            id: "ch-abc-123".into(),
            name: None,
            node_count: 0,
        };
        assert_eq!(ch_unnamed.display_name(), "ch-abc-123");
    }

    #[test]
    fn render_empty_no_panic() {
        let bar = StatusBar::new();
        let area = Rect::new(0, 0, 20, 15);
        let mut buf = Buffer::empty(area);
        bar.render(&[], None, &[], None, area, &mut buf);
    }

    #[test]
    fn render_with_data_no_panic() {
        let bar = StatusBar::new();
        let channels = make_channels(2);
        let agents = make_agents(3);
        let area = Rect::new(0, 0, 24, 20);
        let mut buf = Buffer::empty(area);
        bar.render(&channels, Some("ch0"), &agents, None, area, &mut buf);
    }
}

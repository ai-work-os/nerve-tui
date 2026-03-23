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
pub struct MemberDisplay {
    pub node_id: String,
}

#[derive(Debug, Clone)]
pub struct ChannelDisplay {
    pub id: String,
    pub name: Option<String>,
    pub node_count: usize,
    pub members: Vec<MemberDisplay>,
}

impl ChannelDisplay {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationTarget {
    Channel(usize),
    Agent(usize),
}

pub struct StatusBar {
    /// Unified navigation selection: channels first, then agents.
    pub selected_nav: usize,
}

impl StatusBar {
    pub fn new() -> Self {
        Self { selected_nav: 0 }
    }

    pub fn nav_count(channels: &[ChannelDisplay], agents: &[AgentDisplay]) -> usize {
        channels.len() + agents.len()
    }

    pub fn select_next_item(&mut self, channels: &[ChannelDisplay], agents: &[AgentDisplay]) {
        let total = Self::nav_count(channels, agents);
        if total > 0 {
            self.selected_nav = (self.selected_nav + 1) % total;
        }
    }

    pub fn select_prev_item(&mut self, channels: &[ChannelDisplay], agents: &[AgentDisplay]) {
        let total = Self::nav_count(channels, agents);
        if total > 0 {
            self.selected_nav = if self.selected_nav == 0 {
                total - 1
            } else {
                self.selected_nav - 1
            };
        }
    }

    pub fn selected_target(
        &self,
        channels: &[ChannelDisplay],
        agents: &[AgentDisplay],
    ) -> Option<NavigationTarget> {
        if self.selected_nav < channels.len() {
            Some(NavigationTarget::Channel(self.selected_nav))
        } else {
            let agent_idx = self.selected_nav.checked_sub(channels.len())?;
            if agent_idx < agents.len() {
                Some(NavigationTarget::Agent(agent_idx))
            } else {
                None
            }
        }
    }

    pub fn sync_to_context(
        &mut self,
        channels: &[ChannelDisplay],
        active_channel: Option<&str>,
        agents: &[AgentDisplay],
        active_dm: Option<&str>,
    ) {
        if let Some(dm_name) = active_dm {
            if let Some(agent_idx) = agents.iter().position(|a| a.name == dm_name) {
                self.selected_nav = channels.len() + agent_idx;
                return;
            }
        }
        if let Some(channel_id) = active_channel {
            if let Some(channel_idx) = channels.iter().position(|c| c.id == channel_id) {
                self.selected_nav = channel_idx;
                return;
            }
        }

        let total = Self::nav_count(channels, agents);
        if total == 0 {
            self.selected_nav = 0;
        } else if self.selected_nav >= total {
            self.selected_nav = total - 1;
        }
    }

    pub fn render(
        &self,
        channels: &[ChannelDisplay],
        active_channel: Option<&str>,
        agents: &[AgentDisplay],
        active_dm: Option<&str>,
        project_name: Option<&str>,
        global_mode: bool,
        area: Rect,
        buf: &mut Buffer,
    ) {
        let block = Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(theme::BORDER));

        let inner = block.inner(area);
        block.render(area, buf);

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            "导航",
            Style::default()
                .fg(theme::TITLE)
                .add_modifier(Modifier::BOLD),
        )));

        if global_mode {
            lines.push(Line::from(Span::styled(
                "全局模式",
                Style::default().fg(theme::MENTION),
            )));
            lines.push(Line::from(""));
        } else if let Some(project) = project_name {
            lines.push(Line::from(vec![
                Span::styled("项目 ", Style::default().fg(theme::TIMESTAMP)),
                Span::styled(
                    project.to_string(),
                    Style::default()
                        .fg(theme::TITLE)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(""));
        }

        if channels.is_empty() && agents.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (无)",
                Style::default().fg(theme::SYSTEM_MSG),
            )));
            Paragraph::new(lines).render(inner, buf);
            return;
        }

        let selected = self.selected_target(channels, agents);

        for (i, ch) in channels.iter().enumerate() {
            let is_selected = selected == Some(NavigationTarget::Channel(i));
            let is_active = active_channel == Some(ch.id.as_str()) && active_dm.is_none();
            let marker = if is_selected { "▸" } else { " " };
            let base_style = if is_active {
                Style::default()
                    .fg(theme::CHANNEL_ACTIVE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::CHANNEL_INACTIVE)
            };
            let name_style = if is_selected {
                base_style.add_modifier(Modifier::BOLD)
            } else {
                base_style
            };

            let display = ch.display_name();
            let max_w = inner.width.saturating_sub(6) as usize;
            let char_count = display.chars().count();
            let truncated = if char_count > max_w {
                let s: String = display.chars().take(max_w.saturating_sub(1)).collect();
                format!("{}…", s)
            } else {
                display.to_string()
            };

            let busy_count = ch.members.iter().filter(|m| {
                agents.iter().any(|a| a.node_id == m.node_id && a.status == "busy")
            }).count();
            let count_text = if busy_count > 0 {
                format!(" ({}/{}busy)", ch.node_count, busy_count)
            } else {
                format!(" ({})", ch.node_count)
            };
            lines.push(Line::from(vec![
                Span::raw(format!("{} ", marker)),
                Span::styled(format!("#{}", truncated), name_style),
                Span::styled(count_text, Style::default().fg(theme::TIMESTAMP)),
            ]));
            // Show members under active/selected channel
            if is_active || is_selected {
                for member in &ch.members {
                    let agent = agents.iter().find(|a| a.node_id == member.node_id);
                    let status = agent.map(|a| a.status.as_str()).unwrap_or("idle");
                    let name = agent.map(|a| a.name.as_str()).unwrap_or("?");
                    let icon = theme::status_icon(status);
                    let color = theme::status_color(status);
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(
                            format!("{} {}", icon, name),
                            Style::default().fg(color),
                        ),
                    ]));
                }
            }
        }

        if !channels.is_empty() && !agents.is_empty() {
            lines.push(Line::from(""));
        }

        for (i, agent) in agents.iter().enumerate() {
            let is_selected = selected == Some(NavigationTarget::Agent(i));
            let is_active = active_dm == Some(agent.name.as_str());
            let marker = if is_selected { "▸" } else { " " };
            let color = if is_active {
                theme::MENTION
            } else {
                theme::status_color(&agent.status)
            };
            let mut name_style = Style::default().fg(color);
            if is_selected || is_active {
                name_style = name_style.add_modifier(Modifier::BOLD);
            }

            lines.push(Line::from(vec![
                Span::raw(format!("{} ", marker)),
                Span::styled(
                    format!("{} ", theme::status_icon(&agent.status)),
                    Style::default().fg(color),
                ),
                Span::styled(format!("@{}", agent.name), name_style),
                Span::styled(
                    if is_active { " [DM]" } else { "" },
                    Style::default().fg(theme::TIMESTAMP),
                ),
            ]));

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
                members: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn new_starts_at_zero() {
        let bar = StatusBar::new();
        assert_eq!(bar.selected_nav, 0);
    }

    #[test]
    fn nav_count_counts_channels_and_agents() {
        let channels = make_channels(2);
        let agents = make_agents(3);
        assert_eq!(StatusBar::nav_count(&channels, &agents), 5);
        assert_eq!(StatusBar::nav_count(&[], &[]), 0);
    }

    #[test]
    fn select_next_item_wraps() {
        let mut bar = StatusBar::new();
        let channels = make_channels(1);
        let agents = make_agents(2);

        bar.select_next_item(&channels, &agents);
        assert_eq!(bar.selected_nav, 1);
        bar.select_next_item(&channels, &agents);
        assert_eq!(bar.selected_nav, 2);
        bar.select_next_item(&channels, &agents);
        assert_eq!(bar.selected_nav, 0);
    }

    #[test]
    fn select_prev_item_wraps() {
        let mut bar = StatusBar::new();
        let channels = make_channels(1);
        let agents = make_agents(2);

        bar.select_prev_item(&channels, &agents);
        assert_eq!(bar.selected_nav, 2);
        bar.select_prev_item(&channels, &agents);
        assert_eq!(bar.selected_nav, 1);
    }

    #[test]
    fn selected_target_maps_channels_and_agents() {
        let mut bar = StatusBar::new();
        let channels = make_channels(2);
        let agents = make_agents(2);

        assert_eq!(
            bar.selected_target(&channels, &agents),
            Some(NavigationTarget::Channel(0))
        );

        bar.selected_nav = 2;
        assert_eq!(
            bar.selected_target(&channels, &agents),
            Some(NavigationTarget::Agent(0))
        );
    }

    #[test]
    fn sync_to_context_prefers_active_dm() {
        let mut bar = StatusBar::new();
        let channels = make_channels(2);
        let agents = make_agents(2);

        bar.sync_to_context(&channels, Some("ch1"), &agents, Some("agent-0"));

        assert_eq!(bar.selected_nav, channels.len());
    }

    #[test]
    fn sync_to_context_falls_back_to_active_channel() {
        let mut bar = StatusBar::new();
        let channels = make_channels(2);
        let agents = make_agents(2);

        bar.sync_to_context(&channels, Some("ch1"), &agents, None);

        assert_eq!(bar.selected_nav, 1);
    }

    #[test]
    fn select_with_zero_total_is_noop() {
        let mut bar = StatusBar::new();
        bar.select_next_item(&[], &[]);
        assert_eq!(bar.selected_nav, 0);
        bar.select_prev_item(&[], &[]);
        assert_eq!(bar.selected_nav, 0);
    }

    #[test]
    fn channel_display_name() {
        let ch_named = ChannelDisplay {
            id: "ch1".into(),
            name: Some("main".into()),
            node_count: 2,
            members: Vec::new(),
        };
        assert_eq!(ch_named.display_name(), "main");

        let ch_unnamed = ChannelDisplay {
            id: "ch-abc-123".into(),
            name: None,
            node_count: 0,
            members: Vec::new(),
        };
        assert_eq!(ch_unnamed.display_name(), "ch-abc-123");
    }

    #[test]
    fn render_empty_no_panic() {
        let bar = StatusBar::new();
        let area = Rect::new(0, 0, 20, 15);
        let mut buf = Buffer::empty(area);
        bar.render(&[], None, &[], None, None, false, area, &mut buf);
    }

    #[test]
    fn render_with_data_no_panic() {
        let bar = StatusBar::new();
        let channels = make_channels(2);
        let agents = make_agents(3);
        let area = Rect::new(0, 0, 24, 20);
        let mut buf = Buffer::empty(area);
        bar.render(
            &channels,
            Some("ch0"),
            &agents,
            Some("agent-1"),
            Some("nerve-tui"),
            false,
            area,
            &mut buf,
        );
    }

    #[test]
    fn channel_members_render_no_panic() {
        let bar = StatusBar::new();
        let channels = vec![ChannelDisplay {
            id: "ch0".into(),
            name: Some("main".into()),
            node_count: 2,
            members: vec![
                MemberDisplay { node_id: "n0".into() },
                MemberDisplay { node_id: "n1".into() },
            ],
        }];
        let agents = make_agents(1);
        let area = Rect::new(0, 0, 30, 20);
        let mut buf = Buffer::empty(area);
        bar.render(&channels, Some("ch0"), &agents, None, None, false, area, &mut buf);
    }

    #[test]
    fn channel_busy_count_display() {
        let ch = ChannelDisplay {
            id: "ch1".into(),
            name: Some("test".into()),
            node_count: 3,
            members: vec![
                MemberDisplay { node_id: "n1".into() },
                MemberDisplay { node_id: "n2".into() },
                MemberDisplay { node_id: "n3".into() },
            ],
        };
        let agents = vec![
            AgentDisplay { name: "a".into(), status: "idle".into(), activity: None, adapter: None, node_id: "n1".into() },
            AgentDisplay { name: "b".into(), status: "busy".into(), activity: None, adapter: None, node_id: "n2".into() },
            AgentDisplay { name: "c".into(), status: "busy".into(), activity: None, adapter: None, node_id: "n3".into() },
        ];
        let busy = ch.members.iter().filter(|m| {
            agents.iter().any(|a| a.node_id == m.node_id && a.status == "busy")
        }).count();
        assert_eq!(busy, 2);
    }
}

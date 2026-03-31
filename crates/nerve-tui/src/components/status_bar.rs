use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};
use std::time::Instant;

use unicode_width::UnicodeWidthStr;

use crate::theme;

#[derive(Debug, Clone)]
pub struct AgentDisplay {
    pub name: String,
    pub status: String,
    pub activity: Option<String>,
    pub adapter: Option<String>,
    pub node_id: String,
    pub transport: String,
    pub capabilities: Vec<String>,
    pub usage: Option<(f64, f64, f64)>, // (token_used, token_size, cost)
    /// Currently executing tool call name
    pub tool_call_name: Option<String>,
    /// When the current tool call started
    pub tool_call_started: Option<Instant>,
    /// Agent this agent is waiting for (e.g. "→main")
    pub waiting_for: Option<String>,
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
    pub unread: usize,
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
            .border_type(BorderType::Rounded)
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
            let mut spans = vec![
                Span::raw(format!("{} ", marker)),
                Span::styled(format!("#{}", truncated), name_style),
                Span::styled(count_text, Style::default().fg(theme::TIMESTAMP)),
            ];
            if ch.unread > 0 {
                spans.push(Span::styled(
                    format!(" {}", ch.unread),
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            lines.push(Line::from(spans));
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

        // Group agents by type: AI (stdio), Programs (ws + monitor cap), Clients (ws other)
        let ai_agents: Vec<(usize, &AgentDisplay)> = agents.iter().enumerate()
            .filter(|(_, a)| a.transport == "stdio")
            .collect();
        let program_agents: Vec<(usize, &AgentDisplay)> = agents.iter().enumerate()
            .filter(|(_, a)| a.transport != "stdio" && a.capabilities.iter().any(|c| c == "monitor"))
            .collect();
        let client_agents: Vec<(usize, &AgentDisplay)> = agents.iter().enumerate()
            .filter(|(_, a)| a.transport != "stdio" && !a.capabilities.iter().any(|c| c == "monitor"))
            .collect();

        let mut has_prev = !channels.is_empty();

        // Render AI agents section
        if !ai_agents.is_empty() {
            if has_prev { lines.push(Line::from("")); }
            lines.push(Line::from(Span::styled(
                "AI Agents",
                Style::default().fg(theme::TIMESTAMP).add_modifier(Modifier::BOLD),
            )));
            for (i, agent) in &ai_agents {
                Self::render_agent_item(&mut lines, *i, agent, &selected, active_dm, inner.width);
            }
            has_prev = true;
        }

        // Render program nodes section
        if !program_agents.is_empty() {
            if has_prev { lines.push(Line::from("")); }
            lines.push(Line::from(Span::styled(
                "Programs",
                Style::default().fg(theme::TIMESTAMP).add_modifier(Modifier::BOLD),
            )));
            for (i, agent) in &program_agents {
                Self::render_agent_item(&mut lines, *i, agent, &selected, active_dm, inner.width);
            }
            has_prev = true;
        }

        // Render client nodes section
        if !client_agents.is_empty() {
            if has_prev { lines.push(Line::from("")); }
            lines.push(Line::from(Span::styled(
                "Clients",
                Style::default().fg(theme::TIMESTAMP).add_modifier(Modifier::BOLD),
            )));
            for (i, agent) in &client_agents {
                Self::render_agent_item(&mut lines, *i, agent, &selected, active_dm, inner.width);
            }
        }

        Paragraph::new(lines).render(inner, buf);
    }

    /// Build the second line for an agent: `adapter [hint]` or just `adapter`.
    /// Hint: tool_call > activity. Fits within `max_width` columns.
    pub fn agent_status_line(agent: &AgentDisplay, max_width: usize) -> String {
        let adapter = agent.adapter.as_deref().unwrap_or("");

        // Determine hint: tool_call > activity > none
        let hint = if let Some(ref tool) = agent.tool_call_name {
            Some(format!("[{}]", tool))
        } else if let Some(ref activity) = agent.activity {
            Some(format!("[{}]", activity))
        } else {
            None
        };

        match hint {
            None => truncate_str(adapter, max_width),
            Some(hint) => {
                if adapter.is_empty() {
                    return truncate_str(&hint, max_width);
                }
                let adapter_w = adapter.width();
                let hint_w = hint.width();
                let total = adapter_w + 1 + hint_w;

                if total <= max_width {
                    format!("{} {}", adapter, hint)
                } else {
                    // Truncate adapter first, keep hint visible (min 4 cols)
                    let min_hint = 4;
                    let hint_budget = hint_w.min(max_width.saturating_sub(1)); // at least try full hint
                    let adapter_budget = max_width.saturating_sub(1 + hint_budget);
                    if adapter_budget >= 2 && hint_budget >= min_hint {
                        format!("{} {}", truncate_str(adapter, adapter_budget), truncate_str(&hint, hint_budget))
                    } else if max_width > hint_w {
                        // Just show hint
                        truncate_str(&hint, max_width)
                    } else {
                        truncate_str(&hint, max_width)
                    }
                }
            }
        }
    }

    fn render_agent_item(
        lines: &mut Vec<Line<'_>>,
        i: usize,
        agent: &AgentDisplay,
        selected: &Option<NavigationTarget>,
        active_dm: Option<&str>,
        width: u16,
    ) {
        let is_selected = *selected == Some(NavigationTarget::Agent(i));
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

        // Line 1: "▸ ● agent-name [DM]"
        let prefix_len: usize = 4; // "▸ ● "
        let suffix = if is_active { " [DM]" } else { "" };
        let suffix_len = suffix.width();
        let name_budget = (width as usize).saturating_sub(prefix_len + suffix_len);
        let name_display = truncate_str(&agent.name, name_budget);

        let mut spans = vec![
            Span::raw(format!("{} ", marker)),
            Span::styled(
                format!("{} ", theme::status_icon(&agent.status)),
                Style::default().fg(color),
            ),
            Span::styled(name_display, name_style),
        ];
        if !suffix.is_empty() {
            spans.push(Span::styled(
                suffix.to_string(),
                Style::default().fg(theme::TIMESTAMP),
            ));
        }
        lines.push(Line::from(spans));

        // Line 2: "  adapter [hint]"
        let indent = "  ";
        let indent_len = indent.width();
        let status_budget = (width as usize).saturating_sub(indent_len);
        let status_text = Self::agent_status_line(agent, status_budget);

        if !status_text.is_empty() {
            // Split into adapter part and hint part for coloring
            let (adapter_part, hint_part) = if let Some(pos) = status_text.find('[') {
                let idx = status_text[..pos].trim_end().len();
                (&status_text[..idx], Some(status_text[idx..].trim_start()))
            } else {
                (status_text.as_str(), None)
            };

            let mut line2_spans = vec![
                Span::styled(indent, Style::default()),
            ];
            if !adapter_part.is_empty() {
                line2_spans.push(Span::styled(
                    adapter_part.to_string(),
                    Style::default().fg(theme::TIMESTAMP),
                ));
            }
            if let Some(hint) = hint_part {
                let hint_color = if agent.tool_call_name.is_some() {
                    theme::TOOL_NAME
                } else {
                    theme::TIMESTAMP
                };
                let separator = if adapter_part.is_empty() { "" } else { " " };
                line2_spans.push(Span::styled(
                    format!("{}{}", separator, hint),
                    Style::default().fg(hint_color),
                ));
            }
            lines.push(Line::from(line2_spans));
        }
    }
}

/// Truncate a string to fit within `max` display-width columns, appending "…" if truncated.
/// Uses `unicode_width` for correct CJK / emoji / fullwidth handling.
fn truncate_str(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let target = max - 1; // reserve 1 col for "…"
    let mut width = 0;
    let mut end = 0;
    for (i, ch) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cw > target {
            break;
        }
        width += cw;
        end = i + ch.len_utf8();
    }
    format!("{}…", &s[..end])
}

/// Format elapsed seconds as a compact duration string.
#[allow(dead_code)]
pub(crate) fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        if s > 0 { format!("{}m{}s", m, s) } else { format!("{}m", m) }
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m > 0 { format!("{}h{}m", h, m) } else { format!("{}h", h) }
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
                transport: "stdio".to_string(),
                capabilities: vec![],
                usage: None,
                tool_call_name: None,
                tool_call_started: None,
                waiting_for: None,
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
                unread: 0,
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
            unread: 0,
        };
        assert_eq!(ch_named.display_name(), "main");

        let ch_unnamed = ChannelDisplay {
            id: "ch-abc-123".into(),
            name: None,
            node_count: 0,
            members: Vec::new(),
            unread: 0,
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
            unread: 0,
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
            unread: 0,
        };
        let agents = vec![
            AgentDisplay { name: "a".into(), status: "idle".into(), activity: None, adapter: None, node_id: "n1".into(), transport: "stdio".into(), capabilities: vec![], usage: None, tool_call_name: None, tool_call_started: None, waiting_for: None },
            AgentDisplay { name: "b".into(), status: "busy".into(), activity: None, adapter: None, node_id: "n2".into(), transport: "stdio".into(), capabilities: vec![], usage: None, tool_call_name: None, tool_call_started: None, waiting_for: None },
            AgentDisplay { name: "c".into(), status: "busy".into(), activity: None, adapter: None, node_id: "n3".into(), transport: "stdio".into(), capabilities: vec![], usage: None, tool_call_name: None, tool_call_started: None, waiting_for: None },
        ];
        let busy = ch.members.iter().filter(|m| {
            agents.iter().any(|a| a.node_id == m.node_id && a.status == "busy")
        }).count();
        assert_eq!(busy, 2);
    }

    // --- truncate_str tests ---

    #[test]
    fn truncate_str_no_op_when_fits() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_adds_ellipsis() {
        assert_eq!(truncate_str("hello world", 6), "hello…");
        assert_eq!(truncate_str("abcdef", 4), "abc…");
    }

    #[test]
    fn truncate_str_min_width() {
        assert_eq!(truncate_str("hello", 1), "…");
        assert_eq!(truncate_str("hello", 0), "…"); // degenerate
    }

    #[test]
    fn truncate_str_cjk_respects_display_width() {
        // Each CJK char = 2 cols. "你好世界" = 8 cols
        let s = "你好世界";
        let result = truncate_str(s, 5); // room for 2 CJK chars (4 cols) + "…" (1 col) = 5
        assert!(result.width() <= 5, "truncated CJK too wide: {} ({})", result, result.width());
        assert!(result.contains('…'));
    }

    #[test]
    fn truncate_str_empty() {
        assert_eq!(truncate_str("", 5), "");
    }

    // --- agent_status_line tests ---

    #[test]
    fn status_line_idle_shows_adapter_only() {
        let agent = AgentDisplay {
            name: "worker".into(),
            status: "idle".into(),
            activity: None,
            adapter: Some("claude/opus".into()),
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: None,
            tool_call_started: None,
            waiting_for: None,
        };
        let line = StatusBar::agent_status_line(&agent, 20);
        assert_eq!(line, "claude/opus");
    }

    #[test]
    fn status_line_with_tool_call() {
        let agent = AgentDisplay {
            name: "worker".into(),
            status: "busy".into(),
            activity: None,
            adapter: Some("claude/opus".into()),
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: Some("Read".into()),
            tool_call_started: None,
            waiting_for: None,
        };
        let line = StatusBar::agent_status_line(&agent, 20);
        assert!(line.contains("claude/opus"), "should have adapter: {}", line);
        assert!(line.contains("[Read]"), "should have tool hint: {}", line);
    }

    #[test]
    fn status_line_with_activity() {
        let agent = AgentDisplay {
            name: "worker".into(),
            status: "busy".into(),
            activity: Some("thinking".into()),
            adapter: Some("claude/opus".into()),
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: None,
            tool_call_started: None,
            waiting_for: None,
        };
        let line = StatusBar::agent_status_line(&agent, 20);
        assert!(line.contains("[thinking]"), "should have activity: {}", line);
    }

    #[test]
    fn status_line_tool_call_overrides_activity() {
        let agent = AgentDisplay {
            name: "worker".into(),
            status: "busy".into(),
            activity: Some("typing".into()),
            adapter: Some("claude/opus".into()),
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: Some("Write".into()),
            tool_call_started: None,
            waiting_for: None,
        };
        let line = StatusBar::agent_status_line(&agent, 25);
        assert!(line.contains("[Write]"), "tool_call wins: {}", line);
        assert!(!line.contains("typing"), "activity hidden: {}", line);
    }

    #[test]
    fn status_line_waiting_for_ignored() {
        let agent = AgentDisplay {
            name: "reviewer".into(),
            status: "busy".into(),
            activity: None,
            adapter: Some("claude/opus".into()),
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: None,
            tool_call_started: None,
            waiting_for: Some("main".into()),
        };
        let line = StatusBar::agent_status_line(&agent, 25);
        assert!(!line.contains("→main"), "waiting_for should not appear in sidebar: {}", line);
        assert_eq!(line, "claude/opus", "should show adapter only: {}", line);
    }

    #[test]
    fn status_line_no_adapter() {
        let agent = AgentDisplay {
            name: "mc".into(),
            status: "busy".into(),
            activity: Some("recording".into()),
            adapter: None,
            node_id: "n1".into(),
            transport: "ws".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: None,
            tool_call_started: None,
            waiting_for: None,
        };
        let line = StatusBar::agent_status_line(&agent, 20);
        assert!(line.contains("[recording]"), "should have activity: {}", line);
    }

    #[test]
    fn status_line_truncates_within_width() {
        let agent = AgentDisplay {
            name: "worker".into(),
            status: "busy".into(),
            activity: None,
            adapter: Some("very-long-adapter/very-long-model".into()),
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: Some("VeryLongToolCallName".into()),
            tool_call_started: None,
            waiting_for: None,
        };
        let line = StatusBar::agent_status_line(&agent, 15);
        assert!(line.width() <= 15, "line too wide: {} ({})", line, line.width());
    }

    #[test]
    fn status_line_cjk_width() {
        let agent = AgentDisplay {
            name: "测试".into(),
            status: "busy".into(),
            activity: None,
            adapter: Some("模型".into()),  // 4 display cols
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: Some("Read".into()),
            tool_call_started: None,
            waiting_for: None,
        };
        let line = StatusBar::agent_status_line(&agent, 12);
        assert!(line.width() <= 12, "CJK status too wide: {} ({})", line, line.width());
    }

    // --- Tool call display tests ---

    #[test]
    fn format_elapsed_seconds() {
        assert_eq!(format_elapsed(5), "5s");
        assert_eq!(format_elapsed(59), "59s");
    }

    #[test]
    fn format_elapsed_minutes() {
        assert_eq!(format_elapsed(60), "1m");
        assert_eq!(format_elapsed(90), "1m30s");
        assert_eq!(format_elapsed(3599), "59m59s");
    }

    #[test]
    fn format_elapsed_hours() {
        assert_eq!(format_elapsed(3600), "1h");
        assert_eq!(format_elapsed(5400), "1h30m");
    }

    #[test]
    fn render_with_tool_call_no_panic() {
        let bar = StatusBar::new();
        let agents = vec![AgentDisplay {
            name: "main".into(),
            status: "busy".into(),
            activity: None,
            adapter: None,
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: Some("Write".into()),
            tool_call_started: Some(Instant::now()),
            waiting_for: None,
        }];
        let area = Rect::new(0, 0, 24, 20);
        let mut buf = Buffer::empty(area);
        bar.render(&[], None, &agents, None, None, false, area, &mut buf);
    }

    #[test]
    fn render_with_waiting_for_no_panic() {
        let bar = StatusBar::new();
        let agents = vec![AgentDisplay {
            name: "reviewer".into(),
            status: "busy".into(),
            activity: None,
            adapter: None,
            node_id: "n1".into(),
            transport: "stdio".into(),
            capabilities: vec![],
            usage: None,
            tool_call_name: None,
            tool_call_started: None,
            waiting_for: Some("main".into()),
        }];
        let area = Rect::new(0, 0, 24, 20);
        let mut buf = Buffer::empty(area);
        bar.render(&[], None, &agents, None, None, false, area, &mut buf);
    }
}

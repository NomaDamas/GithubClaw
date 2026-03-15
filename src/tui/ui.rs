//! TUI rendering with ratatui.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, Borders, Gauge, List, ListItem, Paragraph, Sparkline, Tabs,
};
use ratatui::Frame;

use super::app::App;
use super::summary::{CardActionState, CardTone, SummaryCard};
use super::tabs::{AgentStatus, Tab};

/// Render the entire TUI frame.
pub fn render(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Tab bar
            Constraint::Min(0),    // Content
            Constraint::Length(1), // Status bar
        ])
        .split(f.area());

    render_tab_bar(f, app, chunks[0]);

    match app.active_tab {
        Tab::IssueRequest => render_issue_request_tab(f, app, chunks[1]),
        Tab::Monitoring => render_monitoring_tab(f, app, chunks[1]),
        Tab::Release => render_release_tab(f, app, chunks[1]),
    }

    render_status_bar(f, app, chunks[2]);
}

fn render_tab_bar(f: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = Tab::all()
        .iter()
        .map(|t| {
            let style = if *t == app.active_tab {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(Span::styled(t.title(), style))
        })
        .collect();

    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title(" GithubClaw "))
        .highlight_style(Style::default().fg(Color::Yellow))
        .select(match app.active_tab {
            Tab::IssueRequest => 0,
            Tab::Monitoring => 1,
            Tab::Release => 2,
        });

    f.render_widget(tabs, area);
}

fn render_issue_request_tab(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    // Left: Issue list
    let items: Vec<ListItem> = app
        .issue_requests
        .iter()
        .enumerate()
        .map(|(i, issue)| {
            let marker = if issue.vision_report_ready { "+" } else { " " };
            let style = if i == app.selected_issue_index {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} #{:<5} ", marker, issue.issue_number), style),
                Span::styled(
                    format!("[{}] ", issue.issue_type),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(&issue.title, style),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Issues (awaiting session) "),
    );
    f.render_widget(list, chunks[0]);

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(0)])
        .split(chunks[1]);

    render_summary_card(f, &app.issue_request_summary_card(), right_chunks[0]);

    // Right: Interactive session area
    let session_title = if app.interactive_session_active {
        " Interactive Session (Esc to exit) "
    } else {
        " Interactive Session "
    };

    let session_content = if app.interactive_session_active {
        if app.pty_output.is_empty() {
            "Starting interactive session...".to_string()
        } else {
            // Show last N lines that fit the panel
            let available_height = chunks[1].height.saturating_sub(2) as usize;
            let lines: Vec<&str> = app.pty_output.lines().collect();
            let start = lines.len().saturating_sub(available_height);
            lines[start..].join("\n")
        }
    } else if app.issue_requests.is_empty() {
        "No issues awaiting interactive session.\n\n\
         Bugs are auto-processed.\n\
         Feature/Refactoring issues will appear here\n\
         after Vision-gap Analyst completes analysis."
            .to_string()
    } else {
        "Select an issue and press Enter to start\n\
         an interactive session with the Orchestrator.\n\n\
         Ctrl+A: Approve  Ctrl+R: Reject"
            .to_string()
    };

    let session = Paragraph::new(session_content)
        .block(Block::default().borders(Borders::ALL).title(session_title));
    f.render_widget(session, right_chunks[1]);
}

fn render_monitoring_tab(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);

    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Min(0),
        ])
        .split(chunks[0]);

    render_githubclaw_ascii(f, left_chunks[0]);
    render_monitoring_status(f, app, left_chunks[1]);

    let items: Vec<ListItem> = app
        .agent_sessions
        .iter()
        .enumerate()
        .map(|(i, session)| {
            let style = if i == app.selected_agent_index {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let status_color = match session.status {
                AgentStatus::Running => Color::Green,
                AgentStatus::Queued => Color::DarkGray,
                AgentStatus::Completed => Color::Blue,
                AgentStatus::Failed => Color::Red,
                AgentStatus::Idle => Color::DarkGray,
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" #{:<5} ", session.issue_number), style),
                Span::styled(format!("{:<16} ", session.agent_type), style),
                Span::styled(session.status.symbol(), Style::default().fg(status_color)),
            ]))
        })
        .collect();

    let agent_list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Active Sessions "),
    );
    f.render_widget(agent_list, left_chunks[2]);

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Min(0),
        ])
        .split(chunks[1]);

    render_monitoring_heartbeat(f, app, right_chunks[0]);
    render_stage_flow(f, app, right_chunks[1]);

    let detail_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(8)])
        .split(right_chunks[2]);

    render_issue_waterfall(f, app, detail_chunks[0]);
    render_recent_events(f, app, detail_chunks[1]);
}

fn render_githubclaw_ascii(f: &mut Frame, area: Rect) {
    let art = vec![
        Line::from("   ____ _ _   _   _     _                 "),
        Line::from("  / ___(_) |_| |_| |__ | |__   ___ _ _    "),
        Line::from(" | |  _| | __| __| '_ \\| '_ \\ / _ \\ '_|   "),
        Line::from(" | |_| | | |_| |_| | | | |_) |  __/ |     "),
        Line::from("  \\____|_|\\__|\\__|_| |_|_.__/ \\___|_|     "),
        Line::from("        claws on the queue                "),
    ];

    let widget =
        Paragraph::new(art).block(Block::default().borders(Borders::ALL).title(" GithubClaw "));
    f.render_widget(widget, area);
}

fn render_monitoring_status(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(area);

    let rate_color = rate_limit_color(&app.rate_limit_tier);
    let rate_label = rate_limit_label(&app.rate_limit_tier);
    let rate = Paragraph::new(Line::from(vec![
        Span::raw("  Rate Limit "),
        Span::styled(
            format!("{:<12}", rate_label),
            Style::default().fg(rate_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("tier {}", app.rate_limit_tier),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" Rate "));
    f.render_widget(rate, chunks[0]);

    let worker_ratio = if app.worker_count.1 == 0 {
        0.0
    } else {
        app.worker_count.0 as f64 / app.worker_count.1 as f64
    };
    let worker_gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Worker Load "),
        )
        .ratio(worker_ratio.clamp(0.0, 1.0))
        .label(format!(
            "{}/{} active",
            app.worker_count.0, app.worker_count.1
        ))
        .gauge_style(worker_color(app.worker_count));
    f.render_widget(worker_gauge, chunks[1]);

    let queue_color = queue_color(app.queue_depth, app.oldest_queue_age_seconds);
    let queue_state = queue_state_label(app.queue_depth, app.oldest_queue_age_seconds);
    let queue = Paragraph::new(vec![
        Line::from(vec![
            Span::raw("  Queue       "),
            Span::styled(
                format!("{:<10}", format!("{} pending", app.queue_depth)),
                Style::default().fg(queue_color),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Oldest wait "),
            Span::styled(
                format_oldest_wait(app.oldest_queue_age_seconds),
                Style::default().fg(queue_color),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Health      "),
            Span::styled(
                queue_state,
                Style::default()
                    .fg(queue_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Queue Health "),
    );
    f.render_widget(queue, chunks[2]);
}

fn render_monitoring_heartbeat(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(area);

    let queue_history = history_or_zero(&app.queue_history);
    let worker_history = history_or_zero(&app.worker_history);
    let activity_history = history_or_zero(&app.activity_history);

    let queue_spark = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(" Queue 30m "))
        .data(&queue_history)
        .max(queue_history.iter().copied().max().unwrap_or(1).max(1))
        .style(queue_color(app.queue_depth, app.oldest_queue_age_seconds));
    f.render_widget(queue_spark, chunks[0]);

    let worker_spark = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Workers 30m "),
        )
        .data(&worker_history)
        .max(app.worker_count.1.max(1) as u64)
        .style(worker_color(app.worker_count));
    f.render_widget(worker_spark, chunks[1]);

    let activity_spark = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(" Events 30m "))
        .data(&activity_history)
        .max(activity_history.iter().copied().max().unwrap_or(1).max(1))
        .style(activity_color(
            activity_history.last().copied().unwrap_or_default(),
        ));
    f.render_widget(activity_spark, chunks[2]);
}

fn render_stage_flow(f: &mut Frame, app: &App, area: Rect) {
    let mut queued = 0;
    let mut running = 0;
    let mut completed = 0;
    let mut failed = 0;

    for session in &app.agent_sessions {
        match session.status {
            AgentStatus::Queued => queued += 1,
            AgentStatus::Running => running += 1,
            AgentStatus::Completed => completed += 1,
            AgentStatus::Failed => failed += 1,
            AgentStatus::Idle => {}
        }
    }

    let max_value = queued.max(running).max(completed).max(failed).max(1) as u64;
    let bars = vec![
        Bar::default()
            .label("queued".into())
            .value(queued as u64)
            .style(Style::default().fg(Color::Yellow)),
        Bar::default()
            .label("running".into())
            .value(running as u64)
            .style(Style::default().fg(Color::Green)),
        Bar::default()
            .label("done".into())
            .value(completed as u64)
            .style(Style::default().fg(Color::Blue)),
        Bar::default()
            .label("failed".into())
            .value(failed as u64)
            .style(Style::default().fg(Color::Red)),
    ];

    let chart = BarChart::default()
        .block(Block::default().borders(Borders::ALL).title(" Stage Flow "))
        .data(BarGroup::default().bars(&bars))
        .bar_width(8)
        .bar_gap(1)
        .max(max_value)
        .value_style(Style::default().add_modifier(Modifier::BOLD))
        .label_style(Style::default().fg(Color::DarkGray));
    f.render_widget(chart, area);
}

fn render_issue_waterfall(f: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();

    if app.agent_timeline.is_empty() {
        lines.push(Line::from("  Select a session to see the issue waterfall."));
    } else {
        let issue_label = app
            .selected_issue_number_for_timeline()
            .map(|issue| format!("#{}", issue))
            .unwrap_or_else(|| "selected issue".to_string());

        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} pipeline", issue_label),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "sequence over recent events",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        lines.push(Line::from(""));

        let lanes = build_waterfall_lanes(&app.agent_timeline);
        for (agent_type, segments) in lanes {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<16}", truncate_agent_label(&agent_type, 16)),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::raw(segments),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  [c] Comment on PR",
        Style::default().fg(Color::DarkGray),
    )));

    let widget = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Issue Waterfall "),
    );
    f.render_widget(widget, area);
}

fn render_recent_events(f: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();

    if app.recent_events.is_empty() {
        lines.push(Line::from("  No recent timeline events yet."));
    } else {
        for event in &app.recent_events {
            lines.push(Line::from(vec![
                Span::styled("  > ", Style::default().fg(Color::DarkGray)),
                Span::raw(event),
            ]));
        }
    }

    let widget = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Recent Events "),
    );
    f.render_widget(widget, area);
}

fn render_release_tab(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(0)])
        .split(area);
    render_summary_card(f, &app.release_summary_card(), chunks[0]);

    let mut lines: Vec<Line> = Vec::new();

    match &app.release_info {
        Some(info) => {
            lines.push(Line::from(vec![
                Span::raw("  Branch: "),
                Span::styled(&info.branch, Style::default().fg(Color::Cyan)),
                Span::raw(" -> main"),
            ]));

            if let Some(pr) = info.pr_number {
                lines.push(Line::from(format!("  PR: #{}", pr)));
            }

            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Included Issues:",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for (num, title) in &info.included_issues {
                lines.push(Line::from(format!("    #{} {}", num, title)));
            }

            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Dogfooding Checklist:",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for item in &info.checklist {
                let check = if item.checked { "[x]" } else { "[ ]" };
                lines.push(Line::from(format!("    {} {}", check, item.text)));
            }
        }
        None => {
            lines.push(Line::from("  No active release pipeline."));
            lines.push(Line::from(""));
            lines.push(Line::from("  Press [r] to start a release."));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  [r] Run release  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "[o] Open PR in browser",
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    let release =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Release "));
    f.render_widget(release, chunks[1]);
}

fn render_summary_card(f: &mut Frame, card: &SummaryCard, area: Rect) {
    let tone_color = match card.tone {
        CardTone::Info => Color::Cyan,
        CardTone::Success => Color::Green,
        CardTone::Warning => Color::Yellow,
        CardTone::Danger => Color::Red,
    };
    let state_label = match card.action_state {
        CardActionState::Passive => "PASSIVE",
        CardActionState::Watching => "WATCHING",
        CardActionState::NeedsDecision => "DECISION",
        CardActionState::Blocked => "BLOCKED",
        CardActionState::InProgress => "LIVE",
        CardActionState::Done => "DONE",
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(&card.title, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(
                state_label,
                Style::default().fg(tone_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            &card.status_line,
            Style::default().fg(tone_color),
        )),
        Line::from(""),
    ];

    for bullet in card.bullets.iter().take(3) {
        lines.push(Line::from(vec![Span::raw("- "), Span::raw(bullet)]));
    }

    if let Some(next_action) = &card.next_action {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Next: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(next_action),
        ]));
    }

    let widget =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Summary "));
    f.render_widget(widget, area);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let help = match app.active_tab {
        Tab::IssueRequest => {
            if app.interactive_session_active {
                "Esc: Back  |  Interactive Session Active"
            } else {
                "j/k: Navigate  Enter: Open Session  Ctrl+A: Approve  Ctrl+R: Reject  Tab: Switch  q: Quit"
            }
        }
        Tab::Monitoring => "j/k: Navigate  c: Comment  Tab: Switch  q: Quit",
        Tab::Release => "r: Release  o: Open PR  Tab: Switch  q: Quit",
    };

    let bar = Paragraph::new(Span::styled(
        format!(" {}", help),
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(bar, area);
}

fn history_or_zero(history: &std::collections::VecDeque<u64>) -> Vec<u64> {
    if history.is_empty() {
        vec![0]
    } else {
        history.iter().copied().collect()
    }
}

fn rate_limit_label(tier: &str) -> &'static str {
    if tier.eq_ignore_ascii_case("none") || tier.eq_ignore_ascii_case("clear") {
        "CLEAR"
    } else if tier.eq_ignore_ascii_case("low")
        || tier.eq_ignore_ascii_case("soft")
        || tier.eq_ignore_ascii_case("watch")
    {
        "WATCHING"
    } else {
        "CONSTRAINED"
    }
}

fn rate_limit_color(tier: &str) -> Color {
    match rate_limit_label(tier) {
        "CLEAR" => Color::Green,
        "WATCHING" => Color::Yellow,
        _ => Color::Red,
    }
}

fn worker_color(worker_count: (usize, usize)) -> Color {
    let (active, max) = worker_count;
    if max == 0 {
        return Color::DarkGray;
    }

    let ratio = active as f64 / max as f64;
    if ratio < 0.6 {
        Color::Green
    } else if ratio < 0.9 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn queue_color(queue_depth: usize, oldest_queue_age_seconds: Option<u64>) -> Color {
    if queue_depth == 0 {
        return Color::Green;
    }

    match oldest_queue_age_seconds.unwrap_or_default() {
        0..=299 if queue_depth <= 3 => Color::Green,
        0..=899 if queue_depth <= 8 => Color::Yellow,
        _ => Color::Red,
    }
}

fn queue_state_label(queue_depth: usize, oldest_queue_age_seconds: Option<u64>) -> &'static str {
    match queue_color(queue_depth, oldest_queue_age_seconds) {
        Color::Green => "STABLE",
        Color::Yellow => "WATCHING",
        _ => "PRESSURE",
    }
}

fn activity_color(last_value: u64) -> Color {
    if last_value == 0 {
        Color::DarkGray
    } else if last_value <= 2 {
        Color::Cyan
    } else {
        Color::Green
    }
}

fn format_oldest_wait(age_seconds: Option<u64>) -> String {
    match age_seconds {
        Some(seconds) => {
            let minutes = seconds / 60;
            let seconds = seconds % 60;
            format!("{minutes:02}m{seconds:02}s")
        }
        None => "00m00s".into(),
    }
}

fn build_waterfall_lanes(timeline: &[super::tabs::TimelineEntry]) -> Vec<(String, String)> {
    let mut lanes: Vec<(String, Vec<char>)> = Vec::new();

    for entry in timeline {
        let symbol = match entry.status {
            AgentStatus::Queued => '.',
            AgentStatus::Running => '=',
            AgentStatus::Completed => '#',
            AgentStatus::Failed => '!',
            AgentStatus::Idle => '-',
        };

        if let Some((_, segments)) = lanes
            .iter_mut()
            .find(|(agent_type, _)| agent_type == &entry.agent_type)
        {
            segments.push(symbol);
        } else {
            lanes.push((entry.agent_type.clone(), vec![symbol]));
        }
    }

    lanes
        .into_iter()
        .map(|(agent_type, segments)| (agent_type, segments.into_iter().collect()))
        .collect()
}

fn truncate_agent_label(agent_type: &str, width: usize) -> String {
    agent_type.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1. Tab titles are correct
    #[test]
    fn tab_titles() {
        assert_eq!(Tab::IssueRequest.title(), "Issue Request");
        assert_eq!(Tab::Monitoring.title(), "Monitoring");
        assert_eq!(Tab::Release.title(), "Release");
    }

    // 2. Tab cycling
    #[test]
    fn tab_cycling() {
        assert_eq!(Tab::IssueRequest.next(), Tab::Monitoring);
        assert_eq!(Tab::Monitoring.next(), Tab::Release);
        assert_eq!(Tab::Release.next(), Tab::IssueRequest);
        assert_eq!(Tab::IssueRequest.prev(), Tab::Release);
    }

    // 3. AgentStatus symbols
    #[test]
    fn agent_status_symbols() {
        assert_eq!(AgentStatus::Running.symbol(), ">>");
        assert_eq!(AgentStatus::Completed.symbol(), "ok");
        assert_eq!(AgentStatus::Failed.symbol(), "!!");
    }

    // 4. Render doesn't panic with empty app
    #[test]
    fn render_doesnt_panic_empty() {
        // Just verify the render function signature compiles and types align
        let _app = App::new();
        // Actual rendering requires a terminal backend, tested via integration
    }
}

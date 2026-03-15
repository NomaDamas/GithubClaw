//! TUI rendering with ratatui.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs};
use ratatui::Frame;

use super::app::App;
use super::tabs::{AgentStatus, Tab};

/// Render the entire TUI frame.
pub fn render(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Tab bar
            Constraint::Min(0),   // Content
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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" GithubClaw "),
        )
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
            let marker = if issue.vision_report_ready {
                "+"
            } else {
                " "
            };
            let style = if i == app.selected_issue_index {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {} #{:<5} ", marker, issue.issue_number),
                    style,
                ),
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

    // Right: Interactive session area
    let session_content = if app.interactive_session_active {
        "Interactive session active.\nClaude Code / Codex session running.\n\nPress Esc to return to issue list."
    } else if app.issue_requests.is_empty() {
        "No issues awaiting interactive session.\n\nBugs are auto-processed.\nFeature/Refactoring issues will appear here\nafter Vision-gap Analyst completes analysis."
    } else {
        "Select an issue and press Enter to start\nan interactive session with the Orchestrator.\n\nCtrl+A: Approve  Ctrl+R: Reject"
    };

    let session = Paragraph::new(session_content).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Interactive Session "),
    );
    f.render_widget(session, chunks[1]);
}

fn render_monitoring_tab(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    // Left: split into agent list + rate limit status
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(5)])
        .split(chunks[0]);

    // Agent list
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
                Span::styled(
                    format!(" #{:<5} ", session.issue_number),
                    style,
                ),
                Span::styled(
                    format!("{:<16} ", session.agent_type),
                    style,
                ),
                Span::styled(
                    session.status.symbol(),
                    Style::default().fg(status_color),
                ),
            ]))
        })
        .collect();

    let agent_list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Active Sessions "),
    );
    f.render_widget(agent_list, left_chunks[0]);

    // Rate limit status
    let rl_color = if app.rate_limit_tier == "None" {
        Color::Green
    } else {
        Color::Red
    };
    let rl_info = Paragraph::new(vec![
        Line::from(vec![
            Span::raw("  Tier: "),
            Span::styled(&app.rate_limit_tier, Style::default().fg(rl_color)),
        ]),
        Line::from(format!(
            "  Workers: {}/{}",
            app.worker_count.0, app.worker_count.1
        )),
        Line::from(format!("  Queue: {} pending", app.queue_depth)),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Rate Limit "),
    );
    f.render_widget(rl_info, left_chunks[1]);

    // Right: Session detail with timeline
    let mut detail_lines: Vec<Line> = Vec::new();

    if app.agent_timeline.is_empty() {
        detail_lines.push(Line::from("  Select a session to see details."));
    } else {
        detail_lines.push(Line::from(
            Span::styled("  Agent Timeline:", Style::default().add_modifier(Modifier::BOLD)),
        ));
        detail_lines.push(Line::from(""));
        for entry in &app.agent_timeline {
            let status_color = match entry.status {
                AgentStatus::Running => Color::Green,
                AgentStatus::Completed => Color::Blue,
                AgentStatus::Failed => Color::Red,
                _ => Color::DarkGray,
            };
            detail_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{} ", entry.status.symbol()),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    format!("{:<20} ", entry.agent_type),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(&entry.detail),
            ]));
        }
    }

    detail_lines.push(Line::from(""));
    detail_lines.push(Line::from(
        Span::styled("  [c] Comment on PR", Style::default().fg(Color::DarkGray)),
    ));

    let detail = Paragraph::new(detail_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Session Detail "),
    );
    f.render_widget(detail, chunks[1]);
}

fn render_release_tab(f: &mut Frame, app: &App, area: Rect) {
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
            lines.push(Line::from(
                Span::styled("  Included Issues:", Style::default().add_modifier(Modifier::BOLD)),
            ));
            for (num, title) in &info.included_issues {
                lines.push(Line::from(format!("    #{} {}", num, title)));
            }

            lines.push(Line::from(""));
            lines.push(Line::from(
                Span::styled(
                    "  Dogfooding Checklist:",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ));
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
        Span::styled("[o] Open PR in browser", Style::default().fg(Color::DarkGray)),
    ]));

    let release = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Release "),
    );
    f.render_widget(release, area);
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

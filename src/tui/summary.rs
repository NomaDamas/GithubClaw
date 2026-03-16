//! Deterministic summary cards for the TUI.

use super::app::App;
use super::tabs::AgentStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardTone {
    Info,
    Success,
    Warning,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardActionState {
    Passive,
    Watching,
    NeedsDecision,
    Blocked,
    InProgress,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryCard {
    pub title: String,
    pub status_line: String,
    pub bullets: Vec<String>,
    pub next_action: Option<String>,
    pub tone: CardTone,
    pub action_state: CardActionState,
}

impl App {
    pub fn issue_request_summary_card(&self) -> SummaryCard {
        if self.interactive_session_active {
            let issue_label = self
                .selected_issue()
                .map(|issue| format!("#{}", issue.issue_number))
                .unwrap_or_else(|| "selected issue".to_string());
            return SummaryCard {
                title: format!("Interactive session live for {}", issue_label),
                status_line: "Operator is in the loop".into(),
                bullets: vec![
                    "Interactive PTY session is active in the right pane.".into(),
                    "Approval and rejection stay available after the session exits.".into(),
                ],
                next_action: Some(
                    "Use Esc to return to the inbox when the direction is clear.".into(),
                ),
                tone: CardTone::Info,
                action_state: CardActionState::InProgress,
            };
        }

        match self.selected_issue() {
            Some(issue) => {
                let mut bullets = vec![format!(
                    "{} request is waiting for operator review.",
                    issue.issue_type
                )];
                if issue.vision_report_ready {
                    bullets.push("Vision-gap analysis is ready for a quick read.".into());
                } else {
                    bullets.push("Background analysis is still warming up.".into());
                }
                bullets
                    .push("Open the session to turn a long thread into a clear decision.".into());

                SummaryCard {
                    title: format!("#{} {}", issue.issue_number, issue.title),
                    status_line: format!("{} awaiting interactive session", issue.issue_type),
                    bullets,
                    next_action: Some(
                        "Press Enter to open the session, then approve or reject.".into(),
                    ),
                    tone: if issue.vision_report_ready {
                        CardTone::Info
                    } else {
                        CardTone::Warning
                    },
                    action_state: CardActionState::NeedsDecision,
                }
            }
            None => SummaryCard {
                title: "Issue request inbox is clear".into(),
                status_line: "No operator decisions waiting".into(),
                bullets: vec![
                    "Bug issues continue through the automated path.".into(),
                    "Feature and refactor requests will appear here once analysis is ready.".into(),
                ],
                next_action: None,
                tone: CardTone::Success,
                action_state: CardActionState::Done,
            },
        }
    }

    pub fn monitoring_summary_card(&self) -> SummaryCard {
        let Some(session) = self.selected_agent_session() else {
            return SummaryCard {
                title: "Monitoring is idle".into(),
                status_line: "No active session selected".into(),
                bullets: vec![
                    format!(
                        "{} items are queued across tracked repositories.",
                        self.queue_depth
                    ),
                    format!(
                        "{} of {} workers are currently busy.",
                        self.worker_count.0, self.worker_count.1
                    ),
                ],
                next_action: None,
                tone: CardTone::Info,
                action_state: CardActionState::Passive,
            };
        };

        let latest = self.agent_timeline.last();
        let mut bullets = vec![format!(
            "{} worker is attached to issue #{}.",
            session.agent_type, session.issue_number
        )];

        if let Some(entry) = latest {
            bullets.push(format!("Latest update: {}", entry.detail));
        } else {
            bullets.push("No timeline events have been recorded yet.".into());
        }

        bullets.push(format!(
            "Queue depth is {} and worker usage is {}/{}.",
            self.queue_depth, self.worker_count.0, self.worker_count.1
        ));

        let (tone, action_state, next_action, status_line) = match latest
            .map(|entry| &entry.status)
            .unwrap_or(&session.status)
        {
            AgentStatus::Failed => (
                CardTone::Danger,
                CardActionState::Blocked,
                Some("Inspect the failure detail below and choose the retry path.".into()),
                "Session is blocked and likely needs operator help".into(),
            ),
            AgentStatus::Running => (
                CardTone::Info,
                CardActionState::InProgress,
                Some("Watch the timeline for the next checkpoint or leave it running.".into()),
                "Work is actively progressing".into(),
            ),
            AgentStatus::Queued => (
                CardTone::Warning,
                CardActionState::Watching,
                Some("Keep an eye on worker capacity and queue depth.".into()),
                "Waiting for execution slot".into(),
            ),
            AgentStatus::Completed => (
                CardTone::Success,
                CardActionState::Done,
                Some("Review the result below and decide whether a follow-up is needed.".into()),
                "Latest run completed".into(),
            ),
            AgentStatus::Idle => (
                CardTone::Info,
                CardActionState::Passive,
                None,
                "Session is idle".into(),
            ),
        };

        SummaryCard {
            title: format!("{} on issue #{}", session.agent_type, session.issue_number),
            status_line,
            bullets,
            next_action,
            tone,
            action_state,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::tabs::{AgentSessionItem, IssueRequestItem, TimelineEntry};

    #[test]
    fn issue_request_card_shows_needs_decision() {
        let mut app = App::new();
        app.issue_requests.push(IssueRequestItem {
            issue_number: 142,
            title: "Night mode".into(),
            issue_type: "Feature".into(),
            vision_report_ready: true,
        });

        let card = app.issue_request_summary_card();
        assert_eq!(card.action_state, CardActionState::NeedsDecision);
        assert_eq!(card.tone, CardTone::Info);
        assert!(card.status_line.contains("awaiting interactive session"));
    }

    #[test]
    fn monitoring_card_shows_blocked_state_for_failed_timeline() {
        let mut app = App::new();
        app.agent_sessions.push(AgentSessionItem {
            issue_number: 155,
            agent_type: "verifier".into(),
            status: AgentStatus::Running,
            started_at: "2026-03-15T18:00:00Z".into(),
        });
        app.agent_timeline.push(TimelineEntry {
            agent_type: "verifier".into(),
            status: AgentStatus::Failed,
            detail: "Verification loop exceeded retry budget".into(),
        });

        let card = app.monitoring_summary_card();
        assert_eq!(card.action_state, CardActionState::Blocked);
        assert_eq!(card.tone, CardTone::Danger);
        assert!(card.next_action.unwrap().contains("retry path"));
    }
}

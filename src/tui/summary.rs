//! Deterministic summary cards for the TUI.

use super::app::App;
use super::tabs::{AgentStatus, ReleaseInfo};

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
                    "Claude Code PTY session is active in the right pane.".into(),
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

    pub fn release_summary_card(&self) -> SummaryCard {
        match &self.release_info {
            Some(info) => release_summary(info),
            None => SummaryCard {
                title: "No active release pipeline".into(),
                status_line: "Release tab is ready when you are".into(),
                bullets: vec![
                    "A release run will prepare the branch and show the checklist here.".into(),
                    "Use this tab when you want a calm end-of-day ship room.".into(),
                ],
                next_action: Some("Press r when you want to kick off a release.".into()),
                tone: CardTone::Info,
                action_state: CardActionState::Passive,
            },
        }
    }

    fn selected_issue(&self) -> Option<&super::tabs::IssueRequestItem> {
        self.issue_requests.get(self.selected_issue_index)
    }

    fn selected_agent_session(&self) -> Option<&super::tabs::AgentSessionItem> {
        self.agent_sessions.get(self.selected_agent_index)
    }
}

fn release_summary(info: &ReleaseInfo) -> SummaryCard {
    let remaining = info.checklist.iter().filter(|item| !item.checked).count();
    let included = info.included_issues.len();
    let mut bullets = vec![format!(
        "{} issues are grouped into this release.",
        included
    )];
    if let Some(pr) = info.pr_number {
        bullets.push(format!("Release PR #{} is already open.", pr));
    } else {
        bullets.push("Release PR has not been opened yet.".into());
    }
    bullets.push(format!(
        "{} checklist item{} still need attention.",
        remaining,
        if remaining == 1 { "" } else { "s" }
    ));

    if remaining == 0 {
        SummaryCard {
            title: format!("{} is ready for the final release pass", info.branch),
            status_line: "Checklist complete".into(),
            bullets,
            next_action: Some("Open the PR or move into final human review.".into()),
            tone: CardTone::Success,
            action_state: CardActionState::Done,
        }
    } else {
        SummaryCard {
            title: format!("{} is in release prep", info.branch),
            status_line: "Checklist still in progress".into(),
            bullets,
            next_action: Some("Finish the remaining checklist items before shipping.".into()),
            tone: CardTone::Warning,
            action_state: CardActionState::Watching,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::tabs::{AgentSessionItem, ChecklistItem, IssueRequestItem, TimelineEntry};

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

    #[test]
    fn release_card_highlights_remaining_checklist_items() {
        let mut app = App::new();
        app.release_info = Some(ReleaseInfo {
            branch: "release/2026-03-15".into(),
            pr_number: Some(88),
            pr_url: None,
            included_issues: vec![(1, "Ship summary card".into())],
            checklist: vec![
                ChecklistItem {
                    text: "Dogfood locally".into(),
                    checked: true,
                },
                ChecklistItem {
                    text: "Merge release PR".into(),
                    checked: false,
                },
            ],
        });

        let card = app.release_summary_card();
        assert_eq!(card.tone, CardTone::Warning);
        assert_eq!(card.action_state, CardActionState::Watching);
        assert!(card.status_line.contains("Checklist still in progress"));
    }
}

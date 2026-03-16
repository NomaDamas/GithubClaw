//! Tab definitions for the TUI.

use serde::{Deserialize, Serialize};

/// Available TUI tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tab {
    IssueRequest,
    Monitoring,
}

impl Tab {
    pub fn title(&self) -> &'static str {
        match self {
            Self::IssueRequest => "Issue Request",
            Self::Monitoring => "Monitoring",
        }
    }

    pub fn all() -> &'static [Tab] {
        &[Tab::IssueRequest, Tab::Monitoring]
    }

    pub fn next(&self) -> Tab {
        match self {
            Self::IssueRequest => Self::Monitoring,
            Self::Monitoring => Self::IssueRequest,
        }
    }

    pub fn prev(&self) -> Tab {
        match self {
            Self::IssueRequest => Self::Monitoring,
            Self::Monitoring => Self::IssueRequest,
        }
    }
}

/// An issue awaiting interactive session (shown in Issue Request tab).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueRequestItem {
    pub issue_number: u64,
    pub title: String,
    pub issue_type: String, // "Feature" or "Refactoring"
    pub vision_report_ready: bool,
}

/// An active agent session (shown in Monitoring tab).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionItem {
    pub issue_number: u64,
    pub agent_type: String,
    pub status: AgentStatus,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Running,
    Queued,
    Completed,
    Failed,
    Idle,
}

impl AgentStatus {
    pub fn symbol(&self) -> &'static str {
        match self {
            Self::Running => ">>",
            Self::Queued => "..",
            Self::Completed => "ok",
            Self::Failed => "!!",
            Self::Idle => "--",
        }
    }
}

/// Agent execution timeline entry for session detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub agent_type: String,
    pub status: AgentStatus,
    pub detail: String,
}

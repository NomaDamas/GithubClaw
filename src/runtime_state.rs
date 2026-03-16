//! Disk-backed runtime state shared between the server/agents and the TUI.
//!
//! The webhook server and worker CLI run in separate processes, so the TUI
//! cannot read their in-memory state directly. This module persists a compact
//! per-issue snapshot under `~/.githubclaw/sessions/<repo>/<issue>/runtime.json`
//! so `githubclaw tui` can reconstruct issue requests and monitoring state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::global_config_dir;
use crate::markers::MarkerType;
use crate::pipeline::{IssueType, PipelineState, PipelineTracker};
use crate::tui::tabs::{AgentSessionItem, AgentStatus, IssueRequestItem, TimelineEntry};

const RUNTIME_STATE_FILENAME: &str = "runtime.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueRuntimeSnapshot {
    pub repo: String,
    pub issue_number: u64,
    pub title: String,
    pub tracker: Option<PipelineTracker>,
    pub vision_report_ready: bool,
    pub agent_sessions: Vec<AgentSessionItem>,
    pub agent_timeline: Vec<TimelineEntry>,
    pub updated_at_unix_seconds: u64,
}

impl IssueRuntimeSnapshot {
    pub fn new(repo: &str, issue_number: u64) -> Self {
        Self {
            repo: repo.to_string(),
            issue_number,
            title: String::new(),
            tracker: None,
            vision_report_ready: false,
            agent_sessions: Vec::new(),
            agent_timeline: Vec::new(),
            updated_at_unix_seconds: unix_timestamp_now(),
        }
    }

    pub fn to_issue_request_item(&self) -> Option<IssueRequestItem> {
        let tracker = self.tracker.as_ref()?;
        if tracker.state != PipelineState::AwaitingInteractiveSession {
            return None;
        }

        let issue_type = match tracker.issue_type.as_ref() {
            Some(IssueType::Feature) => "Feature",
            Some(IssueType::Refactoring) => "Refactoring",
            Some(IssueType::Bug) | None => return None,
        };

        Some(IssueRequestItem {
            issue_number: self.issue_number,
            title: if self.title.is_empty() {
                format!("Issue #{}", self.issue_number)
            } else {
                self.title.clone()
            },
            issue_type: issue_type.to_string(),
            vision_report_ready: self.vision_report_ready,
        })
    }

    pub fn apply_event(&mut self, payload: &serde_json::Value, root_issue: u64) {
        self.issue_number = root_issue;
        if self.repo.is_empty() {
            self.repo = payload
                .pointer("/repository/full_name")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
        }
        if let Some(title) = payload
            .pointer("/issue/title")
            .or_else(|| payload.pointer("/pull_request/title"))
            .or_else(|| payload.pointer("/data/title"))
            .and_then(|value| value.as_str())
            .filter(|title| !title.trim().is_empty())
        {
            self.title = title.to_string();
        }

        if let Some(comment_body) = payload
            .pointer("/comment/body")
            .and_then(|value| value.as_str())
        {
            if let Some(summary) = crate::markers::extract_summary(comment_body) {
                self.apply_summary(&summary);
            }
            for marker in crate::markers::parse_markers(comment_body) {
                self.apply_marker(&marker.marker_type, &marker.attributes);
            }
        }

        self.updated_at_unix_seconds = unix_timestamp_now();
    }

    pub fn note_agent_started(
        &mut self,
        agent_type: &str,
        started_at: &str,
        detail: impl Into<String>,
    ) {
        let detail = detail.into();
        self.ensure_pipeline_for_agent(agent_type);
        upsert_agent_session(
            &mut self.agent_sessions,
            self.issue_number,
            agent_type,
            AgentStatus::Running,
            started_at.to_string(),
        );
        self.agent_timeline.push(TimelineEntry {
            agent_type: agent_type.to_string(),
            status: AgentStatus::Running,
            detail,
        });
        self.updated_at_unix_seconds = unix_timestamp_now();
    }

    fn ensure_pipeline_for_agent(&mut self, agent_type: &str) {
        match agent_type {
            "bug-reproducer" => {
                let mut tracker = self
                    .tracker
                    .clone()
                    .unwrap_or_else(|| PipelineTracker::new(self.issue_number));
                if tracker.issue_type.is_none() {
                    tracker.classify(IssueType::Bug);
                } else {
                    tracker.state = PipelineState::Reproducing;
                }
                self.tracker = Some(tracker);
            }
            "vision-gap-analyst" => {
                let mut tracker = self
                    .tracker
                    .clone()
                    .unwrap_or_else(|| PipelineTracker::new(self.issue_number));
                tracker.state = PipelineState::AnalyzingVision;
                self.tracker = Some(tracker);
            }
            _ => {}
        }
    }

    pub fn note_agent_finished(
        &mut self,
        agent_type: &str,
        started_at: &str,
        succeeded: bool,
        detail: impl Into<String>,
    ) {
        let detail = detail.into();
        upsert_agent_session(
            &mut self.agent_sessions,
            self.issue_number,
            agent_type,
            if succeeded {
                AgentStatus::Completed
            } else {
                AgentStatus::Failed
            },
            started_at.to_string(),
        );
        self.agent_timeline.push(TimelineEntry {
            agent_type: agent_type.to_string(),
            status: if succeeded {
                AgentStatus::Completed
            } else {
                AgentStatus::Failed
            },
            detail,
        });
        self.updated_at_unix_seconds = unix_timestamp_now();
    }

    fn apply_summary(&mut self, summary: &str) {
        let Some(issue_type) = parse_issue_type_from_summary(summary) else {
            return;
        };
        let mut tracker = self
            .tracker
            .clone()
            .unwrap_or_else(|| PipelineTracker::new(self.issue_number));
        if tracker.issue_type.is_none() {
            tracker.classify(issue_type);
        }
        tracker.on_vision_analysis_done();
        self.tracker = Some(tracker);
        self.vision_report_ready = true;
        self.note_agent_finished(
            "vision-gap-analyst",
            &timestamp_label(),
            true,
            "Vision-gap analysis completed; awaiting interactive session",
        );
    }

    fn apply_marker(&mut self, marker: &MarkerType, attrs: &HashMap<String, String>) {
        let mut tracker = self
            .tracker
            .clone()
            .unwrap_or_else(|| PipelineTracker::new(self.issue_number));
        if tracker.issue_type.is_none() && matches!(marker, MarkerType::Reproduced) {
            tracker.classify(IssueType::Bug);
        }
        let _ = tracker.on_marker(marker, attrs);
        self.tracker = Some(tracker);
        self.updated_at_unix_seconds = unix_timestamp_now();
    }
}

pub type IssueRuntimeState = IssueRuntimeSnapshot;

pub struct RuntimeStateStore {
    base_dir: PathBuf,
}

impl RuntimeStateStore {
    pub fn new() -> Self {
        Self {
            base_dir: global_config_dir().join("sessions"),
        }
    }

    pub fn with_base_dir(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    fn repo_dir(&self, repo: &str) -> PathBuf {
        self.base_dir.join(repo.replace('/', "_"))
    }

    fn issue_dir(&self, repo: &str, issue_number: u64) -> PathBuf {
        self.repo_dir(repo).join(issue_number.to_string())
    }

    fn runtime_path(&self, repo: &str, issue_number: u64) -> PathBuf {
        self.issue_dir(repo, issue_number)
            .join(RUNTIME_STATE_FILENAME)
    }

    pub fn load_issue(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> std::io::Result<Option<IssueRuntimeSnapshot>> {
        let path = self.runtime_path(repo, issue_number);
        if !path.exists() {
            return Ok(None);
        }

        let data = std::fs::read_to_string(path)?;
        let state = serde_json::from_str(&data)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        Ok(Some(state))
    }

    pub fn save_issue(&self, state: &IssueRuntimeSnapshot) -> std::io::Result<()> {
        let path = self.runtime_path(&state.repo, state.issue_number);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(state)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        std::fs::write(path, json)
    }

    pub fn list_issue_states(&self) -> std::io::Result<Vec<IssueRuntimeSnapshot>> {
        let mut states = Vec::new();
        if !self.base_dir.exists() {
            return Ok(states);
        }

        for repo_entry in std::fs::read_dir(&self.base_dir)? {
            let repo_entry = repo_entry?;
            if !repo_entry.file_type()?.is_dir() {
                continue;
            }

            for issue_entry in std::fs::read_dir(repo_entry.path())? {
                let issue_entry = issue_entry?;
                if !issue_entry.file_type()?.is_dir() {
                    continue;
                }
                if let Some(state) =
                    load_issue_state_from_dir(&repo_entry.path(), &issue_entry.path())
                {
                    states.push(state);
                }
            }
        }

        states.sort_by(|a, b| {
            b.updated_at_unix_seconds
                .cmp(&a.updated_at_unix_seconds)
                .then_with(|| a.issue_number.cmp(&b.issue_number))
        });
        Ok(states)
    }

    pub fn upsert_issue_metadata(
        &self,
        repo: &str,
        issue_number: u64,
        title: Option<&str>,
    ) -> std::io::Result<()> {
        self.update_issue(repo, issue_number, |state| {
            if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
                state.title = title.to_string();
            }
        })
    }

    pub fn record_vision_analysis(
        &self,
        repo: &str,
        issue_number: u64,
        title: Option<&str>,
        summary: &str,
    ) -> std::io::Result<()> {
        self.update_issue(repo, issue_number, |state| {
            if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
                state.title = title.to_string();
            }

            let Some(issue_type) = parse_issue_type_from_summary(summary) else {
                return;
            };

            let mut tracker = state
                .tracker
                .clone()
                .unwrap_or_else(|| PipelineTracker::new(issue_number));
            if tracker.issue_type.is_none() {
                tracker.classify(issue_type);
            }
            tracker.on_vision_analysis_done();

            state.tracker = Some(tracker);
            state.vision_report_ready = true;
            upsert_agent_session(
                &mut state.agent_sessions,
                issue_number,
                "vision_gap_analyst",
                AgentStatus::Completed,
                timestamp_label(),
            );
            state.agent_timeline.push(TimelineEntry {
                agent_type: "vision_gap_analyst".into(),
                status: AgentStatus::Completed,
                detail: "Vision-gap analysis completed; awaiting interactive session".into(),
            });
        })
    }

    pub fn record_marker(
        &self,
        repo: &str,
        issue_number: u64,
        title: Option<&str>,
        marker: &MarkerType,
        attrs: &HashMap<String, String>,
    ) -> std::io::Result<()> {
        self.update_issue(repo, issue_number, |state| {
            if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
                state.title = title.to_string();
            }

            let mut tracker = state
                .tracker
                .clone()
                .unwrap_or_else(|| PipelineTracker::new(issue_number));
            if tracker.issue_type.is_none() && matches!(marker, MarkerType::Reproduced) {
                tracker.classify(IssueType::Bug);
            }
            let _ = tracker.on_marker(marker, attrs);
            state.tracker = Some(tracker);

            state.agent_timeline.push(TimelineEntry {
                agent_type: "pipeline".into(),
                status: AgentStatus::Completed,
                detail: format!("Observed marker {}", marker.as_str()),
            });
        })
    }

    pub fn record_agent_status(
        &self,
        repo: &str,
        issue_number: u64,
        title: Option<&str>,
        agent_type: &str,
        status: AgentStatus,
        detail: impl Into<String>,
    ) -> std::io::Result<()> {
        let detail = detail.into();
        self.update_issue(repo, issue_number, |state| {
            if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
                state.title = title.to_string();
            }

            let started_at = match status {
                AgentStatus::Running => timestamp_label(),
                _ => state
                    .agent_sessions
                    .iter()
                    .find(|session| session.agent_type == agent_type)
                    .map(|session| session.started_at.clone())
                    .unwrap_or_else(timestamp_label),
            };

            upsert_agent_session(
                &mut state.agent_sessions,
                issue_number,
                agent_type,
                status.clone(),
                started_at,
            );
            state.agent_timeline.push(TimelineEntry {
                agent_type: agent_type.to_string(),
                status,
                detail,
            });
        })
    }

    fn update_issue(
        &self,
        repo: &str,
        issue_number: u64,
        update: impl FnOnce(&mut IssueRuntimeSnapshot),
    ) -> std::io::Result<()> {
        let mut state = self
            .load_issue(repo, issue_number)?
            .unwrap_or_else(|| IssueRuntimeSnapshot::new(repo, issue_number));
        update(&mut state);
        state.updated_at_unix_seconds = unix_timestamp_now();
        self.save_issue(&state)
    }
}

impl Default for RuntimeStateStore {
    fn default() -> Self {
        Self::new()
    }
}

pub fn parse_issue_type_from_summary(summary: &str) -> Option<IssueType> {
    for line in summary.lines() {
        let trimmed = line.trim().trim_matches('*').trim();
        if !trimmed.to_ascii_lowercase().contains("classification") {
            continue;
        }

        if trimmed.contains("Refactoring") {
            return Some(IssueType::Refactoring);
        }
        if trimmed.contains("Feature") {
            return Some(IssueType::Feature);
        }
        if trimmed.contains("Bug") {
            return Some(IssueType::Bug);
        }
    }

    None
}

fn upsert_agent_session(
    sessions: &mut Vec<AgentSessionItem>,
    issue_number: u64,
    agent_type: &str,
    status: AgentStatus,
    started_at: String,
) {
    if let Some(session) = sessions
        .iter_mut()
        .find(|session| session.issue_number == issue_number && session.agent_type == agent_type)
    {
        session.status = status;
        session.started_at = started_at;
        return;
    }

    sessions.push(AgentSessionItem {
        issue_number,
        agent_type: agent_type.to_string(),
        status,
        started_at,
    });
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn timestamp_label() -> String {
    unix_timestamp_now().to_string()
}

fn load_issue_state_from_dir(repo_dir: &Path, issue_dir: &Path) -> Option<IssueRuntimeSnapshot> {
    let runtime_path = issue_dir.join(RUNTIME_STATE_FILENAME);
    if runtime_path.exists() {
        let data = std::fs::read_to_string(&runtime_path).ok()?;
        return serde_json::from_str::<IssueRuntimeSnapshot>(&data).ok();
    }

    let issue_number: u64 = issue_dir.file_name()?.to_string_lossy().parse().ok()?;
    let repo = repo_dir.file_name()?.to_string_lossy().replace('_', "/");
    let mut state = IssueRuntimeSnapshot::new(&repo, issue_number);

    let pipeline_path = issue_dir.join("pipeline.json");
    if pipeline_path.exists() {
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pipeline_path).ok()?).ok()?;
        state.title = value
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        state.vision_report_ready = matches!(
            value.get("state").and_then(|value| value.as_str()),
            Some("awaiting_interactive_session")
        );
        let issue_type = match value.get("issue_type").and_then(|value| value.as_str()) {
            Some("feature") => Some(IssueType::Feature),
            Some("refactoring") => Some(IssueType::Refactoring),
            Some("bug") => Some(IssueType::Bug),
            _ => None,
        };
        state.tracker = Some(PipelineTracker {
            issue_id: issue_number,
            issue_type,
            state: match value.get("state").and_then(|value| value.as_str()) {
                Some("awaiting_interactive_session") => PipelineState::AwaitingInteractiveSession,
                Some("analyzing_vision") => PipelineState::AnalyzingVision,
                Some("approved") => PipelineState::Approved,
                _ => PipelineState::Classifying,
            },
            sub_issues: Vec::new(),
            current_sub_issue_index: 0,
            branch_name: None,
        });
    }

    let agent_session_path = issue_dir.join("agent_session.json");
    if agent_session_path.exists() {
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&agent_session_path).ok()?).ok()?;
        let status = parse_agent_status(value.get("status").and_then(|value| value.as_str()));
        state.agent_sessions.push(AgentSessionItem {
            issue_number,
            agent_type: value
                .get("agent_type")
                .and_then(|value| value.as_str())
                .unwrap_or("agent")
                .to_string(),
            status: status.clone(),
            started_at: value
                .get("started_at")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
        });
        state.agent_timeline = value
            .get("timeline")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                Some(TimelineEntry {
                    agent_type: entry.get("agent_type")?.as_str()?.to_string(),
                    status: parse_agent_status(
                        entry.get("status").and_then(|value| value.as_str()),
                    ),
                    detail: entry.get("detail")?.as_str()?.to_string(),
                })
            })
            .collect();
    }

    if state.title.is_empty() && state.agent_sessions.is_empty() && state.tracker.is_none() {
        return None;
    }

    Some(state)
}

fn parse_agent_status(raw: Option<&str>) -> AgentStatus {
    match raw.unwrap_or_default() {
        "running" => AgentStatus::Running,
        "queued" => AgentStatus::Queued,
        "completed" => AgentStatus::Completed,
        "failed" => AgentStatus::Failed,
        _ => AgentStatus::Idle,
    }
}

pub fn sessions_dir_from_home(home: &Path) -> PathBuf {
    home.join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn vision_analysis_moves_issue_into_interactive_queue() {
        let tmp = TempDir::new().unwrap();
        let store = RuntimeStateStore::with_base_dir(tmp.path().join("sessions"));

        store
            .record_vision_analysis(
                "owner/repo",
                42,
                Some("Dark mode"),
                "## Vision Alignment Analysis\n**Classification**: Feature",
            )
            .unwrap();

        let state = store.load_issue("owner/repo", 42).unwrap().unwrap();
        assert!(state.vision_report_ready);
        assert!(matches!(
            state.tracker.as_ref().map(|tracker| &tracker.state),
            Some(PipelineState::AwaitingInteractiveSession)
        ));
        assert_eq!(
            state.to_issue_request_item().unwrap().issue_type,
            "Feature".to_string()
        );
    }

    #[test]
    fn agent_status_updates_existing_session_and_timeline() {
        let tmp = TempDir::new().unwrap();
        let store = RuntimeStateStore::with_base_dir(tmp.path().join("sessions"));

        store
            .record_agent_status(
                "owner/repo",
                7,
                Some("Polish dashboard"),
                "implementer",
                AgentStatus::Running,
                "Implementer started",
            )
            .unwrap();
        store
            .record_agent_status(
                "owner/repo",
                7,
                None,
                "implementer",
                AgentStatus::Completed,
                "Implementer finished",
            )
            .unwrap();

        let state = store.load_issue("owner/repo", 7).unwrap().unwrap();
        assert_eq!(state.agent_sessions.len(), 1);
        assert!(matches!(
            state.agent_sessions[0].status,
            AgentStatus::Completed
        ));
        assert_eq!(state.agent_timeline.len(), 2);
        assert_eq!(state.title, "Polish dashboard");
    }

    #[test]
    fn list_issue_states_ignores_missing_runtime_files() {
        let tmp = TempDir::new().unwrap();
        let store = RuntimeStateStore::with_base_dir(tmp.path().join("sessions"));
        std::fs::create_dir_all(tmp.path().join("sessions/owner_repo/99")).unwrap();

        let states = store.list_issue_states().unwrap();
        assert!(states.is_empty());
    }
}

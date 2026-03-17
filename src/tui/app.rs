//! TUI application state and main loop.

use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::VecDeque;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::event::{is_quit, AppEvent};
use super::tabs::*;
use crate::constants::MONITOR_HISTORY_MINUTES;
use crate::runtime_state::{sessions_dir_from_home, IssueRuntimeState, RuntimeStateStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueReviewDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingIssueReview {
    pub issue_number: u64,
    pub decision: IssueReviewDecision,
}

/// Main TUI application state.
pub struct App {
    /// Currently active tab.
    pub active_tab: Tab,
    /// Whether the app should quit.
    pub should_quit: bool,

    // Issue Request tab state
    pub issue_requests: Vec<IssueRequestItem>,
    pub selected_issue_index: usize,
    pub interactive_session_active: bool,
    /// Issue number pending interactive session launch (consumed by TUI main loop).
    pub pending_interactive_issue: Option<u64>,
    /// Approval or rejection action pending GitHub write (consumed by TUI main loop).
    pub pending_issue_review: Option<PendingIssueReview>,
    /// Active PTY session for interactive Claude Code / Codex.
    pub pty_session: Option<super::pty::PtySession>,
    /// Accumulated PTY output text for rendering.
    pub pty_output: String,

    // Monitoring tab state
    pub agent_sessions: Vec<AgentSessionItem>,
    pub selected_agent_index: usize,
    pub agent_timeline: Vec<TimelineEntry>,
    pub rate_limit_tier: String,
    pub queue_depth: usize,
    pub worker_count: (usize, usize), // (active, max)
    pub oldest_queue_age_seconds: Option<u64>,
    pub queue_history: VecDeque<u64>,
    pub worker_history: VecDeque<u64>,
    pub activity_history: VecDeque<u64>,
    pub recent_events: VecDeque<String>,
    last_history_minute: Option<u64>,
    last_timeline_len: usize,
}

impl App {
    pub fn new() -> Self {
        Self {
            active_tab: Tab::IssueRequest,
            should_quit: false,
            issue_requests: Vec::new(),
            selected_issue_index: 0,
            interactive_session_active: false,
            pending_interactive_issue: None,
            pending_issue_review: None,
            pty_session: None,
            pty_output: String::new(),
            agent_sessions: Vec::new(),
            selected_agent_index: 0,
            agent_timeline: Vec::new(),
            rate_limit_tier: "None".into(),
            queue_depth: 0,
            worker_count: (0, 8),
            oldest_queue_age_seconds: None,
            queue_history: VecDeque::with_capacity(MONITOR_HISTORY_MINUTES),
            worker_history: VecDeque::with_capacity(MONITOR_HISTORY_MINUTES),
            activity_history: VecDeque::with_capacity(MONITOR_HISTORY_MINUTES),
            recent_events: VecDeque::with_capacity(8),
            last_history_minute: None,
            last_timeline_len: 0,
        }
    }

    /// Refresh data from disk (called on Tick events).
    ///
    /// Reads queue depths, process states, and session info from the
    /// GithubClaw home directory. This is a lightweight polling approach
    /// that doesn't require IPC with the webhook server.
    pub fn refresh_from_disk(&mut self) {
        let home = crate::config::global_config_dir();
        self.refresh_from_disk_root(&home);
    }

    fn refresh_from_disk_root(&mut self, home: &Path) {
        // Read queue depths
        let runtime_dir = crate::config::runtime_dir_from_home(home);
        let mut queue_depth = 0usize;
        let mut oldest_queue_age_seconds: Option<u64> = None;
        if let Ok(repo_dirs) = std::fs::read_dir(&runtime_dir) {
            for repo_dir in repo_dirs.flatten().map(|entry| entry.path()) {
                let queue_dir = repo_dir.join("queue");
                if !queue_dir.is_dir() {
                    continue;
                }
                queue_depth += std::fs::read_dir(&queue_dir)
                    .map(|entries| {
                        entries
                            .filter_map(|e| e.ok())
                            .filter(|entry| {
                                entry.path().is_file()
                                    && entry.path().extension().is_some_and(|ext| ext == "json")
                            })
                            .count()
                    })
                    .unwrap_or(0);
                if let Some(age) = oldest_entry_age_seconds(&queue_dir, SystemTime::now()) {
                    oldest_queue_age_seconds = Some(match oldest_queue_age_seconds {
                        Some(current) => current.max(age),
                        None => age,
                    });
                }
            }
        }
        self.queue_depth = queue_depth;
        self.oldest_queue_age_seconds = oldest_queue_age_seconds;

        // Read registry for repo list
        let registry_path = home.join("registry.json");
        if registry_path.exists() {
            if let Ok(data) = std::fs::read_to_string(&registry_path) {
                if let Ok(registry) = serde_json::from_str::<serde_json::Value>(&data) {
                    // Could populate repo list for issue request tab
                    let _repos = registry
                        .get("repos")
                        .and_then(|v| v.as_object())
                        .map(|m| m.keys().cloned().collect::<Vec<_>>());
                }
            }
        }

        self.refresh_runtime_views_from_disk(home);

        self.refresh_recent_events();
        self.capture_monitoring_sample(SystemTime::now());
    }

    fn refresh_runtime_views_from_disk(&mut self, home: &Path) {
        let previously_selected_issue = self.selected_issue().map(|issue| issue.issue_number);
        let previously_selected_agent = self
            .selected_agent_session()
            .map(|session| (session.issue_number, session.agent_type.clone()));

        let store = RuntimeStateStore::with_base_dir(sessions_dir_from_home(home));
        let Ok(states) = store.list_issue_states() else {
            return;
        };

        self.issue_requests = build_issue_requests(&states);
        self.agent_sessions = build_agent_sessions(&states);
        self.worker_count = (
            self.agent_sessions
                .iter()
                .filter(|session| matches!(session.status, AgentStatus::Running))
                .count(),
            self.worker_count.1,
        );

        self.selected_issue_index =
            find_issue_index(&self.issue_requests, previously_selected_issue).unwrap_or_default();
        self.selected_agent_index =
            find_agent_index(&self.agent_sessions, previously_selected_agent).unwrap_or_default();

        self.agent_timeline = selected_timeline(
            &states,
            self.selected_agent_session(),
            self.selected_issue(),
        );
    }

    /// Handle an application event.
    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => {
                if is_quit(&key) && !self.interactive_session_active {
                    self.should_quit = true;
                    return;
                }
                self.handle_key(key.code, key.modifiers);
            }
            AppEvent::Tick => {
                self.refresh_from_disk();
                // Poll PTY output if active
                if let Some(ref pty) = self.pty_session {
                    let new_output = pty.read_output();
                    if !new_output.is_empty() {
                        self.pty_output
                            .push_str(&String::from_utf8_lossy(&new_output));
                        // Cap at 64KB
                        if self.pty_output.len() > 64 * 1024 {
                            let drain_to = self.pty_output.len() - 32 * 1024;
                            self.pty_output.drain(..drain_to);
                        }
                    }
                    if pty.has_exited() {
                        self.pty_session = None;
                        self.interactive_session_active = false;
                        self.pending_issue_review = None;
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        // Global keybindings
        match code {
            KeyCode::Tab => {
                if !self.interactive_session_active {
                    self.active_tab = self.active_tab.next();
                }
                return;
            }
            KeyCode::BackTab => {
                if !self.interactive_session_active {
                    self.active_tab = self.active_tab.prev();
                }
                return;
            }
            _ => {}
        }

        // Tab-specific keybindings
        match self.active_tab {
            Tab::IssueRequest => self.handle_issue_request_key(code, modifiers),
            Tab::Monitoring => self.handle_monitoring_key(code, modifiers),
        }
    }

    fn handle_issue_request_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        if self.interactive_session_active {
            // In interactive session, Escape exits back to list
            if code == KeyCode::Esc {
                self.interactive_session_active = false;
                self.pty_session = None;
                self.pty_output.clear();
                return;
            }
            if code == KeyCode::Char('a') && modifiers.contains(KeyModifiers::CONTROL) {
                self.queue_selected_issue_review(IssueReviewDecision::Approve);
                return;
            }
            if code == KeyCode::Char('r') && modifiers.contains(KeyModifiers::CONTROL) {
                self.queue_selected_issue_review(IssueReviewDecision::Reject);
                return;
            }
            // Forward keys to PTY session
            if let Some(ref pty) = self.pty_session {
                let bytes: Vec<u8> = match code {
                    KeyCode::Char(c) => {
                        if modifiers.contains(KeyModifiers::CONTROL) {
                            // Ctrl+C = 0x03, Ctrl+D = 0x04, etc.
                            vec![(c as u8).wrapping_sub(b'`')]
                        } else {
                            let mut buf = [0u8; 4];
                            c.encode_utf8(&mut buf).as_bytes().to_vec()
                        }
                    }
                    KeyCode::Enter => vec![b'\n'],
                    KeyCode::Backspace => vec![0x7f],
                    KeyCode::Tab => vec![b'\t'],
                    KeyCode::Up => vec![0x1b, b'[', b'A'],
                    KeyCode::Down => vec![0x1b, b'[', b'B'],
                    KeyCode::Right => vec![0x1b, b'[', b'C'],
                    KeyCode::Left => vec![0x1b, b'[', b'D'],
                    _ => vec![],
                };
                if !bytes.is_empty() {
                    let _ = pty.send_input(&bytes);
                }
            }
            return;
        }

        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.issue_requests.is_empty() {
                    self.selected_issue_index =
                        (self.selected_issue_index + 1) % self.issue_requests.len();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.issue_requests.is_empty() {
                    self.selected_issue_index = self
                        .selected_issue_index
                        .checked_sub(1)
                        .unwrap_or(self.issue_requests.len() - 1);
                }
            }
            KeyCode::Enter => {
                if let Some(issue) = self.selected_issue() {
                    self.pending_interactive_issue = Some(issue.issue_number);
                }
            }
            KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.queue_selected_issue_review(IssueReviewDecision::Approve);
            }
            KeyCode::Char('r') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.queue_selected_issue_review(IssueReviewDecision::Reject);
            }
            _ => {}
        }
    }

    fn handle_monitoring_key(&mut self, code: KeyCode, _modifiers: KeyModifiers) {
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.agent_sessions.is_empty() {
                    self.selected_agent_index =
                        (self.selected_agent_index + 1) % self.agent_sessions.len();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.agent_sessions.is_empty() {
                    self.selected_agent_index = self
                        .selected_agent_index
                        .checked_sub(1)
                        .unwrap_or(self.agent_sessions.len() - 1);
                }
            }
            _ => {}
        }
    }

    fn capture_monitoring_sample(&mut self, now: SystemTime) {
        let elapsed = now.duration_since(UNIX_EPOCH).unwrap_or_default();
        self.capture_monitoring_sample_for_second(elapsed.as_secs());
    }

    fn capture_monitoring_sample_for_second(&mut self, unix_seconds: u64) {
        let minute_bucket = unix_seconds / 60;
        if self.last_history_minute == Some(minute_bucket) {
            return;
        }

        let activity_delta = self
            .agent_timeline
            .len()
            .saturating_sub(self.last_timeline_len) as u64;
        self.last_timeline_len = self.agent_timeline.len();

        push_history_sample(&mut self.queue_history, self.queue_depth as u64);
        push_history_sample(&mut self.worker_history, self.worker_count.0 as u64);
        push_history_sample(&mut self.activity_history, activity_delta);

        self.last_history_minute = Some(minute_bucket);
    }

    fn refresh_recent_events(&mut self) {
        self.recent_events.clear();
        let issue_label = self
            .selected_issue_number_for_timeline()
            .map(|issue| format!("#{issue}"))
            .unwrap_or_else(|| "issue".to_string());
        for entry in self.agent_timeline.iter().rev().take(6) {
            let event = format!(
                "{} {} {} {}",
                issue_label,
                entry.agent_type,
                entry.status.symbol(),
                entry.detail
            );
            self.recent_events.push_back(event);
        }
    }

    pub fn selected_agent_session(&self) -> Option<&AgentSessionItem> {
        self.agent_sessions.get(self.selected_agent_index)
    }

    pub fn selected_issue(&self) -> Option<&IssueRequestItem> {
        self.issue_requests.get(self.selected_issue_index)
    }

    pub fn selected_issue_number_for_timeline(&self) -> Option<u64> {
        self.selected_agent_session()
            .map(|session| session.issue_number)
    }

    pub fn take_pending_interactive_issue(&mut self) -> Option<u64> {
        self.pending_interactive_issue.take()
    }

    pub fn take_pending_issue_review(&mut self) -> Option<PendingIssueReview> {
        self.pending_issue_review.take()
    }

    pub fn start_interactive_session_for_issue(&mut self, issue_number: u64, repo_root: &Path) {
        self.start_interactive_session_for_issue_with(
            issue_number,
            repo_root,
            |backend, session_name, working_dir, cols, rows| {
                super::pty::PtySession::spawn(backend, session_name, working_dir, cols, rows)
            },
        );
    }

    fn start_interactive_session_for_issue_with<F>(
        &mut self,
        issue_number: u64,
        repo_root: &Path,
        launcher: F,
    ) where
        F: FnOnce(
            super::pty::InteractiveBackend,
            &str,
            &str,
            u16,
            u16,
        ) -> Result<super::pty::PtySession, String>,
    {
        let backend = super::pty::InteractiveBackend::for_repo(repo_root);
        let session_name = format!("githubclaw-issue-{issue_number}");
        let working_dir = repo_root.to_string_lossy().to_string();

        match launcher(backend, &session_name, &working_dir, 80, 24) {
            Ok(pty) => {
                self.pty_session = Some(pty);
                self.pty_output.clear();
                self.interactive_session_active = true;
            }
            Err(err) => {
                self.pty_session = None;
                self.interactive_session_active = false;
                self.pty_output = format!("Failed to spawn {}: {}", backend.display_name(), err);
            }
        }
    }

    pub fn apply_issue_review(
        &mut self,
        repo_root: &Path,
        review: PendingIssueReview,
    ) -> Result<(), String> {
        self.apply_issue_review_with(repo_root, review, |repo_root, args| {
            run_gh_command(repo_root, args)
        })
    }

    fn apply_issue_review_with<F>(
        &mut self,
        repo_root: &Path,
        review: PendingIssueReview,
        runner: F,
    ) -> Result<(), String>
    where
        F: FnOnce(&Path, &[String]) -> Result<(), String>,
    {
        let result = (|| {
            let repo_slug = detect_repo_slug(repo_root)?;
            let args = issue_review_command_args(&repo_slug, review);
            runner(repo_root, &args)
        })();

        match result {
            Ok(()) => {
                self.interactive_session_active = false;
                self.pty_session = None;
                self.pty_output.clear();
                Ok(())
            }
            Err(err) => {
                let message = format!(
                    "[githubclaw] Failed to {} issue #{}: {}",
                    review.decision.label(),
                    review.issue_number,
                    err
                );
                if self.pty_output.is_empty() {
                    self.pty_output = message;
                } else {
                    self.pty_output.push('\n');
                    self.pty_output.push_str(&message);
                }
                Err(err)
            }
        }
    }

    fn queue_selected_issue_review(&mut self, decision: IssueReviewDecision) {
        if let Some(issue) = self.selected_issue() {
            self.pending_issue_review = Some(PendingIssueReview {
                issue_number: issue.issue_number,
                decision,
            });
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

fn push_history_sample(history: &mut VecDeque<u64>, value: u64) {
    if history.len() == MONITOR_HISTORY_MINUTES {
        history.pop_front();
    }
    history.push_back(value);
}

fn oldest_entry_age_seconds(dir: &Path, now: SystemTime) -> Option<u64> {
    let mut oldest_age = None;
    let entries = std::fs::read_dir(dir).ok()?;

    for entry in entries.filter_map(|entry| entry.ok()) {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        oldest_age =
            Some(oldest_age.map_or(age.as_secs(), |current: u64| current.max(age.as_secs())));
    }

    oldest_age
}

fn build_issue_requests(states: &[IssueRuntimeState]) -> Vec<IssueRequestItem> {
    let mut items: Vec<IssueRequestItem> = states
        .iter()
        .filter_map(IssueRuntimeState::to_issue_request_item)
        .collect();
    items.sort_by_key(|item| item.issue_number);
    items
}

fn build_agent_sessions(states: &[IssueRuntimeState]) -> Vec<AgentSessionItem> {
    let mut sessions: Vec<AgentSessionItem> = states
        .iter()
        .flat_map(|state| state.agent_sessions.clone())
        .collect();
    sessions.sort_by(|left, right| {
        agent_status_rank(&left.status)
            .cmp(&agent_status_rank(&right.status))
            .then_with(|| left.issue_number.cmp(&right.issue_number))
            .then_with(|| left.agent_type.cmp(&right.agent_type))
    });
    sessions
}

fn selected_timeline(
    states: &[IssueRuntimeState],
    selected_agent: Option<&AgentSessionItem>,
    selected_issue: Option<&IssueRequestItem>,
) -> Vec<TimelineEntry> {
    let selected_issue_number = selected_agent
        .map(|session| session.issue_number)
        .or_else(|| selected_issue.map(|issue| issue.issue_number));
    let Some(issue_number) = selected_issue_number else {
        return Vec::new();
    };

    states
        .iter()
        .find(|state| state.issue_number == issue_number)
        .map(|state| state.agent_timeline.clone())
        .unwrap_or_default()
}

fn agent_status_rank(status: &AgentStatus) -> usize {
    match status {
        AgentStatus::Running => 0,
        AgentStatus::Queued => 1,
        AgentStatus::Failed => 2,
        AgentStatus::Completed => 3,
        AgentStatus::Idle => 4,
    }
}

fn find_issue_index(items: &[IssueRequestItem], issue_number: Option<u64>) -> Option<usize> {
    let issue_number = issue_number?;
    items
        .iter()
        .position(|item| item.issue_number == issue_number)
}

fn find_agent_index(
    sessions: &[AgentSessionItem],
    selected: Option<(u64, String)>,
) -> Option<usize> {
    let (issue_number, agent_type) = selected?;
    sessions.iter().position(|session| {
        session.issue_number == issue_number && session.agent_type == agent_type
    })
}

impl IssueReviewDecision {
    fn label(&self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
        }
    }
}

fn issue_review_command_args(repo_slug: &str, review: PendingIssueReview) -> Vec<String> {
    match review.decision {
        IssueReviewDecision::Approve => vec![
            "issue".to_string(),
            "comment".to_string(),
            review.issue_number.to_string(),
            "--repo".to_string(),
            repo_slug.to_string(),
            "--body".to_string(),
            format!(
                "<!-- githubclaw:approved -->\nApproved via GithubClaw TUI interactive session."
            ),
        ],
        IssueReviewDecision::Reject => vec![
            "issue".to_string(),
            "close".to_string(),
            review.issue_number.to_string(),
            "--repo".to_string(),
            repo_slug.to_string(),
            "--reason".to_string(),
            "not planned".to_string(),
            "--comment".to_string(),
            "Rejected via GithubClaw TUI interactive session.".to_string(),
        ],
    }
}

fn run_gh_command(repo_root: &Path, args: &[String]) -> Result<(), String> {
    let output = std::process::Command::new("gh")
        .args(args)
        .current_dir(repo_root)
        .output()
        .map_err(|err| format!("failed to run gh: {err}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return Err(stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return Err(stdout);
    }

    Err(format!(
        "gh exited with code {}",
        output.status.code().unwrap_or_default()
    ))
}

fn detect_repo_slug(repo_root: &Path) -> Result<String, String> {
    crate::config::detect_repo_name(repo_root)
        .ok_or_else(|| "could not detect GitHub owner/repo from repo remote".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};

    use crate::runtime_state::RuntimeStateStore;
    use crate::tui::pty::{InteractiveBackend, PtySession};
    use tempfile::TempDir;

    fn key_event(code: KeyCode) -> AppEvent {
        AppEvent::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn key_event_with_mod(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
        AppEvent::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    // 1. New app starts on Issue Request tab
    #[test]
    fn new_app_starts_on_issue_request() {
        let app = App::new();
        assert_eq!(app.active_tab, Tab::IssueRequest);
        assert!(!app.should_quit);
    }

    // 2. Tab key cycles tabs forward
    #[test]
    fn tab_cycles_forward() {
        let mut app = App::new();
        app.handle_event(key_event(KeyCode::Tab));
        assert_eq!(app.active_tab, Tab::Monitoring);
        app.handle_event(key_event(KeyCode::Tab));
        assert_eq!(app.active_tab, Tab::IssueRequest);
    }

    // 3. BackTab cycles tabs backward
    #[test]
    fn backtab_cycles_backward() {
        let mut app = App::new();
        app.handle_event(key_event(KeyCode::BackTab));
        assert_eq!(app.active_tab, Tab::Monitoring);
        app.handle_event(key_event(KeyCode::BackTab));
        assert_eq!(app.active_tab, Tab::IssueRequest);
    }

    // 4. q quits the app
    #[test]
    fn q_quits() {
        let mut app = App::new();
        app.handle_event(key_event(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    // 5. Ctrl+C quits the app
    #[test]
    fn ctrl_c_quits() {
        let mut app = App::new();
        app.handle_event(key_event_with_mod(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ));
        assert!(app.should_quit);
    }

    // 6. j/k navigate issue list
    #[test]
    fn jk_navigate_issues() {
        let mut app = App::new();
        app.issue_requests = vec![
            IssueRequestItem {
                issue_number: 1,
                title: "A".into(),
                issue_type: "Feature".into(),
                vision_report_ready: true,
            },
            IssueRequestItem {
                issue_number: 2,
                title: "B".into(),
                issue_type: "Refactoring".into(),
                vision_report_ready: false,
            },
        ];

        assert_eq!(app.selected_issue_index, 0);
        app.handle_event(key_event(KeyCode::Char('j')));
        assert_eq!(app.selected_issue_index, 1);
        app.handle_event(key_event(KeyCode::Char('k')));
        assert_eq!(app.selected_issue_index, 0);
    }

    // 7. Enter queues interactive session launch for the main loop
    #[test]
    fn enter_queues_pending_session() {
        let mut app = App::new();
        app.issue_requests = vec![IssueRequestItem {
            issue_number: 1,
            title: "Test".into(),
            issue_type: "Feature".into(),
            vision_report_ready: true,
        }];

        app.handle_event(key_event(KeyCode::Enter));
        assert_eq!(app.take_pending_interactive_issue(), Some(1));
        assert!(!app.interactive_session_active);
        assert!(app.pty_session.is_none());
    }

    // 8. Esc exits interactive session
    #[test]
    fn esc_exits_session() {
        let mut app = App::new();
        app.issue_requests = vec![IssueRequestItem {
            issue_number: 1,
            title: "Test".into(),
            issue_type: "Feature".into(),
            vision_report_ready: true,
        }];
        app.interactive_session_active = true;

        app.handle_event(key_event(KeyCode::Esc));
        assert!(!app.interactive_session_active);
    }

    // 9. q does NOT quit during interactive session
    #[test]
    fn q_does_not_quit_in_session() {
        let mut app = App::new();
        app.interactive_session_active = true;
        app.handle_event(key_event(KeyCode::Char('q')));
        assert!(!app.should_quit);
    }

    // 10. Tab does NOT cycle during interactive session
    #[test]
    fn tab_does_not_cycle_in_session() {
        let mut app = App::new();
        app.interactive_session_active = true;
        app.handle_event(key_event(KeyCode::Tab));
        assert_eq!(app.active_tab, Tab::IssueRequest);
    }

    #[test]
    fn ctrl_a_in_session_queues_approval() {
        let mut app = App::new();
        app.issue_requests = vec![IssueRequestItem {
            issue_number: 7,
            title: "Approve me".into(),
            issue_type: "Feature".into(),
            vision_report_ready: true,
        }];
        app.interactive_session_active = true;

        app.handle_event(key_event_with_mod(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        ));

        assert_eq!(
            app.take_pending_issue_review(),
            Some(PendingIssueReview {
                issue_number: 7,
                decision: IssueReviewDecision::Approve,
            })
        );
    }

    #[test]
    fn start_interactive_session_uses_resolved_backend() {
        let temp = init_git_repo();
        let repo_root = temp.path();

        let mut app = App::new();
        let mut seen_backend = None;

        app.start_interactive_session_for_issue_with(
            42,
            repo_root,
            |backend, session, cwd, _, _| {
                seen_backend = Some((backend, session.to_string(), cwd.to_string()));
                Ok(PtySession::test_stub())
            },
        );

        assert_eq!(
            seen_backend,
            Some((
                InteractiveBackend::Codex,
                "githubclaw-issue-42".to_string(),
                repo_root.to_string_lossy().to_string(),
            ))
        );
        assert!(app.interactive_session_active);
        assert!(app.pty_session.is_some());
    }

    #[test]
    fn apply_issue_review_builds_approve_comment_command() {
        let temp = init_git_repo();
        let mut app = App::new();
        app.interactive_session_active = true;
        app.pty_session = Some(PtySession::test_stub());

        let mut captured_args = Vec::new();
        app.apply_issue_review_with(
            temp.path(),
            PendingIssueReview {
                issue_number: 12,
                decision: IssueReviewDecision::Approve,
            },
            |_, args| {
                captured_args = args.to_vec();
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            captured_args,
            vec![
                "issue".to_string(),
                "comment".to_string(),
                "12".to_string(),
                "--repo".to_string(),
                "octocat/Hello-World".to_string(),
                "--body".to_string(),
                "<!-- githubclaw:approved -->\nApproved via GithubClaw TUI interactive session."
                    .to_string(),
            ]
        );
        assert!(!app.interactive_session_active);
        assert!(app.pty_session.is_none());
    }

    #[test]
    fn apply_issue_review_builds_reject_close_command() {
        let temp = init_git_repo();
        let mut app = App::new();

        let mut captured_args = Vec::new();
        app.apply_issue_review_with(
            temp.path(),
            PendingIssueReview {
                issue_number: 19,
                decision: IssueReviewDecision::Reject,
            },
            |_, args| {
                captured_args = args.to_vec();
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            captured_args,
            vec![
                "issue".to_string(),
                "close".to_string(),
                "19".to_string(),
                "--repo".to_string(),
                "octocat/Hello-World".to_string(),
                "--reason".to_string(),
                "not planned".to_string(),
                "--comment".to_string(),
                "Rejected via GithubClaw TUI interactive session.".to_string(),
            ]
        );
    }

    // 11. Monitoring tab j/k navigation
    #[test]
    fn monitoring_jk_navigate() {
        let mut app = App::new();
        app.active_tab = Tab::Monitoring;
        app.agent_sessions = vec![
            AgentSessionItem {
                issue_number: 1,
                agent_type: "implementer".into(),
                status: AgentStatus::Running,
                started_at: "14:00".into(),
            },
            AgentSessionItem {
                issue_number: 2,
                agent_type: "verifier".into(),
                status: AgentStatus::Queued,
                started_at: "14:01".into(),
            },
        ];

        app.handle_event(key_event(KeyCode::Char('j')));
        assert_eq!(app.selected_agent_index, 1);
    }

    // 12. Tick event doesn't crash
    #[test]
    fn tick_event_noop() {
        let mut app = App::new();
        app.handle_event(AppEvent::Tick);
        assert!(!app.should_quit);
    }

    #[test]
    fn monitoring_history_samples_once_per_minute() {
        let mut app = App::new();
        app.queue_depth = 4;
        app.worker_count = (2, 8);
        app.agent_timeline.push(TimelineEntry {
            agent_type: "implementer".into(),
            status: AgentStatus::Running,
            detail: "Started work".into(),
        });

        app.capture_monitoring_sample_for_second(120);
        app.capture_monitoring_sample_for_second(179);

        assert_eq!(app.queue_history.len(), 1);
        assert_eq!(app.worker_history.len(), 1);
        assert_eq!(app.activity_history.len(), 1);
        assert_eq!(app.activity_history[0], 1);
    }

    #[test]
    fn monitoring_history_is_capped_to_last_thirty_minutes() {
        let mut app = App::new();

        for minute in 0..40 {
            app.queue_depth = minute as usize;
            app.capture_monitoring_sample_for_second(minute * 60);
        }

        assert_eq!(app.queue_history.len(), MONITOR_HISTORY_MINUTES);
        assert_eq!(app.queue_history.front(), Some(&10));
        assert_eq!(app.queue_history.back(), Some(&39));
    }

    #[test]
    fn recent_events_follow_latest_timeline_entries() {
        let mut app = App::new();
        app.agent_sessions.push(AgentSessionItem {
            issue_number: 42,
            agent_type: "implementer".into(),
            status: AgentStatus::Running,
            started_at: "14:00".into(),
        });
        for idx in 0..8 {
            app.agent_timeline.push(TimelineEntry {
                agent_type: "implementer".into(),
                status: AgentStatus::Running,
                detail: format!("Step {}", idx),
            });
        }

        app.refresh_recent_events();

        assert_eq!(app.recent_events.len(), 6);
        assert!(app.recent_events[0].contains("Step 7"));
        assert!(app.recent_events[5].contains("Step 2"));
    }

    #[test]
    fn refresh_from_disk_populates_runtime_views() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let store = RuntimeStateStore::with_base_dir(home.join("sessions"));
        store
            .record_vision_analysis(
                "owner/repo",
                42,
                Some("Dark mode"),
                "## Vision Alignment Analysis\n**Classification**: Feature",
            )
            .unwrap();
        store
            .record_agent_status(
                "owner/repo",
                42,
                None,
                "orchestrator",
                AgentStatus::Running,
                "Reviewing issue context",
            )
            .unwrap();

        let mut app = App::new();
        app.refresh_from_disk_root(home);

        assert_eq!(app.issue_requests.len(), 1);
        assert_eq!(app.issue_requests[0].issue_number, 42);
        assert_eq!(app.issue_requests[0].title, "Dark mode");
        assert_eq!(app.agent_sessions.len(), 2);
        assert!(app
            .agent_sessions
            .iter()
            .any(|session| session.agent_type == "orchestrator"));
        assert!(!app.agent_timeline.is_empty());
    }

    fn init_git_repo() -> TempDir {
        let temp = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "git@github.com:octocat/Hello-World.git",
            ])
            .current_dir(temp.path())
            .output()
            .unwrap();
        temp
    }
}

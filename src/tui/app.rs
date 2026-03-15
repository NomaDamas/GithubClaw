//! TUI application state and main loop.

use crossterm::event::{KeyCode, KeyModifiers};

use super::event::{is_quit, AppEvent};
use super::tabs::*;

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

    // Monitoring tab state
    pub agent_sessions: Vec<AgentSessionItem>,
    pub selected_agent_index: usize,
    pub agent_timeline: Vec<TimelineEntry>,
    pub rate_limit_tier: String,
    pub queue_depth: usize,
    pub worker_count: (usize, usize), // (active, max)

    // Release tab state
    pub release_info: Option<ReleaseInfo>,
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
            agent_sessions: Vec::new(),
            selected_agent_index: 0,
            agent_timeline: Vec::new(),
            rate_limit_tier: "None".into(),
            queue_depth: 0,
            worker_count: (0, 8),
            release_info: None,
        }
    }

    /// Refresh data from disk (called on Tick events).
    ///
    /// Reads queue depths, process states, and session info from the
    /// GithubClaw home directory. This is a lightweight polling approach
    /// that doesn't require IPC with the webhook server.
    pub fn refresh_from_disk(&mut self) {
        let home = crate::config::global_config_dir();

        // Read queue depths
        let queue_dir = home.join("queue");
        if queue_dir.exists() {
            self.queue_depth = std::fs::read_dir(&queue_dir)
                .map(|entries| entries.filter_map(|e| e.ok()).count())
                .unwrap_or(0);
        }

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
            Tab::Release => self.handle_release_key(code, modifiers),
        }
    }

    fn handle_issue_request_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        if self.interactive_session_active {
            // In interactive session, Escape exits back to list
            if code == KeyCode::Esc {
                self.interactive_session_active = false;
            }
            // Other keys would be forwarded to the PTY session
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
                if !self.issue_requests.is_empty() {
                    // Store the selected issue for the caller to spawn Claude Code
                    self.interactive_session_active = true;
                    self.pending_interactive_issue = Some(
                        self.issue_requests[self.selected_issue_index].issue_number,
                    );
                }
            }
            KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
                // Approve current issue
                // Would post approved marker via gh CLI
            }
            KeyCode::Char('r') if modifiers.contains(KeyModifiers::CONTROL) => {
                // Reject current issue
                // Would close issue via gh CLI
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
            KeyCode::Char('c') => {
                // Would open PR comment input
            }
            _ => {}
        }
    }

    fn handle_release_key(&mut self, code: KeyCode, _modifiers: KeyModifiers) {
        match code {
            KeyCode::Char('r') => {
                // Would run githubclaw release
            }
            KeyCode::Char('o') => {
                // Would open PR in browser
            }
            _ => {}
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};

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
        assert_eq!(app.active_tab, Tab::Release);
        app.handle_event(key_event(KeyCode::Tab));
        assert_eq!(app.active_tab, Tab::IssueRequest);
    }

    // 3. BackTab cycles tabs backward
    #[test]
    fn backtab_cycles_backward() {
        let mut app = App::new();
        app.handle_event(key_event(KeyCode::BackTab));
        assert_eq!(app.active_tab, Tab::Release);
        app.handle_event(key_event(KeyCode::BackTab));
        assert_eq!(app.active_tab, Tab::Monitoring);
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

    // 7. Enter activates interactive session
    #[test]
    fn enter_activates_session() {
        let mut app = App::new();
        app.issue_requests = vec![IssueRequestItem {
            issue_number: 1,
            title: "Test".into(),
            issue_type: "Feature".into(),
            vision_report_ready: true,
        }];

        app.handle_event(key_event(KeyCode::Enter));
        assert!(app.interactive_session_active);
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
}

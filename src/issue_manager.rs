use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::clone_manager::{CloneManager, DEFAULT_CLONE_POOL_SIZE};
use crate::config::global_config_dir;
use crate::config::issue_managers_dir_for_repo_from_home;
use crate::errors::Result;
use crate::runtime_state::{IssueManagerRuntimeUpdate, RuntimeStateStore};
use crate::tmux_manager::{TmuxManager, TmuxOps};
use crate::tui::tabs::AgentStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueManagerStatus {
    PendingApproval,
    Approved,
    Running,
    CleanupPending,
    Closed,
}

impl IssueManagerStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PendingApproval => "pending_approval",
            Self::Approved => "approved",
            Self::Running => "running",
            Self::CleanupPending => "cleanup_pending",
            Self::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueManagerState {
    pub repo: String,
    pub issue_number: u64,
    pub tmux_session_name: String,
    pub clone_id: String,
    pub clone_path: PathBuf,
    pub branch_name: String,
    pub launch_command: String,
    pub status: IssueManagerStatus,
    pub last_event_id: Option<String>,
    pub cleanup_note: Option<String>,
    pub created_at_unix_seconds: u64,
    pub updated_at_unix_seconds: u64,
}

pub struct IssueManagerStore {
    githubclaw_home: PathBuf,
}

impl IssueManagerStore {
    pub fn new() -> Self {
        Self {
            githubclaw_home: global_config_dir(),
        }
    }

    pub fn with_home(githubclaw_home: PathBuf) -> Self {
        Self { githubclaw_home }
    }

    pub fn load(&self, repo: &str, issue_number: u64) -> Result<Option<IssueManagerState>> {
        let path = self.state_path(repo, issue_number);
        if !path.exists() {
            return Ok(None);
        }
        let json = std::fs::read_to_string(path)?;
        Ok(Some(serde_json::from_str(&json)?))
    }

    pub fn save(&self, state: &IssueManagerState) -> Result<()> {
        let path = self.state_path(&state.repo, state.issue_number);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, serde_json::to_string_pretty(state)?)?;
        std::fs::rename(tmp_path, path)?;
        Ok(())
    }

    pub fn delete(&self, repo: &str, issue_number: u64) -> Result<()> {
        let dir = self.issue_dir(repo, issue_number);
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    pub fn state_path(&self, repo: &str, issue_number: u64) -> PathBuf {
        self.issue_dir(repo, issue_number).join("state.json")
    }

    pub fn inbox_dir(&self, repo: &str, issue_number: u64) -> PathBuf {
        self.issue_dir(repo, issue_number).join("inbox")
    }

    fn issue_dir(&self, repo: &str, issue_number: u64) -> PathBuf {
        issue_managers_dir_for_repo_from_home(&self.githubclaw_home, repo)
            .join(issue_number.to_string())
    }
}

impl Default for IssueManagerStore {
    fn default() -> Self {
        Self::new()
    }
}

pub struct IssueManagerController {
    githubclaw_home: PathBuf,
}

impl IssueManagerController {
    pub fn new() -> Self {
        Self {
            githubclaw_home: global_config_dir(),
        }
    }

    pub fn with_home(githubclaw_home: PathBuf) -> Self {
        Self { githubclaw_home }
    }

    pub fn approve_issue(
        &self,
        repo_root: &Path,
        repo: &str,
        issue_number: u64,
    ) -> Result<IssueManagerState> {
        let clone_manager = CloneManager::new(repo_root, repo, self.githubclaw_home.clone());
        clone_manager.bootstrap_pool(DEFAULT_CLONE_POOL_SIZE)?;
        let tmux = TmuxManager::new();
        self.approve_issue_with(repo, issue_number, &clone_manager, &tmux)
    }

    pub fn reject_issue(&self, repo_root: &Path, repo: &str, issue_number: u64) -> Result<()> {
        self.cleanup_issue(repo_root, repo, issue_number, "rejected")
    }

    pub fn cleanup_issue(
        &self,
        repo_root: &Path,
        repo: &str,
        issue_number: u64,
        reason: &str,
    ) -> Result<()> {
        let clone_manager = CloneManager::new(repo_root, repo, self.githubclaw_home.clone());
        let tmux = TmuxManager::new();
        self.cleanup_issue_with(repo, issue_number, reason, &clone_manager, &tmux)
    }

    pub fn ensure_issue_manager(
        &self,
        repo_root: &Path,
        repo: &str,
        issue_number: u64,
    ) -> Result<IssueManagerState> {
        self.approve_issue(repo_root, repo, issue_number)
    }

    pub fn resume_issue_manager(
        &self,
        repo_root: &Path,
        repo: &str,
        issue_number: u64,
    ) -> Result<IssueManagerState> {
        self.approve_issue(repo_root, repo, issue_number)
    }

    pub fn enqueue_issue_event(
        &self,
        repo: &str,
        issue_number: u64,
        event_id: &str,
        payload: &serde_json::Value,
    ) -> Result<PathBuf> {
        let store = IssueManagerStore::with_home(self.githubclaw_home.clone());
        let inbox_dir = store.inbox_dir(repo, issue_number);
        std::fs::create_dir_all(&inbox_dir)?;
        let path = inbox_dir.join(format!("{event_id}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(payload)?)?;
        Ok(path)
    }

    pub fn reconcile_issue_manager(
        &self,
        repo_root: &Path,
        repo: &str,
        issue_number: u64,
    ) -> Result<Option<IssueManagerState>> {
        let store = IssueManagerStore::with_home(self.githubclaw_home.clone());
        let Some(state) = store.load(repo, issue_number)? else {
            return Ok(None);
        };
        let tmux = TmuxManager::new();
        if tmux.session_exists(&state.tmux_session_name) {
            return Ok(Some(state));
        }
        self.approve_issue(repo_root, repo, issue_number).map(Some)
    }

    pub fn session_name(repo: &str, issue_number: u64) -> String {
        format!(
            "githubclaw-{}-issue-{}",
            crate::config::repo_key(repo),
            issue_number
        )
    }

    pub fn default_launch_command() -> String {
        std::env::var("GITHUBCLAW_ISSUE_MANAGER_COMMAND")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "omx --madmax".to_string())
    }

    fn approve_issue_with<T: TmuxOps>(
        &self,
        repo: &str,
        issue_number: u64,
        clone_manager: &CloneManager,
        tmux: &T,
    ) -> Result<IssueManagerState> {
        let store = IssueManagerStore::with_home(self.githubclaw_home.clone());
        if let Some(existing) = store.load(repo, issue_number)? {
            let clone_status = clone_manager
                .clone_for_issue(issue_number)?
                .map(|lease| lease.status.as_str().to_string())
                .ok_or_else(|| {
                    crate::errors::GithubClawError::Session(format!(
                        "missing clone lease for issue manager {}",
                        existing.tmux_session_name
                    ))
                })?;
            if tmux.session_exists(&existing.tmux_session_name) {
                self.record_runtime_state(&existing, &clone_status);
                self.record_agent_status(
                    repo,
                    issue_number,
                    AgentStatus::Running,
                    format!(
                        "Reused issue-manager session {}",
                        existing.tmux_session_name
                    ),
                );
                return Ok(existing);
            }

            tmux.start_issue_manager_session(
                &existing.tmux_session_name,
                &existing.clone_path,
                repo,
                issue_number,
                &existing.launch_command,
            )?;
            self.record_runtime_state(&existing, &clone_status);
            self.record_agent_status(
                repo,
                issue_number,
                AgentStatus::Running,
                format!(
                    "Restarted issue-manager session {}",
                    existing.tmux_session_name
                ),
            );
            return Ok(existing);
        }

        let clone = clone_manager.allocate_clone(issue_number)?;
        let session_name = Self::session_name(repo, issue_number);
        let launch_command = Self::default_launch_command();
        tmux.start_issue_manager_session(
            &session_name,
            &clone.clone_path,
            repo,
            issue_number,
            &launch_command,
        )?;

        let state = IssueManagerState {
            repo: repo.to_string(),
            issue_number,
            tmux_session_name: session_name,
            clone_id: clone.clone_id.clone(),
            clone_path: clone.clone_path.clone(),
            branch_name: clone
                .branch_name
                .clone()
                .unwrap_or_else(|| format!("githubclaw/issue-{issue_number}")),
            launch_command,
            status: IssueManagerStatus::Running,
            last_event_id: None,
            cleanup_note: None,
            created_at_unix_seconds: now(),
            updated_at_unix_seconds: now(),
        };
        store.save(&state)?;
        self.record_runtime_state(&state, clone.status.as_str());
        self.record_agent_status(
            repo,
            issue_number,
            AgentStatus::Running,
            format!(
                "Issue-manager session {} running in {}",
                state.tmux_session_name,
                state.clone_path.display()
            ),
        );
        Ok(state)
    }

    fn cleanup_issue_with<T: TmuxOps>(
        &self,
        repo: &str,
        issue_number: u64,
        reason: &str,
        clone_manager: &CloneManager,
        tmux: &T,
    ) -> Result<()> {
        let store = IssueManagerStore::with_home(self.githubclaw_home.clone());
        let Some(mut state) = store.load(repo, issue_number)? else {
            return Ok(());
        };

        tmux.kill_session(&state.tmux_session_name)?;
        match clone_manager.release_clone(issue_number) {
            Ok(()) => {
                store.delete(repo, issue_number)?;
                self.record_runtime_state(&state, "idle");
                self.record_agent_status(
                    repo,
                    issue_number,
                    AgentStatus::Completed,
                    format!("Issue-manager cleaned up ({reason})"),
                );
                Ok(())
            }
            Err(err) => {
                state.status = IssueManagerStatus::CleanupPending;
                state.cleanup_note = Some(err.to_string());
                state.updated_at_unix_seconds = now();
                store.save(&state)?;
                let clone_status = clone_manager
                    .clone_for_issue(issue_number)?
                    .map(|lease| lease.status.as_str().to_string())
                    .unwrap_or_else(|| "repair_needed".to_string());
                self.record_runtime_state(&state, &clone_status);
                self.record_agent_status(
                    repo,
                    issue_number,
                    AgentStatus::Failed,
                    format!("Issue-manager cleanup pending: {}", err),
                );
                Err(err)
            }
        }
    }

    fn record_runtime_state(&self, state: &IssueManagerState, clone_status: &str) {
        let runtime_store = RuntimeStateStore::with_base_dir(self.githubclaw_home.join("sessions"));
        let _ = runtime_store.record_issue_manager_state(
            &state.repo,
            state.issue_number,
            IssueManagerRuntimeUpdate {
                session_name: Some(&state.tmux_session_name),
                issue_manager_status: Some(state.status.as_str()),
                clone_id: Some(&state.clone_id),
                clone_path: Some(&state.clone_path),
                clone_status: Some(clone_status),
            },
        );
    }

    fn record_agent_status(
        &self,
        repo: &str,
        issue_number: u64,
        status: AgentStatus,
        detail: String,
    ) {
        let runtime_store = RuntimeStateStore::with_base_dir(self.githubclaw_home.join("sessions"));
        let _ = runtime_store.record_agent_status(
            repo,
            issue_number,
            None,
            "issue-manager",
            status,
            detail,
        );
    }
}

impl Default for IssueManagerController {
    fn default() -> Self {
        Self::new()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux_manager::TmuxOps;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    #[derive(Clone, Default)]
    struct FakeTmux {
        existing: Arc<Mutex<HashSet<String>>>,
        started: Arc<Mutex<Vec<String>>>,
        killed: Arc<Mutex<Vec<String>>>,
    }

    impl TmuxOps for FakeTmux {
        fn session_exists(&self, session_name: &str) -> bool {
            self.existing.lock().unwrap().contains(session_name)
        }

        fn start_issue_manager_session(
            &self,
            session_name: &str,
            _working_dir: &Path,
            _repo: &str,
            _issue_number: u64,
            _launch_command: &str,
        ) -> Result<()> {
            self.existing
                .lock()
                .unwrap()
                .insert(session_name.to_string());
            self.started.lock().unwrap().push(session_name.to_string());
            Ok(())
        }

        fn kill_session(&self, session_name: &str) -> Result<()> {
            self.existing.lock().unwrap().remove(session_name);
            self.killed.lock().unwrap().push(session_name.to_string());
            Ok(())
        }
    }

    fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> TempDir {
        let temp = TempDir::new().unwrap();
        run_git(temp.path(), ["init"]);
        run_git(temp.path(), ["config", "user.email", "bot@example.com"]);
        run_git(temp.path(), ["config", "user.name", "GithubClaw Bot"]);
        std::fs::write(temp.path().join("README.md"), "hello\n").unwrap();
        run_git(temp.path(), ["add", "README.md"]);
        run_git(temp.path(), ["commit", "-m", "init"]);
        temp
    }

    #[test]
    fn issue_manager_store_round_trips_persisted_state() {
        let temp = TempDir::new().unwrap();
        let store = IssueManagerStore::with_home(temp.path().to_path_buf());
        let state = IssueManagerState {
            repo: "owner/repo".into(),
            issue_number: 42,
            tmux_session_name: "githubclaw-owner_repo-issue-42".into(),
            clone_id: "clone-1".into(),
            clone_path: PathBuf::from("/tmp/clone-1"),
            branch_name: "githubclaw/issue-42".into(),
            launch_command: "omx --madmax".into(),
            status: IssueManagerStatus::Running,
            last_event_id: None,
            cleanup_note: None,
            created_at_unix_seconds: 1,
            updated_at_unix_seconds: 1,
        };
        store.save(&state).unwrap();
        let loaded = store.load("owner/repo", 42).unwrap().unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn approve_issue_creates_clone_lease_and_tmux_backed_state() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let controller = IssueManagerController::with_home(home.path().to_path_buf());
        let clone_manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());
        clone_manager
            .bootstrap_pool(DEFAULT_CLONE_POOL_SIZE)
            .unwrap();
        let tmux = FakeTmux::default();

        let state = controller
            .approve_issue_with("owner/repo", 42, &clone_manager, &tmux)
            .unwrap();

        assert_eq!(state.clone_id, "clone-1");
        assert_eq!(state.branch_name, "githubclaw/issue-42");
        assert_eq!(tmux.started.lock().unwrap().len(), 1);
        let persisted = IssueManagerStore::with_home(home.path().to_path_buf())
            .load("owner/repo", 42)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.tmux_session_name, state.tmux_session_name);
    }

    #[test]
    fn approve_issue_reuses_existing_running_tmux_session() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let controller = IssueManagerController::with_home(home.path().to_path_buf());
        let clone_manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());
        clone_manager
            .bootstrap_pool(DEFAULT_CLONE_POOL_SIZE)
            .unwrap();
        let tmux = FakeTmux::default();

        let first = controller
            .approve_issue_with("owner/repo", 7, &clone_manager, &tmux)
            .unwrap();
        tmux.existing
            .lock()
            .unwrap()
            .insert(first.tmux_session_name.clone());
        let second = controller
            .approve_issue_with("owner/repo", 7, &clone_manager, &tmux)
            .unwrap();

        assert_eq!(first.tmux_session_name, second.tmux_session_name);
        assert_eq!(tmux.started.lock().unwrap().len(), 1);
    }

    #[test]
    fn cleanup_issue_removes_persisted_state_for_clean_release() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let controller = IssueManagerController::with_home(home.path().to_path_buf());
        let clone_manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());
        clone_manager
            .bootstrap_pool(DEFAULT_CLONE_POOL_SIZE)
            .unwrap();
        let tmux = FakeTmux::default();

        controller
            .approve_issue_with("owner/repo", 11, &clone_manager, &tmux)
            .unwrap();
        controller
            .cleanup_issue_with("owner/repo", 11, "test", &clone_manager, &tmux)
            .unwrap();

        let persisted = IssueManagerStore::with_home(home.path().to_path_buf())
            .load("owner/repo", 11)
            .unwrap();
        assert!(persisted.is_none());
    }

    #[test]
    fn cleanup_issue_marks_cleanup_pending_when_clone_release_fails() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let controller = IssueManagerController::with_home(home.path().to_path_buf());
        let clone_manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());
        clone_manager
            .bootstrap_pool(DEFAULT_CLONE_POOL_SIZE)
            .unwrap();
        let tmux = FakeTmux::default();

        let state = controller
            .approve_issue_with("owner/repo", 19, &clone_manager, &tmux)
            .unwrap();
        std::fs::write(state.clone_path.join("dirty.txt"), "dirty\n").unwrap();

        let err = controller
            .cleanup_issue_with("owner/repo", 19, "test", &clone_manager, &tmux)
            .unwrap_err();
        assert!(err.to_string().contains("not clean"));
        let persisted = IssueManagerStore::with_home(home.path().to_path_buf())
            .load("owner/repo", 19)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.status, IssueManagerStatus::CleanupPending);
        assert!(persisted.cleanup_note.is_some());
    }

    #[test]
    fn reject_issue_delegates_to_cleanup_logic() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let controller = IssueManagerController::with_home(home.path().to_path_buf());
        let clone_manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());
        clone_manager
            .bootstrap_pool(DEFAULT_CLONE_POOL_SIZE)
            .unwrap();
        let tmux = FakeTmux::default();

        controller
            .approve_issue_with("owner/repo", 27, &clone_manager, &tmux)
            .unwrap();
        controller
            .cleanup_issue_with("owner/repo", 27, "rejected", &clone_manager, &tmux)
            .unwrap();
        assert_eq!(tmux.killed.lock().unwrap().len(), 1);
    }
}

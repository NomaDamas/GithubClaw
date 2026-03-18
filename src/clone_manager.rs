use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::clone_pool_path_for_repo_from_home;
use crate::config::repo_key;
use crate::errors::{GithubClawError, Result};

pub const DEFAULT_CLONE_POOL_SIZE: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneStatus {
    Idle,
    Assigned,
    Dirty,
    RepairNeeded,
}

impl CloneStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Assigned => "assigned",
            Self::Dirty => "dirty",
            Self::RepairNeeded => "repair_needed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneLease {
    pub clone_id: String,
    pub repo: String,
    pub clone_path: PathBuf,
    pub assigned_issue: Option<u64>,
    pub branch_name: Option<String>,
    pub status: CloneStatus,
    pub status_reason: Option<String>,
    pub last_synced_at_unix_seconds: Option<u64>,
    pub updated_at_unix_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClonePoolState {
    pub repo: String,
    pub pool_size: usize,
    pub clones: HashMap<String, CloneLease>,
    pub updated_at_unix_seconds: u64,
}

pub struct CloneManager {
    repo_root: PathBuf,
    repo: String,
    githubclaw_home: PathBuf,
}

impl CloneManager {
    pub fn new(repo_root: &Path, repo: &str, githubclaw_home: PathBuf) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            repo: repo.to_string(),
            githubclaw_home,
        }
    }

    pub fn bootstrap_pool(&self, pool_size: usize) -> Result<ClonePoolState> {
        let pool_size = if pool_size == 0 {
            DEFAULT_CLONE_POOL_SIZE
        } else {
            pool_size
        };

        let existing = self.load_pool_state().ok();
        let mut clones = HashMap::new();
        for idx in 1..=pool_size {
            let clone_id = format!("clone-{idx}");
            let mut lease = existing
                .as_ref()
                .and_then(|state| state.clones.get(&clone_id))
                .cloned()
                .unwrap_or(CloneLease {
                    clone_id: clone_id.clone(),
                    repo: self.repo.clone(),
                    clone_path: self.clone_path(&clone_id),
                    assigned_issue: None,
                    branch_name: None,
                    status: CloneStatus::Idle,
                    status_reason: None,
                    last_synced_at_unix_seconds: None,
                    updated_at_unix_seconds: now(),
                });
            lease.clone_path = self.ensure_clone_exists(&clone_id)?;
            lease.updated_at_unix_seconds = now();
            clones.insert(clone_id, lease);
        }

        let state = ClonePoolState {
            repo: self.repo.clone(),
            pool_size,
            clones,
            updated_at_unix_seconds: now(),
        };
        self.save_pool_state(&state)?;
        Ok(state)
    }

    pub fn load_pool_state(&self) -> Result<ClonePoolState> {
        let path = self.clone_pool_path();
        if !path.exists() {
            return Err(GithubClawError::Session(format!(
                "clone pool state missing at {}",
                path.display()
            )));
        }
        let json = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn save_pool_state(&self, state: &ClonePoolState) -> Result<()> {
        let path = self.clone_pool_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, serde_json::to_string_pretty(state)?)?;
        std::fs::rename(tmp_path, path)?;
        Ok(())
    }

    pub fn reconcile_pool(&self) -> Result<ClonePoolState> {
        let pool_size = self
            .load_pool_state()
            .map(|state| state.pool_size)
            .unwrap_or(DEFAULT_CLONE_POOL_SIZE);
        let mut state = self.bootstrap_pool(pool_size)?;
        for lease in state.clones.values_mut() {
            if !lease.clone_path.exists() {
                lease.status = CloneStatus::RepairNeeded;
                lease.status_reason = Some("clone path missing".into());
                lease.updated_at_unix_seconds = now();
                continue;
            }
            if !matches!(lease.status, CloneStatus::Assigned) {
                lease.status = self.clone_health(&lease.clone_path)?;
                lease.updated_at_unix_seconds = now();
            }
        }
        state.updated_at_unix_seconds = now();
        self.save_pool_state(&state)?;
        Ok(state)
    }

    pub fn sync_idle_clones(&self) -> Result<ClonePoolState> {
        let pool_size = self
            .load_pool_state()
            .map(|state| state.pool_size)
            .unwrap_or(DEFAULT_CLONE_POOL_SIZE);
        let mut state = self.bootstrap_pool(pool_size)?;
        let base = self.base_branch()?;
        for lease in state.clones.values_mut() {
            if lease.assigned_issue.is_some() || !matches!(lease.status, CloneStatus::Idle) {
                continue;
            }
            self.run_git_in(&lease.clone_path, ["fetch", "--all", "--prune"])?;
            self.run_git_in(&lease.clone_path, ["checkout", &base])?;
            self.run_git_in(
                &lease.clone_path,
                ["reset", "--hard", &format!("origin/{base}")],
            )?;
            lease.last_synced_at_unix_seconds = Some(now());
            lease.updated_at_unix_seconds = now();
        }
        state.updated_at_unix_seconds = now();
        self.save_pool_state(&state)?;
        Ok(state)
    }

    pub fn allocate_clone(&self, issue_number: u64) -> Result<CloneLease> {
        let pool_size = self
            .load_pool_state()
            .map(|state| state.pool_size)
            .unwrap_or(DEFAULT_CLONE_POOL_SIZE);
        let mut state = self.bootstrap_pool(pool_size)?;

        if let Some(existing) = state
            .clones
            .values()
            .find(|lease| lease.assigned_issue == Some(issue_number))
            .cloned()
        {
            return Ok(existing);
        }

        let mut clone_ids = state.clones.keys().cloned().collect::<Vec<_>>();
        clone_ids.sort();
        let clone_id = clone_ids
            .into_iter()
            .find(|clone_id| {
                state.clones.get(clone_id).is_some_and(|lease| {
                    lease.assigned_issue.is_none() && matches!(lease.status, CloneStatus::Idle)
                })
            })
            .ok_or_else(|| GithubClawError::Session("no idle clone available".into()))?;

        let branch_name = self.ensure_issue_branch(&clone_id, issue_number)?;
        let lease = state.clones.get_mut(&clone_id).expect("clone id exists");
        lease.assigned_issue = Some(issue_number);
        lease.branch_name = Some(branch_name);
        lease.status = CloneStatus::Assigned;
        lease.status_reason = None;
        lease.updated_at_unix_seconds = now();

        let out = lease.clone();
        state.updated_at_unix_seconds = now();
        self.save_pool_state(&state)?;
        Ok(out)
    }

    pub fn release_clone(&self, issue_number: u64) -> Result<()> {
        let mut state = self.load_pool_state()?;
        let clone_id = state
            .clones
            .iter()
            .find(|(_, lease)| lease.assigned_issue == Some(issue_number))
            .map(|(clone_id, _)| clone_id.clone())
            .ok_or_else(|| {
                GithubClawError::Session(format!("no clone assigned to issue #{issue_number}"))
            })?;

        let lease = state.clones.get_mut(&clone_id).expect("clone id exists");
        if !self.is_clean(&lease.clone_path)? {
            let err = GithubClawError::Session(format!(
                "clone {} is not clean on release",
                lease.clone_id
            ));
            Self::mark_repair_needed_in_state(&mut state, &clone_id, err.to_string());
            self.save_pool_state(&state)?;
            return Err(err);
        }

        let base = self.base_branch()?;
        let current_branch = self.current_branch(&lease.clone_path)?;
        if current_branch.as_deref() != lease.branch_name.as_deref() {
            let err = GithubClawError::Session(format!(
                "clone {} is on unexpected branch {:?}",
                lease.clone_id, current_branch
            ));
            Self::mark_repair_needed_in_state(&mut state, &clone_id, err.to_string());
            self.save_pool_state(&state)?;
            return Err(err);
        }

        if self.branch_diverges_from_base(&lease.clone_path, &base)? {
            let err = GithubClawError::Session(format!(
                "clone {} branch diverged from base {}",
                lease.clone_id, base
            ));
            Self::mark_repair_needed_in_state(&mut state, &clone_id, err.to_string());
            self.save_pool_state(&state)?;
            return Err(err);
        }

        if let Err(err) = self.run_git_in(&lease.clone_path, ["checkout", &base]) {
            Self::mark_repair_needed_in_state(&mut state, &clone_id, err.to_string());
            self.save_pool_state(&state)?;
            return Err(err);
        }
        if let Err(err) = self.run_git_in(
            &lease.clone_path,
            ["reset", "--hard", &format!("origin/{base}")],
        ) {
            Self::mark_repair_needed_in_state(&mut state, &clone_id, err.to_string());
            self.save_pool_state(&state)?;
            return Err(err);
        }

        lease.assigned_issue = None;
        lease.branch_name = None;
        lease.status = CloneStatus::Idle;
        lease.status_reason = None;
        lease.last_synced_at_unix_seconds = Some(now());
        lease.updated_at_unix_seconds = now();

        state.updated_at_unix_seconds = now();
        self.save_pool_state(&state)?;
        Ok(())
    }

    pub fn mark_dirty(&self, clone_id: &str, reason: impl Into<String>) -> Result<()> {
        self.update_clone(clone_id, |lease| {
            lease.status = CloneStatus::Dirty;
            lease.status_reason = Some(reason.into());
            lease.updated_at_unix_seconds = now();
        })
    }

    pub fn mark_repair_needed(&self, clone_id: &str, reason: impl Into<String>) -> Result<()> {
        self.update_clone(clone_id, |lease| {
            lease.status = CloneStatus::RepairNeeded;
            lease.status_reason = Some(reason.into());
            lease.updated_at_unix_seconds = now();
        })
    }

    pub fn clone_for_issue(&self, issue_number: u64) -> Result<Option<CloneLease>> {
        let state = self.load_pool_state()?;
        Ok(state
            .clones
            .values()
            .find(|lease| lease.assigned_issue == Some(issue_number))
            .cloned())
    }

    pub fn idle_clones(&self) -> Result<Vec<CloneLease>> {
        let state = self.load_pool_state()?;
        Ok(state
            .clones
            .values()
            .filter(|lease| {
                lease.assigned_issue.is_none() && matches!(lease.status, CloneStatus::Idle)
            })
            .cloned()
            .collect())
    }

    pub fn ensure_issue_branch(&self, clone_id: &str, issue_number: u64) -> Result<String> {
        let clone_path = self.clone_path(clone_id);
        let base = self.base_branch()?;
        let branch_name = format!("githubclaw/issue-{issue_number}");

        self.run_git_in(&clone_path, ["fetch", "--all", "--prune"])?;
        self.run_git_in(&clone_path, ["checkout", &base])?;
        self.run_git_in(&clone_path, ["reset", "--hard", &format!("origin/{base}")])?;

        let local_branch_ref = format!("refs/heads/{branch_name}");
        if self.branch_exists_in(&clone_path, &local_branch_ref)? {
            self.run_git_in(&clone_path, ["checkout", &branch_name])?;
        } else {
            self.run_git_in(&clone_path, ["checkout", "-b", &branch_name, &base])?;
        }

        Ok(branch_name)
    }

    pub fn reset_clone_to_base(&self, clone_id: &str) -> Result<()> {
        let clone_path = self.clone_path(clone_id);
        let base = self.base_branch()?;
        self.run_git_in(&clone_path, ["fetch", "--all", "--prune"])?;
        self.run_git_in(&clone_path, ["checkout", &base])?;
        self.run_git_in(&clone_path, ["reset", "--hard", &format!("origin/{base}")])?;
        Ok(())
    }

    pub fn clone_pool_path(&self) -> PathBuf {
        clone_pool_path_for_repo_from_home(&self.githubclaw_home, &self.repo)
    }

    pub fn clone_root_dir(&self) -> PathBuf {
        self.githubclaw_home
            .join("clones")
            .join(repo_key(&self.repo))
    }

    fn update_clone<F>(&self, clone_id: &str, update: F) -> Result<()>
    where
        F: FnOnce(&mut CloneLease),
    {
        let mut state = self.load_pool_state()?;
        let lease = state
            .clones
            .get_mut(clone_id)
            .ok_or_else(|| GithubClawError::Session(format!("unknown clone id {clone_id}")))?;
        update(lease);
        state.updated_at_unix_seconds = now();
        self.save_pool_state(&state)
    }

    fn mark_repair_needed_in_state(state: &mut ClonePoolState, clone_id: &str, reason: String) {
        if let Some(lease) = state.clones.get_mut(clone_id) {
            lease.status = CloneStatus::RepairNeeded;
            lease.status_reason = Some(reason);
            lease.updated_at_unix_seconds = now();
        }
        state.updated_at_unix_seconds = now();
    }

    fn ensure_clone_exists(&self, clone_id: &str) -> Result<PathBuf> {
        let clone_path = self.clone_path(clone_id);
        if clone_path.join(".git").exists() {
            return Ok(clone_path);
        }
        if clone_path.exists() {
            return Err(GithubClawError::Session(format!(
                "clone path {} exists but is not a git clone",
                clone_path.display()
            )));
        }
        if let Some(parent) = clone_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let output = Command::new("git")
            .args([
                "clone",
                self.repo_root.to_string_lossy().as_ref(),
                clone_path.to_string_lossy().as_ref(),
            ])
            .output()?;
        if !output.status.success() {
            return Err(GithubClawError::Session(stderr_or_fallback(
                "git clone",
                &output,
            )));
        }
        Ok(clone_path)
    }

    fn clone_path(&self, clone_id: &str) -> PathBuf {
        self.clone_root_dir().join(clone_id)
    }

    fn base_branch(&self) -> Result<String> {
        for candidate in ["dev", "main", "master"] {
            let branch_ref = format!("refs/heads/{candidate}");
            if self.branch_exists_in(&self.repo_root, &branch_ref)? {
                return Ok(candidate.to_string());
            }
        }
        Err(GithubClawError::Session(
            "could not determine base branch (expected dev/main/master)".into(),
        ))
    }

    fn clone_health(&self, clone_path: &Path) -> Result<CloneStatus> {
        if !clone_path.exists() {
            return Ok(CloneStatus::RepairNeeded);
        }
        if self.is_clean(clone_path)? {
            Ok(CloneStatus::Idle)
        } else {
            Ok(CloneStatus::Dirty)
        }
    }

    fn is_clean(&self, clone_path: &Path) -> Result<bool> {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(clone_path)
            .output()?;
        if !output.status.success() {
            return Err(GithubClawError::Session(stderr_or_fallback(
                "git status",
                &output,
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().is_empty())
    }

    fn branch_exists_in(&self, dir: &Path, reference: &str) -> Result<bool> {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", reference])
            .current_dir(dir)
            .output()?;
        Ok(output.status.success())
    }

    fn current_branch(&self, dir: &Path) -> Result<Option<String>> {
        let output = Command::new("git")
            .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
            .current_dir(dir)
            .output()?;
        if output.status.success() {
            return Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            ));
        }
        Ok(None)
    }

    fn branch_diverges_from_base(&self, dir: &Path, base: &str) -> Result<bool> {
        let output = Command::new("git")
            .args([
                "rev-list",
                "--left-right",
                "--count",
                &format!("{base}...HEAD"),
            ])
            .current_dir(dir)
            .output()?;
        if !output.status.success() {
            return Err(GithubClawError::Session(stderr_or_fallback(
                "git rev-list",
                &output,
            )));
        }

        let counts = String::from_utf8_lossy(&output.stdout);
        let mut parts = counts.split_whitespace();
        let behind = parts
            .next()
            .and_then(|part| part.parse::<u64>().ok())
            .unwrap_or_default();
        let ahead = parts
            .next()
            .and_then(|part| part.parse::<u64>().ok())
            .unwrap_or_default();
        Ok(behind > 0 || ahead > 0)
    }

    fn run_git_in<const N: usize>(&self, dir: &Path, args: [&str; N]) -> Result<()> {
        let output = Command::new("git").args(args).current_dir(dir).output()?;
        if output.status.success() {
            return Ok(());
        }
        Err(GithubClawError::Session(stderr_or_fallback("git", &output)))
    }
}

fn stderr_or_fallback(command: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return format!("{command} failed: {stderr}");
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return format!("{command} failed: {stdout}");
    }
    format!(
        "{command} failed with exit code {}",
        output.status.code().unwrap_or_default()
    )
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
    use tempfile::TempDir;

    fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) {
        let output = Command::new("git")
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
    fn bootstrap_pool_creates_expected_clone_slots() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());

        let state = manager.bootstrap_pool(4).unwrap();
        assert_eq!(state.clones.len(), 4);
        assert!(state.clones.contains_key("clone-1"));
        assert!(state.clones.contains_key("clone-4"));
    }

    #[test]
    fn bootstrap_pool_is_idempotent() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());

        let first = manager.bootstrap_pool(4).unwrap();
        let second = manager.bootstrap_pool(4).unwrap();
        assert_eq!(first.clones.len(), second.clones.len());
        assert_eq!(first.pool_size, second.pool_size);
    }

    #[test]
    fn allocate_clone_assigns_idle_clone() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());

        manager.bootstrap_pool(2).unwrap();
        let lease = manager.allocate_clone(42).unwrap();
        assert_eq!(lease.assigned_issue, Some(42));
        assert!(matches!(lease.status, CloneStatus::Assigned));
        assert_eq!(lease.branch_name.as_deref(), Some("githubclaw/issue-42"));
    }

    #[test]
    fn allocate_clone_reuses_the_same_lease_for_same_issue() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());

        manager.bootstrap_pool(2).unwrap();
        let first = manager.allocate_clone(7).unwrap();
        let second = manager.allocate_clone(7).unwrap();
        assert_eq!(first.clone_id, second.clone_id);
    }

    #[test]
    fn allocate_clone_rejects_when_only_dirty_clones_remain() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());

        manager.bootstrap_pool(1).unwrap();
        manager.mark_dirty("clone-1", "manual edits").unwrap();
        let err = manager.allocate_clone(5).unwrap_err();
        assert!(err.to_string().contains("no idle clone available"));
    }

    #[test]
    fn release_clone_returns_clean_clone_to_idle() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());

        manager.bootstrap_pool(1).unwrap();
        manager.allocate_clone(9).unwrap();
        manager.release_clone(9).unwrap();

        let lease = manager.idle_clones().unwrap().pop().unwrap();
        assert!(matches!(lease.status, CloneStatus::Idle));
        assert!(lease.assigned_issue.is_none());
        assert!(lease.branch_name.is_none());
    }

    #[test]
    fn release_clone_marks_unclean_clone_as_repair_needed() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());

        manager.bootstrap_pool(1).unwrap();
        let lease = manager.allocate_clone(13).unwrap();
        std::fs::write(lease.clone_path.join("dirty.txt"), "dirty\n").unwrap();

        let err = manager.release_clone(13).unwrap_err();
        assert!(err.to_string().contains("not clean"));
        let lease = manager.clone_for_issue(13).unwrap().unwrap();
        assert!(matches!(lease.status, CloneStatus::RepairNeeded));
    }

    #[test]
    fn release_clone_marks_diverged_branch_as_repair_needed() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());

        manager.bootstrap_pool(1).unwrap();
        let lease = manager.allocate_clone(14).unwrap();
        std::fs::write(lease.clone_path.join("README.md"), "changed\n").unwrap();
        run_git(&lease.clone_path, ["add", "README.md"]);
        run_git(&lease.clone_path, ["commit", "-m", "issue work"]);

        let err = manager.release_clone(14).unwrap_err();
        assert!(err.to_string().contains("diverged from base"));
        let lease = manager.clone_for_issue(14).unwrap().unwrap();
        assert!(matches!(lease.status, CloneStatus::RepairNeeded));
    }

    #[test]
    fn ensure_issue_branch_creates_expected_branch_name() {
        let repo = init_repo();
        let home = TempDir::new().unwrap();
        let manager = CloneManager::new(repo.path(), "owner/repo", home.path().to_path_buf());

        manager.bootstrap_pool(1).unwrap();
        let branch = manager.ensure_issue_branch("clone-1", 22).unwrap();
        assert_eq!(branch, "githubclaw/issue-22");
    }
}

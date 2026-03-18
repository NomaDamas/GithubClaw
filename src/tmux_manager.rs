//! Tmux session naming and execution helpers for issue managers.

use std::path::Path;
use std::process::Command;

use crate::config::repo_key;
use crate::errors::{GithubClawError, Result};

pub trait TmuxOps {
    fn session_exists(&self, session_name: &str) -> bool;
    fn start_issue_manager_session(
        &self,
        session_name: &str,
        working_dir: &Path,
        repo: &str,
        issue_number: u64,
        launch_command: &str,
    ) -> Result<()>;
    fn kill_session(&self, session_name: &str) -> Result<()>;
}

pub struct TmuxManager;

impl TmuxManager {
    pub fn new() -> Self {
        Self
    }

    pub fn repo_orchestrator_session_name(repo: &str) -> String {
        format!("githubclaw-repo-{}", repo_key(repo).replace('_', "-"))
    }

    pub fn issue_manager_session_name(repo: &str, issue_number: u64) -> String {
        format!("githubclaw-{}-issue-{}", repo_key(repo), issue_number)
    }

    pub fn build_issue_manager_shell_command(
        repo: &str,
        issue_number: u64,
        launch_command: &str,
    ) -> String {
        let inner = format!("exec {launch_command}");
        format!(
            "env GITHUBCLAW_REPO={} GITHUBCLAW_ROOT_ISSUE={} sh -lc {}",
            shell_single_quote(repo),
            shell_single_quote(&issue_number.to_string()),
            shell_single_quote(&inner),
        )
    }

    pub fn new_issue_manager_session_args(
        session_name: &str,
        working_dir: &Path,
        repo: &str,
        issue_number: u64,
        launch_command: &str,
    ) -> Vec<String> {
        vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            session_name.into(),
            "-c".into(),
            working_dir.display().to_string(),
            Self::build_issue_manager_shell_command(repo, issue_number, launch_command),
        ]
    }

    pub fn kill_session_args(session_name: &str) -> Vec<String> {
        vec!["kill-session".into(), "-t".into(), session_name.into()]
    }
}

impl Default for TmuxManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TmuxOps for TmuxManager {
    fn session_exists(&self, session_name: &str) -> bool {
        Command::new("tmux")
            .args(["has-session", "-t", session_name])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn start_issue_manager_session(
        &self,
        session_name: &str,
        working_dir: &Path,
        repo: &str,
        issue_number: u64,
        launch_command: &str,
    ) -> Result<()> {
        let status = Command::new("tmux")
            .args(Self::new_issue_manager_session_args(
                session_name,
                working_dir,
                repo,
                issue_number,
                launch_command,
            ))
            .status()
            .map_err(|err| GithubClawError::Orchestrator(format!("failed to run tmux: {err}")))?;

        if status.success() {
            Ok(())
        } else {
            Err(GithubClawError::Orchestrator(format!(
                "tmux new-session failed for {session_name}"
            )))
        }
    }

    fn kill_session(&self, session_name: &str) -> Result<()> {
        let output = Command::new("tmux")
            .args(Self::kill_session_args(session_name))
            .output()
            .map_err(|err| GithubClawError::Orchestrator(format!("failed to run tmux: {err}")))?;

        let missing_session =
            String::from_utf8_lossy(&output.stderr).contains("can't find session");
        if output.status.success() || missing_session {
            Ok(())
        } else {
            Err(GithubClawError::Orchestrator(format!(
                "tmux kill-session failed for {session_name}"
            )))
        }
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_session_name_is_repo_scoped() {
        assert_eq!(
            TmuxManager::repo_orchestrator_session_name("owner/repo"),
            "githubclaw-repo-owner-repo"
        );
    }

    #[test]
    fn issue_session_name_is_repo_and_issue_scoped() {
        assert_eq!(
            TmuxManager::issue_manager_session_name("owner/repo", 42),
            "githubclaw-owner_repo-issue-42"
        );
    }

    #[test]
    fn new_issue_manager_session_args_wrap_launch_command_in_shell() {
        let args = TmuxManager::new_issue_manager_session_args(
            "githubclaw-owner_repo-issue-42",
            Path::new("/tmp/repo"),
            "owner/repo",
            42,
            "omx --madmax",
        );

        assert_eq!(
            args,
            vec![
                "new-session",
                "-d",
                "-s",
                "githubclaw-owner_repo-issue-42",
                "-c",
                "/tmp/repo",
                "env GITHUBCLAW_REPO='owner/repo' GITHUBCLAW_ROOT_ISSUE='42' sh -lc 'exec omx --madmax'"
            ]
        );
    }
}

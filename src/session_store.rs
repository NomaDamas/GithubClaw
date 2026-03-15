//! Session ID persistence for Claude Code `--resume` pattern.
//!
//! Each Orchestrator session is tied to a root issue and persisted at:
//! `~/.githubclaw/sessions/<repo>/<issue-id>/session_id`
//!
//! This enables the fire-and-forget dispatch pattern where the Orchestrator
//! exits after dispatching agents and resumes when a marker webhook arrives.

use std::path::PathBuf;

use crate::config::global_config_dir;
use crate::errors::Result;

// ---------------------------------------------------------------------------
// SessionStore
// ---------------------------------------------------------------------------

/// Manages Claude Code session IDs on disk.
///
/// Uses a configurable base directory for testability.
pub struct SessionStore {
    base_dir: PathBuf,
}

impl SessionStore {
    /// Create a store using the default global config directory.
    pub fn new() -> Self {
        Self {
            base_dir: global_config_dir().join("sessions"),
        }
    }

    /// Create a store rooted at a custom directory (for testing).
    pub fn with_base_dir(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    fn session_dir(&self, repo: &str, issue_id: u64) -> PathBuf {
        let safe_repo = repo.replace('/', "_");
        self.base_dir.join(safe_repo).join(issue_id.to_string())
    }

    fn session_id_path(&self, repo: &str, issue_id: u64) -> PathBuf {
        self.session_dir(repo, issue_id).join("session_id")
    }

    /// Store a Claude Code session ID for a given repo + issue.
    pub fn save(&self, repo: &str, issue_id: u64, session_id: &str) -> Result<()> {
        let path = self.session_id_path(repo, issue_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, session_id)?;
        tracing::debug!(
            repo = repo,
            issue_id = issue_id,
            session_id = session_id,
            path = %path.display(),
            "saved session ID"
        );
        Ok(())
    }

    /// Load a Claude Code session ID for a given repo + issue.
    ///
    /// Returns `None` if no session exists.
    pub fn load(&self, repo: &str, issue_id: u64) -> Result<Option<String>> {
        let path = self.session_id_path(repo, issue_id);
        if !path.exists() {
            return Ok(None);
        }
        let id = std::fs::read_to_string(&path)?.trim().to_string();
        if id.is_empty() {
            return Ok(None);
        }
        Ok(Some(id))
    }

    /// Delete a session ID (e.g., when an issue is closed/completed).
    pub fn delete(&self, repo: &str, issue_id: u64) -> Result<()> {
        let dir = self.session_dir(repo, issue_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
            tracing::debug!(repo = repo, issue_id = issue_id, "deleted session");
        }
        Ok(())
    }

    /// List all active session IDs for a given repo.
    ///
    /// Returns a vec of (issue_id, session_id) pairs sorted by issue_id.
    pub fn list(&self, repo: &str) -> Result<Vec<(u64, String)>> {
        let safe_repo = repo.replace('/', "_");
        let repo_dir = self.base_dir.join(safe_repo);

        if !repo_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in std::fs::read_dir(&repo_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let issue_id: u64 = match entry.file_name().to_string_lossy().parse() {
                    Ok(id) => id,
                    Err(_) => continue,
                };
                let sid_path = entry.path().join("session_id");
                if sid_path.exists() {
                    let sid = std::fs::read_to_string(&sid_path)?.trim().to_string();
                    if !sid.is_empty() {
                        sessions.push((issue_id, sid));
                    }
                }
            }
        }

        sessions.sort_by_key(|(id, _)| *id);
        Ok(sessions)
    }

    /// Check if a session exists for a given repo + issue.
    pub fn has_session(&self, repo: &str, issue_id: u64) -> bool {
        self.session_id_path(repo, issue_id).exists()
    }

    /// Get the session directory path for external use.
    pub fn get_session_dir(&self, repo: &str, issue_id: u64) -> PathBuf {
        self.session_dir(repo, issue_id)
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store(tmp: &TempDir) -> SessionStore {
        SessionStore::with_base_dir(tmp.path().join("sessions"))
    }

    // 1. Save and load session ID
    #[test]
    fn save_and_load_session_id() {
        let tmp = TempDir::new().unwrap();
        let store = test_store(&tmp);
        store.save("owner/repo", 42, "session-abc-123").unwrap();
        let loaded = store.load("owner/repo", 42).unwrap();
        assert_eq!(loaded, Some("session-abc-123".to_string()));
    }

    // 2. Load nonexistent session returns None
    #[test]
    fn load_nonexistent_returns_none() {
        let tmp = TempDir::new().unwrap();
        let store = test_store(&tmp);
        let loaded = store.load("owner/repo", 999).unwrap();
        assert_eq!(loaded, None);
    }

    // 3. Delete session
    #[test]
    fn delete_session_removes_dir() {
        let tmp = TempDir::new().unwrap();
        let store = test_store(&tmp);
        store.save("owner/repo", 42, "session-xyz").unwrap();
        assert!(store.has_session("owner/repo", 42));

        store.delete("owner/repo", 42).unwrap();
        assert!(!store.has_session("owner/repo", 42));
        assert_eq!(store.load("owner/repo", 42).unwrap(), None);
    }

    // 4. Delete nonexistent session is no-op
    #[test]
    fn delete_nonexistent_is_noop() {
        let tmp = TempDir::new().unwrap();
        let store = test_store(&tmp);
        store.delete("owner/repo", 999).unwrap();
    }

    // 5. List sessions for repo
    #[test]
    fn list_sessions_for_repo() {
        let tmp = TempDir::new().unwrap();
        let store = test_store(&tmp);
        store.save("owner/repo", 10, "sid-10").unwrap();
        store.save("owner/repo", 20, "sid-20").unwrap();
        store.save("owner/repo", 5, "sid-5").unwrap();

        let sessions = store.list("owner/repo").unwrap();
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0], (5, "sid-5".to_string()));
        assert_eq!(sessions[1], (10, "sid-10".to_string()));
        assert_eq!(sessions[2], (20, "sid-20".to_string()));
    }

    // 6. List sessions for empty repo
    #[test]
    fn list_sessions_empty_repo() {
        let tmp = TempDir::new().unwrap();
        let store = test_store(&tmp);
        let sessions = store.list("nobody/nothing").unwrap();
        assert!(sessions.is_empty());
    }

    // 7. has_session returns correct values
    #[test]
    fn has_session_check() {
        let tmp = TempDir::new().unwrap();
        let store = test_store(&tmp);
        assert!(!store.has_session("owner/repo", 42));
        store.save("owner/repo", 42, "sid").unwrap();
        assert!(store.has_session("owner/repo", 42));
    }

    // 8. Overwrite session ID
    #[test]
    fn overwrite_session_id() {
        let tmp = TempDir::new().unwrap();
        let store = test_store(&tmp);
        store.save("owner/repo", 42, "old-session").unwrap();
        store.save("owner/repo", 42, "new-session").unwrap();
        let loaded = store.load("owner/repo", 42).unwrap();
        assert_eq!(loaded, Some("new-session".to_string()));
    }

    // 9. Repo with slash is sanitized
    #[test]
    fn repo_slash_sanitized() {
        let tmp = TempDir::new().unwrap();
        let store = test_store(&tmp);
        store.save("org/my-repo", 1, "sid").unwrap();
        let dir = store.get_session_dir("org/my-repo", 1);
        assert!(dir.to_string_lossy().contains("org_my-repo"));
        assert!(!dir.to_string_lossy().contains("org/my-repo"));
    }

    // 10. Multiple repos are isolated
    #[test]
    fn multiple_repos_isolated() {
        let tmp = TempDir::new().unwrap();
        let store = test_store(&tmp);
        store.save("org/repo-a", 1, "sid-a").unwrap();
        store.save("org/repo-b", 1, "sid-b").unwrap();

        assert_eq!(
            store.load("org/repo-a", 1).unwrap(),
            Some("sid-a".to_string())
        );
        assert_eq!(
            store.load("org/repo-b", 1).unwrap(),
            Some("sid-b".to_string())
        );
    }
}

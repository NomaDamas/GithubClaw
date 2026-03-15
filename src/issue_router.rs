//! Root issue routing for GitHub events.
//!
//! Maps GitHub events (issues, PRs, comments, CI checks) back to their
//! root issue for correct Orchestrator session routing.
//!
//! Three routing strategies:
//! 1. Direct: `issues.*` events use `issue.number` directly
//! 2. Ref parsing: Comment/PR bodies contain `ref #N` (injected by gh wrapper)
//! 3. PR map: `check_run` events use PR number → root issue lookup

use std::collections::HashMap;
use std::path::PathBuf;

use crate::errors::Result;
use crate::markers;

// ---------------------------------------------------------------------------
// PR → Root Issue map (persistent)
// ---------------------------------------------------------------------------

/// Maps PR numbers to root issue numbers.
///
/// Persisted at `~/.githubclaw/sessions/<repo>/pr_map.json`.
pub struct IssueRouter {
    base_dir: PathBuf,
}

impl IssueRouter {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    fn pr_map_path(&self, repo: &str) -> PathBuf {
        let safe_repo = repo.replace('/', "_");
        self.base_dir.join(safe_repo).join("pr_map.json")
    }

    fn load_pr_map(&self, repo: &str) -> Result<HashMap<u64, u64>> {
        let path = self.pr_map_path(repo);
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let contents = std::fs::read_to_string(&path)?;
        let map: HashMap<String, u64> = serde_json::from_str(&contents)?;
        // Convert string keys to u64
        Ok(map
            .into_iter()
            .filter_map(|(k, v)| k.parse::<u64>().ok().map(|pk| (pk, v)))
            .collect())
    }

    fn save_pr_map(&self, repo: &str, map: &HashMap<u64, u64>) -> Result<()> {
        let path = self.pr_map_path(repo);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Convert u64 keys to strings for JSON
        let string_map: HashMap<String, u64> =
            map.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let json = serde_json::to_string_pretty(&string_map)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Register a PR → root issue mapping.
    ///
    /// Called when a PR is created by an agent (detected via `ref #N` in PR body).
    pub fn register_pr(&self, repo: &str, pr_number: u64, root_issue: u64) -> Result<()> {
        let mut map = self.load_pr_map(repo)?;
        map.insert(pr_number, root_issue);
        self.save_pr_map(repo, &map)?;
        tracing::debug!(
            repo = repo,
            pr = pr_number,
            root_issue = root_issue,
            "registered PR → root issue mapping"
        );
        Ok(())
    }

    /// Look up root issue for a PR number.
    pub fn lookup_pr(&self, repo: &str, pr_number: u64) -> Result<Option<u64>> {
        let map = self.load_pr_map(repo)?;
        Ok(map.get(&pr_number).copied())
    }

    /// Remove a PR mapping (e.g., when PR is merged/closed).
    pub fn unregister_pr(&self, repo: &str, pr_number: u64) -> Result<()> {
        let mut map = self.load_pr_map(repo)?;
        map.remove(&pr_number);
        self.save_pr_map(repo, &map)?;
        Ok(())
    }

    /// Route a GitHub webhook event to its root issue.
    ///
    /// Returns `None` if the event cannot be routed.
    pub fn route_event(&self, repo: &str, event: &serde_json::Value) -> Result<Option<u64>> {
        let event_type = event
            .get("_githubclaw_event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match event_type {
            // Direct: issues.* → issue.number is the candidate
            "issues" => {
                let issue_number = event
                    .pointer("/issue/number")
                    .and_then(|v| v.as_u64());

                if let Some(num) = issue_number {
                    // Check if this issue's body has a ref #N (sub-issue)
                    let body = event
                        .pointer("/issue/body")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(root) = markers::extract_ref_issue(body) {
                        // This is a sub-issue — route to the root
                        return Ok(Some(root));
                    }
                    // This IS the root issue
                    return Ok(Some(num));
                }
                Ok(None)
            }

            // issue_comment.* → issue.number + check ref in body
            "issue_comment" => {
                let issue_number = event
                    .pointer("/issue/number")
                    .and_then(|v| v.as_u64());

                // First try ref #N in comment body (agent-generated comments)
                let comment_body = event
                    .pointer("/comment/body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(root) = markers::extract_ref_issue(comment_body) {
                    return Ok(Some(root));
                }

                // Fall back: check if the issue itself is a sub-issue
                let issue_body = event
                    .pointer("/issue/body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(root) = markers::extract_ref_issue(issue_body) {
                    return Ok(Some(root));
                }

                // Fall back: the issue itself is the root
                Ok(issue_number)
            }

            // pull_request.* → ref #N in PR body + register mapping
            "pull_request" => {
                let pr_number = event
                    .pointer("/pull_request/number")
                    .and_then(|v| v.as_u64());

                let pr_body = event
                    .pointer("/pull_request/body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if let Some(root) = markers::extract_ref_issue(pr_body) {
                    // Register PR mapping for future check_run lookups
                    if let Some(pr) = pr_number {
                        let _ = self.register_pr(repo, pr, root);
                    }
                    return Ok(Some(root));
                }

                // Try existing PR map
                if let Some(pr) = pr_number {
                    return self.lookup_pr(repo, pr);
                }
                Ok(None)
            }

            // pull_request_review.* → PR number → lookup
            "pull_request_review" => {
                // Try ref in review body first
                let review_body = event
                    .pointer("/review/body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(root) = markers::extract_ref_issue(review_body) {
                    return Ok(Some(root));
                }

                // Fall back to PR map
                let pr_number = event
                    .pointer("/pull_request/number")
                    .and_then(|v| v.as_u64());
                if let Some(pr) = pr_number {
                    return self.lookup_pr(repo, pr);
                }
                Ok(None)
            }

            // check_run.* → PR number(s) → lookup
            "check_run" => {
                // check_run has pull_requests array
                let prs = event
                    .pointer("/check_run/pull_requests")
                    .and_then(|v| v.as_array());

                if let Some(pr_list) = prs {
                    for pr in pr_list {
                        if let Some(pr_number) = pr.get("number").and_then(|v| v.as_u64()) {
                            if let Ok(Some(root)) = self.lookup_pr(repo, pr_number) {
                                return Ok(Some(root));
                            }
                        }
                    }
                }
                Ok(None)
            }

            _ => Ok(None),
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn test_router(tmp: &TempDir) -> IssueRouter {
        IssueRouter::new(tmp.path().join("sessions"))
    }

    // 1. Route issues.opened → direct issue number (root issue)
    #[test]
    fn route_issue_opened_root() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);
        let event = json!({
            "_githubclaw_event_type": "issues",
            "action": "opened",
            "issue": { "number": 42, "body": "Some bug report" }
        });
        let root = router.route_event("org/repo", &event).unwrap();
        assert_eq!(root, Some(42));
    }

    // 2. Route issues.opened with ref #N → sub-issue routed to root
    #[test]
    fn route_sub_issue_to_root() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);
        let event = json!({
            "_githubclaw_event_type": "issues",
            "action": "opened",
            "issue": { "number": 100, "body": "Sub-task: implement auth\n\n_ref #42_" }
        });
        let root = router.route_event("org/repo", &event).unwrap();
        assert_eq!(root, Some(42));
    }

    // 3. Route issue_comment with ref #N in comment body
    #[test]
    fn route_issue_comment_ref_in_body() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);
        let event = json!({
            "_githubclaw_event_type": "issue_comment",
            "action": "created",
            "issue": { "number": 100, "body": "" },
            "comment": { "body": "<!-- githubclaw:verified -->\n\nref #42" }
        });
        let root = router.route_event("org/repo", &event).unwrap();
        assert_eq!(root, Some(42));
    }

    // 4. Route issue_comment fallback to issue body ref
    #[test]
    fn route_issue_comment_fallback_issue_body() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);
        let event = json!({
            "_githubclaw_event_type": "issue_comment",
            "action": "created",
            "issue": { "number": 100, "body": "Sub-issue\n_ref #42_" },
            "comment": { "body": "LGTM" }
        });
        let root = router.route_event("org/repo", &event).unwrap();
        assert_eq!(root, Some(42));
    }

    // 5. Route issue_comment fallback to issue number
    #[test]
    fn route_issue_comment_fallback_issue_number() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);
        let event = json!({
            "_githubclaw_event_type": "issue_comment",
            "action": "created",
            "issue": { "number": 42, "body": "Root issue" },
            "comment": { "body": "Some comment" }
        });
        let root = router.route_event("org/repo", &event).unwrap();
        assert_eq!(root, Some(42));
    }

    // 6. Route pull_request with ref #N + auto-register PR map
    #[test]
    fn route_pr_with_ref_and_register() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);
        let event = json!({
            "_githubclaw_event_type": "pull_request",
            "action": "opened",
            "pull_request": { "number": 50, "body": "Fixes auth module\n\n_ref #42_" }
        });
        let root = router.route_event("org/repo", &event).unwrap();
        assert_eq!(root, Some(42));

        // Verify PR mapping was registered
        let mapped = router.lookup_pr("org/repo", 50).unwrap();
        assert_eq!(mapped, Some(42));
    }

    // 7. Route check_run via PR map
    #[test]
    fn route_check_run_via_pr_map() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);

        // Pre-register PR mapping
        router.register_pr("org/repo", 50, 42).unwrap();

        let event = json!({
            "_githubclaw_event_type": "check_run",
            "action": "completed",
            "check_run": {
                "conclusion": "failure",
                "pull_requests": [{ "number": 50 }]
            }
        });
        let root = router.route_event("org/repo", &event).unwrap();
        assert_eq!(root, Some(42));
    }

    // 8. Route check_run with no PR map → None
    #[test]
    fn route_check_run_no_map_returns_none() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);
        let event = json!({
            "_githubclaw_event_type": "check_run",
            "action": "completed",
            "check_run": {
                "conclusion": "failure",
                "pull_requests": [{ "number": 99 }]
            }
        });
        let root = router.route_event("org/repo", &event).unwrap();
        assert_eq!(root, None);
    }

    // 9. Route pull_request_review via review body ref
    #[test]
    fn route_pr_review_ref_in_body() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);
        let event = json!({
            "_githubclaw_event_type": "pull_request_review",
            "action": "submitted",
            "review": { "body": "LGTM\n\nref #42" },
            "pull_request": { "number": 50 }
        });
        let root = router.route_event("org/repo", &event).unwrap();
        assert_eq!(root, Some(42));
    }

    // 10. Route pull_request_review fallback to PR map
    #[test]
    fn route_pr_review_fallback_pr_map() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);
        router.register_pr("org/repo", 50, 42).unwrap();

        let event = json!({
            "_githubclaw_event_type": "pull_request_review",
            "action": "submitted",
            "review": { "body": "" },
            "pull_request": { "number": 50 }
        });
        let root = router.route_event("org/repo", &event).unwrap();
        assert_eq!(root, Some(42));
    }

    // 11. Unknown event type → None
    #[test]
    fn route_unknown_event_none() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);
        let event = json!({
            "_githubclaw_event_type": "ping",
            "zen": "hello"
        });
        let root = router.route_event("org/repo", &event).unwrap();
        assert_eq!(root, None);
    }

    // 12. PR map persistence roundtrip
    #[test]
    fn pr_map_persistence() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);

        router.register_pr("org/repo", 10, 100).unwrap();
        router.register_pr("org/repo", 20, 200).unwrap();

        // New router instance reads from disk
        let router2 = test_router(&tmp);
        assert_eq!(router2.lookup_pr("org/repo", 10).unwrap(), Some(100));
        assert_eq!(router2.lookup_pr("org/repo", 20).unwrap(), Some(200));
    }

    // 13. Unregister PR
    #[test]
    fn unregister_pr_removes_mapping() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);

        router.register_pr("org/repo", 50, 42).unwrap();
        assert_eq!(router.lookup_pr("org/repo", 50).unwrap(), Some(42));

        router.unregister_pr("org/repo", 50).unwrap();
        assert_eq!(router.lookup_pr("org/repo", 50).unwrap(), None);
    }

    // 14. Multiple repos are isolated
    #[test]
    fn pr_map_repo_isolation() {
        let tmp = TempDir::new().unwrap();
        let router = test_router(&tmp);

        router.register_pr("org/repo-a", 50, 1).unwrap();
        router.register_pr("org/repo-b", 50, 2).unwrap();

        assert_eq!(router.lookup_pr("org/repo-a", 50).unwrap(), Some(1));
        assert_eq!(router.lookup_pr("org/repo-b", 50).unwrap(), Some(2));
    }
}

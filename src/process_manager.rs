//! Flat process tree manager for orchestrators and worker agents.
//!
//! All child processes (orchestrators and workers) are managed as siblings of the
//! webhook server process. The manager handles spawning, monitoring exit codes,
//! idle timeouts, crash detection, concurrency throttling, and the fork PR gate.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::constants::MONITOR_CHECK_INTERVAL_SECONDS;

/// The kind of child process being managed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessKind {
    Orchestrator,
    Worker,
}

impl std::fmt::Display for ProcessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessKind::Orchestrator => write!(f, "orchestrator"),
            ProcessKind::Worker => write!(f, "worker"),
        }
    }
}

/// The lifecycle state of a managed process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Running,
    Finished,
    Crashed,
    TimedOut,
    Killed,
}

/// Bookkeeping record for a child process.
pub struct ManagedProcess {
    pub pid: u32,
    pub kind: ProcessKind,
    pub repo: String,
    pub label: String,
    pub started_at: Instant,
    pub timeout_seconds: u64,
    pub state: ProcessState,
    pub exit_code: Option<i32>,
}

/// Read-only agent types exempt from the fork PR gate.
const READ_ONLY_AGENT_TYPES: &[&str] = &["security_reviewer"];

/// Return `true` if the dispatch is allowed, `false` if it should be blocked.
///
/// Enforcement rules (from SECURITY.md):
/// - Not a fork PR -> allow
/// - Fork PR with `githubclaw-approved` label -> allow
/// - Read-only agents (e.g. `security_reviewer`) -> always allow
/// - Otherwise -> block
pub fn check_fork_pr_gate(event_payload: &serde_json::Value, agent_type: &str) -> bool {
    let pr = match event_payload.get("pull_request") {
        Some(pr) if !pr.is_null() => pr,
        _ => return true, // Not a PR event at all
    };

    let is_fork = pr
        .get("head")
        .and_then(|h| h.get("repo"))
        .and_then(|r| r.get("fork"))
        .and_then(|f| f.as_bool())
        .unwrap_or(false);

    if !is_fork {
        return true; // Not from a fork
    }

    // Check for the approved label
    if let Some(labels) = pr.get("labels").and_then(|l| l.as_array()) {
        for label in labels {
            if let Some(name) = label.get("name").and_then(|n| n.as_str()) {
                if name == "githubclaw-approved" {
                    return true;
                }
            }
        }
    }

    // Read-only agents are always allowed on fork PRs
    READ_ONLY_AGENT_TYPES.contains(&agent_type)
}

/// Manages all child processes as a flat sibling tree.
///
/// Provides spawn, monitor, idle-timeout, concurrency throttle, graceful
/// drain, and force kill capabilities.
pub struct ProcessManager {
    pub max_concurrent_agents: usize,
    processes: Arc<Mutex<HashMap<u32, ManagedProcess>>>,
}

impl ProcessManager {
    /// Create a new `ProcessManager` with the given concurrency limit.
    pub fn new(max_concurrent_agents: usize) -> Self {
        Self {
            max_concurrent_agents,
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Number of processes currently in the `Running` state.
    pub async fn active_count(&self) -> usize {
        let procs = self.processes.lock().await;
        procs
            .values()
            .filter(|p| p.state == ProcessState::Running)
            .count()
    }

    /// Whether there is at least one free concurrency slot.
    pub async fn has_capacity(&self) -> bool {
        self.active_count().await < self.max_concurrent_agents
    }

    /// Return a snapshot of all process PIDs and their states.
    pub async fn all_processes(&self) -> Vec<(u32, ProcessState)> {
        let procs = self.processes.lock().await;
        procs.iter().map(|(&pid, p)| (pid, p.state)).collect()
    }

    /// Register a spawned process for tracking.
    pub async fn register(
        &self,
        pid: u32,
        kind: ProcessKind,
        repo: &str,
        label: &str,
        timeout_seconds: u64,
    ) {
        let managed = ManagedProcess {
            pid,
            kind,
            repo: repo.to_string(),
            label: label.to_string(),
            started_at: Instant::now(),
            timeout_seconds,
            state: ProcessState::Running,
            exit_code: None,
        };
        self.processes.lock().await.insert(pid, managed);
    }

    /// Report that a process has exited with the given exit code.
    pub async fn report_exit(&self, pid: u32, exit_code: i32) {
        if let Some(proc) = self.processes.lock().await.get_mut(&pid) {
            proc.exit_code = Some(exit_code);
            proc.state = if exit_code == 0 {
                ProcessState::Finished
            } else {
                ProcessState::Crashed
            };
        }
    }

    /// Start a background monitor that periodically checks for timed-out processes.
    pub fn start_monitor(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let pm = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(MONITOR_CHECK_INTERVAL_SECONDS)).await;
                let mut procs = pm.processes.lock().await;
                let now = Instant::now();
                for proc in procs.values_mut() {
                    if proc.state == ProcessState::Running {
                        let elapsed = now.duration_since(proc.started_at).as_secs();
                        if elapsed > proc.timeout_seconds {
                            proc.state = ProcessState::TimedOut;
                            tracing::warn!(
                                "Process [{}] pid={} timed out after {}s",
                                proc.label,
                                proc.pid,
                                elapsed
                            );
                        }
                    }
                }
            }
        })
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::DEFAULT_MAX_CONCURRENT_AGENTS;
    use serde_json::json;

    // -- check_fork_pr_gate tests --

    #[test]
    fn fork_gate_not_a_pr_event_returns_true() {
        let payload = json!({"action": "opened", "issue": {"number": 1}});
        assert!(check_fork_pr_gate(&payload, "coder"));
    }

    #[test]
    fn fork_gate_non_fork_pr_returns_true() {
        let payload = json!({
            "pull_request": {
                "head": {"repo": {"fork": false}},
                "labels": []
            }
        });
        assert!(check_fork_pr_gate(&payload, "coder"));
    }

    #[test]
    fn fork_gate_fork_pr_with_approved_label_returns_true() {
        let payload = json!({
            "pull_request": {
                "head": {"repo": {"fork": true}},
                "labels": [{"name": "githubclaw-approved"}]
            }
        });
        assert!(check_fork_pr_gate(&payload, "coder"));
    }

    #[test]
    fn fork_gate_fork_pr_without_label_blocks() {
        let payload = json!({
            "pull_request": {
                "head": {"repo": {"fork": true}},
                "labels": []
            }
        });
        assert!(!check_fork_pr_gate(&payload, "coder"));
    }

    #[test]
    fn fork_gate_fork_pr_security_reviewer_always_allowed() {
        let payload = json!({
            "pull_request": {
                "head": {"repo": {"fork": true}},
                "labels": []
            }
        });
        assert!(check_fork_pr_gate(&payload, "security_reviewer"));
    }

    #[test]
    fn fork_gate_fork_pr_coder_blocked() {
        let payload = json!({
            "pull_request": {
                "head": {"repo": {"fork": true}},
                "labels": [{"name": "some-other-label"}]
            }
        });
        assert!(!check_fork_pr_gate(&payload, "coder"));
    }

    // -- ProcessManager tests --

    #[test]
    fn process_manager_new_with_capacity() {
        let pm = ProcessManager::new(4);
        assert_eq!(pm.max_concurrent_agents, 4);
    }

    #[tokio::test]
    async fn active_count_starts_at_zero() {
        let pm = ProcessManager::new(DEFAULT_MAX_CONCURRENT_AGENTS);
        assert_eq!(pm.active_count().await, 0);
    }

    #[tokio::test]
    async fn has_capacity_returns_true_when_empty() {
        let pm = ProcessManager::new(DEFAULT_MAX_CONCURRENT_AGENTS);
        assert!(pm.has_capacity().await);
    }

    #[tokio::test]
    async fn register_adds_process_to_tracking() {
        let pm = ProcessManager::new(4);
        assert_eq!(pm.active_count().await, 0);

        pm.register(1001, ProcessKind::Worker, "owner/repo", "test-worker", 3600)
            .await;
        assert_eq!(pm.active_count().await, 1);

        let procs = pm.all_processes().await;
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0], (1001, ProcessState::Running));
    }

    #[tokio::test]
    async fn report_exit_updates_state_success() {
        let pm = ProcessManager::new(4);
        pm.register(2001, ProcessKind::Orchestrator, "owner/repo", "orch", 3600)
            .await;

        pm.report_exit(2001, 0).await;
        let procs = pm.all_processes().await;
        assert_eq!(procs[0].1, ProcessState::Finished);
        // No longer counted as active
        assert_eq!(pm.active_count().await, 0);
    }

    #[tokio::test]
    async fn report_exit_updates_state_crash() {
        let pm = ProcessManager::new(4);
        pm.register(3001, ProcessKind::Worker, "owner/repo", "worker", 3600)
            .await;

        pm.report_exit(3001, 1).await;
        let procs = pm.all_processes().await;
        assert_eq!(procs[0].1, ProcessState::Crashed);
    }

    #[tokio::test]
    async fn has_capacity_respects_registered_processes() {
        let pm = ProcessManager::new(2);
        assert!(pm.has_capacity().await);

        pm.register(100, ProcessKind::Worker, "r", "w1", 3600).await;
        assert!(pm.has_capacity().await); // 1 of 2

        pm.register(101, ProcessKind::Worker, "r", "w2", 3600).await;
        assert!(!pm.has_capacity().await); // 2 of 2 — full

        // Finishing one frees capacity
        pm.report_exit(100, 0).await;
        assert!(pm.has_capacity().await); // 1 of 2 again
    }
}

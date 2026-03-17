//! Webhook server — axum HTTP entry point.
//!
//! Receives GitHub webhook events, verifies HMAC signatures, applies the fork
//! PR gate, and routes events into per-repo disk-persisted queues.
//!
//! Translated from the Python `server.py`.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
    Router,
};
use serde_json::Value;
use std::collections::{hash_map::Entry, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, warn};

use crate::agents::parser::load_agent_definition;
use crate::agents::prompt_assembler::PromptAssembler;
use crate::agents::spawner::AgentSpawner;
use crate::process_manager::{check_fork_pr_gate, ProcessManager};
use crate::queue::DiskPersistedQueue;
use crate::runtime_state::IssueRuntimeSnapshot;
use crate::scheduler::ScheduledEventManager;
use crate::signature::verify_webhook_signature;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// All shared state for the running webhook server.
pub struct ServerState {
    pub webhook_secret: String,
    pub registry: RwLock<HashMap<String, RegistryEntry>>,
    pub started_repos: RwLock<HashSet<String>>,
    pub queues: Mutex<HashMap<String, DiskPersistedQueue>>,
    pub githubclaw_home: PathBuf,
    pub process_manager: Arc<ProcessManager>,
    pub scheduler: Mutex<ScheduledEventManager>,
    pub rate_limiter: Arc<crate::rate_limiter::RateLimiter>,
    pub shutdown: Arc<std::sync::atomic::AtomicBool>,
    // Orchestrators run as subprocesses; session state lives on disk.
    /// Routes GitHub events to their root issue for orchestrator session routing.
    pub issue_router: crate::issue_router::IssueRouter,
    /// Persists Claude Code session IDs for the `--resume` flow.
    pub session_store: crate::session_store::SessionStore,
}

/// A single entry in `registry.json`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RegistryEntry {
    pub local_path: String,
    #[serde(default)]
    pub socket_path: String,
}

/// Top-level shape of `registry.json` when it uses the nested `{ "repos": { ... } }` format.
#[derive(Debug, serde::Deserialize)]
pub struct RegistryFile {
    #[serde(default)]
    pub repos: HashMap<String, RegistryEntry>,
}

// ---------------------------------------------------------------------------
// Configuration helpers
// ---------------------------------------------------------------------------

/// Read the webhook secret from a file and trim whitespace.
pub fn load_webhook_secret(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| {
            format!(
                "Failed to read webhook secret from {}: {}",
                path.display(),
                e
            )
        })
}

/// Load the repo registry mapping `full_name -> RegistryEntry`.
///
/// Supports two formats:
/// - Nested: `{ "repos": { "owner/repo": { "local_path": "..." } } }`
/// - Flat: `{ "owner/repo": { "local_path": "..." } }`
pub fn load_registry(path: &Path) -> HashMap<String, RegistryEntry> {
    if !path.exists() {
        warn!(
            "Registry file not found at {} -- no repos registered",
            path.display()
        );
        return HashMap::new();
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to read registry file {}: {}", path.display(), e);
            return HashMap::new();
        }
    };

    // Try nested format first: { "repos": { ... } }
    if let Ok(nested) = serde_json::from_str::<RegistryFile>(&content) {
        if !nested.repos.is_empty() {
            return nested.repos;
        }
    }

    // Try flat format: { "owner/repo": { ... } }
    match serde_json::from_str::<HashMap<String, RegistryEntry>>(&content) {
        Ok(flat) => flat,
        Err(e) => {
            warn!("Failed to parse registry.json: {}", e);
            HashMap::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Fork PR gate
// ---------------------------------------------------------------------------

/// Add a `_githubclaw_fork_unapproved` flag to the payload when the event is
/// a fork PR without the `githubclaw-approved` label.
fn annotate_fork_status(mut payload: Value) -> Value {
    let is_fork = payload
        .pointer("/pull_request/head/repo/fork")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if !is_fork {
        return payload;
    }

    let has_approved_label = payload
        .pointer("/pull_request/labels")
        .and_then(Value::as_array)
        .map(|labels| {
            labels
                .iter()
                .any(|l| l.get("name").and_then(Value::as_str) == Some("githubclaw-approved"))
        })
        .unwrap_or(false);

    if !has_approved_label {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("_githubclaw_fork_unapproved".to_string(), Value::Bool(true));
        }
        let pr_number = payload
            .pointer("/pull_request/number")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string());
        let fork_name = payload
            .pointer("/pull_request/head/repo/full_name")
            .and_then(Value::as_str)
            .unwrap_or("?");
        info!(
            "Fork PR #{} from {} queued as unapproved",
            pr_number, fork_name
        );
    }

    payload
}

// ---------------------------------------------------------------------------
// Queue helper
// ---------------------------------------------------------------------------

/// Return (or create) the disk-persisted queue for a repo.
///
/// Must be called with the queues mutex already locked.
pub(crate) fn get_or_create_queue<'a>(
    queues: &'a mut HashMap<String, DiskPersistedQueue>,
    githubclaw_home: &Path,
    repo_full_name: &str,
) -> std::io::Result<&'a mut DiskPersistedQueue> {
    let key = repo_full_name.to_string();
    match queues.entry(key) {
        Entry::Occupied(entry) => Ok(entry.into_mut()),
        Entry::Vacant(entry) => {
            let queue_dir =
                crate::config::queue_dir_for_repo_from_home(githubclaw_home, repo_full_name);
            let queue =
                DiskPersistedQueue::new(&queue_dir, crate::constants::DEFAULT_QUEUE_MAX_RETRY)?;
            Ok(entry.insert(queue))
        }
    }
}

async fn registry_snapshot(state: &Arc<ServerState>) -> HashMap<String, RegistryEntry> {
    state.registry.read().await.clone()
}

pub async fn start_repo_processing(
    state: Arc<ServerState>,
    repo_name: &str,
    entry: &RegistryEntry,
) {
    {
        let mut started = state.started_repos.write().await;
        if !started.insert(repo_name.to_string()) {
            return;
        }
    }

    let state_clone = Arc::clone(&state);
    let repo = repo_name.to_string();
    let entry_clone = entry.clone();
    tokio::spawn(async move {
        event_drain_loop(&state_clone, &repo, &entry_clone).await;
    });
}

async fn ensure_repo_registered(
    state: &Arc<ServerState>,
    repo_full_name: &str,
) -> Option<RegistryEntry> {
    if let Some(entry) = state.registry.read().await.get(repo_full_name).cloned() {
        return Some(entry);
    }

    let registry_path = state.githubclaw_home.join("registry.json");
    let latest = load_registry(&registry_path);
    let entry = latest.get(repo_full_name).cloned()?;

    {
        let mut registry = state.registry.write().await;
        *registry = latest;
    }

    if let Err(e) = bootstrap_repo(state, repo_full_name, &entry, false).await {
        warn!(
            "Bootstrap failed for {} after registry refresh: {}",
            repo_full_name, e
        );
    }
    start_repo_processing(Arc::clone(state), repo_full_name, &entry).await;
    info!(
        "Hot-registered repo {} from refreshed registry",
        repo_full_name
    );

    Some(entry)
}

fn is_process_alive(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }

    match std::io::Error::last_os_error().raw_os_error() {
        Some(code) if code == libc::EPERM => true,
        Some(code) if code == libc::ESRCH => false,
        _ => false,
    }
}

fn runtime_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn load_or_new_runtime_snapshot(
    state: &ServerState,
    repo_name: &str,
    issue_id: u64,
) -> IssueRuntimeSnapshot {
    state
        .session_store
        .load_runtime_snapshot(repo_name, issue_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| IssueRuntimeSnapshot::new(repo_name, issue_id))
}

fn save_runtime_snapshot(state: &ServerState, repo_name: &str, snapshot: &IssueRuntimeSnapshot) {
    if let Err(err) = state
        .session_store
        .save_runtime_snapshot(repo_name, snapshot)
    {
        warn!(
            repo = repo_name,
            issue = snapshot.issue_number,
            error = %err,
            "Failed to persist runtime snapshot",
        );
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn webhook_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<(StatusCode, String), (StatusCode, String)> {
    // 1. Signature verification
    let signature = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if signature.is_empty() {
        return Err((
            StatusCode::FORBIDDEN,
            "Missing X-Hub-Signature-256 header".to_string(),
        ));
    }

    if !verify_webhook_signature(&body, signature, &state.webhook_secret) {
        warn!("Invalid webhook signature");
        return Err((StatusCode::FORBIDDEN, "Invalid signature".to_string()));
    }

    // 2. Parse JSON payload
    let payload: Value = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid JSON payload: {}", e),
        )
    })?;

    // 3. Registry check
    let repo_full_name = payload
        .pointer("/repository/full_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if repo_full_name.is_empty() {
        debug!("Discarding event for unregistered repo: {}", repo_full_name);
        return Ok((StatusCode::OK, "Ignored: repo not registered".to_string()));
    }

    if ensure_repo_registered(&state, &repo_full_name)
        .await
        .is_none()
    {
        debug!(
            "Discarding event for unregistered repo after refresh: {}",
            repo_full_name
        );
        return Ok((StatusCode::OK, "Ignored: repo not registered".to_string()));
    }

    // 4. Fork PR annotation
    let mut payload = annotate_fork_status(payload);

    // 5. Build event label
    let event_type = headers
        .get("X-Github-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    let delivery_id = headers
        .get("X-GitHub-Delivery")
        .or_else(|| headers.get("X-Github-Delivery"))
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let action = payload.get("action").and_then(Value::as_str).unwrap_or("");

    let event_label = if action.is_empty() {
        event_type.to_string()
    } else {
        format!("{}_{}", event_type, action)
    };

    // Before enqueue, tag the payload with the event type
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "_githubclaw_event_type".to_string(),
            serde_json::Value::String(event_type.to_string()),
        );
        obj.insert(
            "_githubclaw_event_id".to_string(),
            serde_json::Value::String(delivery_id.clone()),
        );
    }

    // 6. Enqueue
    let mut queues = state.queues.lock().await;
    let queue =
        get_or_create_queue(&mut queues, &state.githubclaw_home, &repo_full_name).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Queue error: {}", e),
            )
        })?;

    queue
        .enqueue_with_id(payload, &event_label, Some(delivery_id))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Enqueue error: {}", e),
            )
        })?;

    let queue_size = queue.size();
    info!(
        "Queued event {} for {} (queue_size={})",
        event_label, repo_full_name, queue_size
    );

    Ok((StatusCode::ACCEPTED, "Event queued".to_string()))
}

async fn health_handler(State(state): State<Arc<ServerState>>) -> Json<Value> {
    Json(serde_json::json!({
        "status": "ok",
        "registered_repos": state.registry.read().await.len(),
    }))
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

/// Bootstrap a repo by scanning existing open issues and PRs.
///
/// When `force` is false, skips if the queue already has events.
pub async fn bootstrap_repo(
    state: &Arc<ServerState>,
    repo_name: &str,
    _entry: &RegistryEntry,
    force: bool,
) -> Result<(), String> {
    // Check if queue already has events
    if !force {
        let mut queues = state.queues.lock().await;
        let queue = get_or_create_queue(&mut queues, &state.githubclaw_home, repo_name)
            .map_err(|e| format!("Queue creation error: {}", e))?;

        if queue.size() > 0 {
            info!(
                "Skipping bootstrap for {} (queue already has {} events)",
                repo_name,
                queue.size()
            );
            return Ok(());
        }
    }

    // Scan existing open issues
    let issue_output = tokio::process::Command::new("gh")
        .args([
            "issue",
            "list",
            "--state",
            "open",
            "--json",
            "number,title,labels",
            "--limit",
            "100",
            "--repo",
            repo_name,
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to run gh issue list: {}", e))?;

    if issue_output.status.success() {
        let issues: Vec<serde_json::Value> =
            serde_json::from_slice(&issue_output.stdout).unwrap_or_default();
        if !issues.is_empty() {
            let mut queues = state.queues.lock().await;
            let queue = get_or_create_queue(&mut queues, &state.githubclaw_home, repo_name)
                .map_err(|e| format!("Queue creation error: {}", e))?;

            for issue in &issues {
                queue
                    .enqueue(
                        serde_json::json!({
                            "type": "virtual_bootstrap",
                            "source": "bootstrap",
                            "item_type": "issue",
                            "data": issue
                        }),
                        "virtual_bootstrap",
                    )
                    .map_err(|e| format!("Enqueue error: {}", e))?;
            }
            info!(
                "Bootstrapped {} open issues for {}",
                issues.len(),
                repo_name
            );
        }
    } else {
        warn!(
            "gh issue list failed for {}: {}",
            repo_name,
            String::from_utf8_lossy(&issue_output.stderr).trim()
        );
    }

    // Scan existing open PRs
    let pr_output = tokio::process::Command::new("gh")
        .args([
            "pr",
            "list",
            "--state",
            "open",
            "--json",
            "number,title,labels",
            "--limit",
            "100",
            "--repo",
            repo_name,
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to run gh pr list: {}", e))?;

    if pr_output.status.success() {
        let prs: Vec<serde_json::Value> =
            serde_json::from_slice(&pr_output.stdout).unwrap_or_default();
        if !prs.is_empty() {
            let mut queues = state.queues.lock().await;
            let queue = get_or_create_queue(&mut queues, &state.githubclaw_home, repo_name)
                .map_err(|e| format!("Queue creation error: {}", e))?;

            for pr in &prs {
                queue
                    .enqueue(
                        serde_json::json!({
                            "type": "virtual_bootstrap",
                            "source": "bootstrap",
                            "item_type": "pull_request",
                            "data": pr
                        }),
                        "virtual_bootstrap",
                    )
                    .map_err(|e| format!("Enqueue error: {}", e))?;
            }
            info!("Bootstrapped {} open PRs for {}", prs.len(), repo_name);
        }
    } else {
        warn!(
            "gh pr list failed for {}: {}",
            repo_name,
            String::from_utf8_lossy(&pr_output.stderr).trim()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the axum router with shared state.
pub fn create_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/webhook", post(webhook_handler))
        .route("/health", get(health_handler))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Dispatch execution
// ---------------------------------------------------------------------------

/// Dispatch action extracted from the orchestrator's `Action::Dispatch` variant.
pub struct DispatchAction {
    pub agent_type: String,
    pub issue_ref: String,
    pub task_context: String,
}

/// Execute a single dispatch action: validate agent, check fork gate, assemble
/// prompt, build command, and spawn the agent subprocess.
///
/// This function is still used by the `githubclaw dispatch` CLI command
/// for direct agent spawning. The server drain loop no longer calls it directly
/// because the orchestrator subprocess handles dispatch via CLI.
#[allow(dead_code)]
async fn execute_dispatch(
    state: &Arc<ServerState>,
    dispatch: &DispatchAction,
    event_payload: &serde_json::Value,
    repo_full_name: &str,
) -> Result<(), String> {
    // 0. Check rate limiter before dispatching.
    if state.rate_limiter.is_dispatch_paused() {
        return Err("Rate limited: dispatch paused".into());
    }

    // 1. Resolve the repo's local path from the registry.
    let registry = state.registry.read().await;
    let entry = registry
        .get(repo_full_name)
        .ok_or_else(|| format!("Repo {} not in registry", repo_full_name))?;
    let repo_root = Path::new(&entry.local_path);

    // 2. Validate agent_type against repo overrides / global profiles (or built-in defaults).
    let agent_def = load_agent_definition(
        repo_full_name,
        &dispatch.agent_type,
        Some(&state.githubclaw_home),
    )?;

    // 3. Check the fork PR gate — block execution-capable agents on unapproved fork PRs.
    if !check_fork_pr_gate(event_payload, &dispatch.agent_type) {
        return Err(format!(
            "Fork PR gate blocked dispatch of agent '{}' for {}",
            dispatch.agent_type, repo_full_name,
        ));
    }

    // 4. Check concurrency capacity — return a special error so the drain
    //    loop knows to wait instead of nacking.
    if !state
        .process_manager
        .has_capacity_for(crate::process_manager::ProcessKind::Worker)
        .await
    {
        return Err(format!(
            "CAPACITY_FULL: No worker capacity for agent '{}' — {}/{} workers running",
            dispatch.agent_type,
            state.process_manager.active_worker_count().await,
            state.process_manager.max_concurrent_workers,
        ));
    }

    // 5. Assemble the 4-layer prompt.
    //    IMPORTANT: We must NOT drop the assembler here because its Drop impl
    //    deletes the temp file. The agent subprocess reads the file asynchronously
    //    after we spawn it. We leak the assembler and schedule cleanup after the
    //    agent process exits.
    let mut assembler = PromptAssembler::with_home(repo_full_name, &state.githubclaw_home);
    let prompt_file = assembler
        .assemble(&agent_def, &dispatch.task_context)
        .map_err(|e| format!("Prompt assembly failed: {}", e))?;
    let prompt_file_for_cleanup = prompt_file.clone();
    // Prevent Drop from deleting the temp file — we'll clean up after agent exits.
    std::mem::forget(assembler);

    // 6. Build command + env via AgentSpawner.
    let spawner = AgentSpawner::new(repo_root, crate::constants::DEFAULT_AGENT_MAX_TURNS);
    let cmd_parts = spawner.build_command(&agent_def, &prompt_file, &dispatch.task_context)?;
    let env_map = spawner.build_env(&agent_def, &prompt_file, &dispatch.task_context, None);

    // 7. Spawn via tokio::process::Command.
    if cmd_parts.is_empty() {
        return Err("build_command returned empty command".into());
    }
    let program = &cmd_parts[0];
    let args = &cmd_parts[1..];

    info!(
        agent_type = %dispatch.agent_type,
        repo = %repo_full_name,
        issue_ref = %dispatch.issue_ref,
        "Spawning agent: {} {}",
        program,
        args.join(" "),
    );

    let child = tokio::process::Command::new(program)
        .args(args)
        .envs(&env_map)
        .current_dir(repo_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn agent '{}': {}", dispatch.agent_type, e))?;

    let pid = child.id().unwrap_or(0);
    info!(
        pid,
        agent_type = %dispatch.agent_type,
        repo = %repo_full_name,
        "Agent spawned successfully",
    );

    // 8. Register with ProcessManager for monitoring.
    let label = format!("{}/{}", dispatch.agent_type, dispatch.issue_ref);
    state
        .process_manager
        .register(
            pid,
            crate::process_manager::ProcessKind::Worker,
            repo_full_name,
            &label,
            crate::constants::DEFAULT_PROCESS_TIMEOUT_SECONDS,
        )
        .await;

    info!(
        pid,
        agent_type = %dispatch.agent_type,
        "Agent registered with process manager (pid={})",
        pid,
    );

    let process_manager = Arc::clone(&state.process_manager);
    let agent_type = dispatch.agent_type.clone();
    let repo_name = repo_full_name.to_string();
    tokio::spawn(async move {
        match child.wait_with_output().await {
            Ok(output) => {
                let exit_code = output.status.code().unwrap_or(1);
                process_manager.report_exit(pid, exit_code).await;

                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

                if output.status.success() {
                    info!(
                        pid,
                        agent_type = %agent_type,
                        repo = %repo_name,
                        stdout = %stdout,
                        "Agent exited successfully",
                    );
                } else {
                    warn!(
                        pid,
                        agent_type = %agent_type,
                        repo = %repo_name,
                        exit_code,
                        stdout = %stdout,
                        stderr = %stderr,
                        "Agent exited with failure",
                    );
                }
            }
            Err(e) => {
                process_manager.report_exit(pid, 1).await;
                warn!(
                    pid,
                    agent_type = %agent_type,
                    repo = %repo_name,
                    error = %e,
                    "Failed while waiting for agent process",
                );
            }
        }
        // Clean up the temp prompt file now that the agent has exited.
        if let Err(e) = std::fs::remove_file(&prompt_file_for_cleanup) {
            debug!(
                "Failed to clean up prompt file {:?}: {}",
                prompt_file_for_cleanup, e
            );
        }
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Event drain loop
// ---------------------------------------------------------------------------

/// Start background event-processing tasks for every registered repo.
///
/// Call this once after constructing the server state. Each repo gets its own
/// `tokio::spawn`'d drain loop.
pub async fn start_event_processing(state: Arc<ServerState>) {
    // Start the rate limiter recovery probe in the background.
    let rate_limiter = state.rate_limiter.clone();
    tokio::spawn(async move {
        rate_limiter.start_recovery_probe().await;
    });

    let registry = registry_snapshot(&state).await;
    for (repo_name, entry) in &registry {
        start_repo_processing(state.clone(), repo_name, entry).await;
    }
    info!("Event processing started for {} repos", registry.len());
}

/// Events worth sending to the orchestrator. Everything else is noise.
fn is_actionable_event(event: &serde_json::Value) -> bool {
    // Check the webhook event type stored in the payload
    let event_type = event
        .get("_githubclaw_event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let action = event.get("action").and_then(|v| v.as_str()).unwrap_or("");

    matches!(
        (event_type, action),
        ("issues", "opened")
            | ("issues", "closed")
            | ("issue_comment", "created")
            | ("pull_request", "opened")
            | ("pull_request", "closed")
            | ("pull_request_review", "submitted")
            | ("discussion", "created")
            | ("discussion_comment", "created")
            // check_run only on failure
            | ("check_run", "completed") // will further filter by conclusion below
    )
    // For check_run.completed, only pass through failures
    && if event_type == "check_run" {
        event
            .pointer("/check_run/conclusion")
            .and_then(|v| v.as_str())
            == Some("failure")
    } else {
        true
    }
    // Special case: issues.labeled only for "githubclaw-approved" (fork PR gate)
    || (event_type == "issues"
        && action == "labeled"
        && event
            .get("label")
            .and_then(|l| l.get("name"))
            .and_then(|n| n.as_str())
            == Some("githubclaw-approved"))
    || (event_type == "pull_request"
        && action == "labeled"
        && event
            .get("label")
            .and_then(|l| l.get("name"))
            .and_then(|n| n.as_str())
            == Some("githubclaw-approved"))
}

fn normalized_event_type(event: &serde_json::Value) -> &'static str {
    if let Some(event_type) = event
        .get("_githubclaw_event_type")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return match event_type {
            "issues" => "issues",
            "issue_comment" => "issue_comment",
            "pull_request" => "pull_request",
            "pull_request_review" => "pull_request_review",
            "discussion" => "discussion",
            "discussion_comment" => "discussion_comment",
            "check_run" => "check_run",
            _ => "unknown",
        };
    }

    if event.get("type").and_then(|v| v.as_str()) == Some("virtual_bootstrap") {
        return match event.get("item_type").and_then(|v| v.as_str()) {
            Some("issue") => "issues",
            Some("pull_request") => "pull_request",
            _ => "unknown",
        };
    }

    "unknown"
}

fn event_summary(event: &serde_json::Value) -> Option<String> {
    event
        .pointer("/issue/title")
        .or_else(|| event.pointer("/pull_request/title"))
        .or_else(|| event.pointer("/data/title"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn assemble_orchestrator_prompt_file(
    githubclaw_home: &Path,
    repo_name: &str,
    orch_def: &crate::agents::parser::AgentDefinition,
    orchestrator_prompt: &str,
) -> Result<(PromptAssembler, PathBuf), String> {
    let mut assembler = PromptAssembler::with_home(repo_name, githubclaw_home);
    let prompt_path = assembler
        .assemble(orch_def, orchestrator_prompt)
        .map_err(|e| format!("Failed to assemble orchestrator prompt: {}", e))?;
    Ok((assembler, prompt_path))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CodexExecMetadata {
    thread_id: Option<String>,
    last_agent_message: Option<String>,
}

fn parse_codex_exec_output(stdout: &[u8]) -> CodexExecMetadata {
    let mut metadata = CodexExecMetadata::default();

    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        if metadata.thread_id.is_none()
            && value.get("type").and_then(|v| v.as_str()) == Some("thread.started")
        {
            metadata.thread_id = value
                .get("thread_id")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
        }

        if value.get("type").and_then(|v| v.as_str()) == Some("item.completed")
            && value.pointer("/item/type").and_then(|v| v.as_str()) == Some("agent_message")
        {
            metadata.last_agent_message = value
                .pointer("/item/text")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
        }
    }

    metadata
}

/// Per-repo drain loop: peek the queue, send to orchestrator, execute actions.
async fn event_drain_loop(state: &Arc<ServerState>, repo_name: &str, _entry: &RegistryEntry) {
    let _repo_slug = repo_name.replace('/', "-");
    info!(repo = %repo_name, "Starting event drain loop");

    loop {
        if state.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            info!(
                "Shutdown signal received, stopping drain loop for {}",
                repo_name
            );
            break;
        }
        // 1. Recover any orphaned inflight event and peek the next queued event.
        let peeked = {
            let mut queues = state.queues.lock().await;
            let queue = match get_or_create_queue(&mut queues, &state.githubclaw_home, repo_name) {
                Ok(q) => q,
                Err(e) => {
                    error!(repo = %repo_name, "Failed to get queue: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            match queue.recover_orphaned_inflight(is_process_alive) {
                Ok(recovered) if recovered > 0 => {
                    info!(repo = %repo_name, recovered, "Recovered orphaned inflight events");
                }
                Ok(_) => {}
                Err(e) => {
                    error!(repo = %repo_name, "Failed to recover inflight events: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    continue;
                }
            }

            if queue.has_inflight() {
                None
            } else {
                match queue.peek() {
                    Ok(Some(event)) => Some(event),
                    Ok(None) => None,
                    Err(e) => {
                        error!(repo = %repo_name, "Failed to peek queue: {}", e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        continue;
                    }
                }
            }
        };

        let event = match peeked {
            Some(ev) => ev,
            None => {
                // Queue is empty or another orchestrator attempt is still inflight.
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        // 2. Filter: only actionable events reach the orchestrator
        if !is_actionable_event(&event.payload)
            && event.payload.get("type").and_then(|v| v.as_str()) != Some("virtual_bootstrap")
        {
            // Silently dequeue and skip
            let mut queues = state.queues.lock().await;
            if let Ok(q) = get_or_create_queue(&mut queues, &state.githubclaw_home, repo_name) {
                let _ = q.dequeue();
            }
            continue;
        }

        // 2b. Route the event to its root issue for the correct orchestrator session.
        let root_issue = state
            .issue_router
            .route_event(repo_name, &event.payload)
            .unwrap_or(None);
        if let Some(root) = root_issue {
            tracing::debug!(
                repo = %repo_name,
                root_issue = root,
                "Routed event to root issue #{}",
                root
            );

            let mut snapshot = load_or_new_runtime_snapshot(state, repo_name, root);
            snapshot.apply_event(&event.payload, root);
            save_runtime_snapshot(state, repo_name, &snapshot);
        }

        // 2c. Build the prompt for the orchestrator subprocess.
        // Detect markers and build ResumeMessage, or use raw event for new issues.
        let orchestrator_prompt = if let Some(comment_body) = event
            .payload
            .pointer("/comment/body")
            .and_then(|v| v.as_str())
        {
            let markers = crate::markers::parse_markers(comment_body);
            let summary = crate::markers::extract_summary(comment_body);
            if let (Some(marker), Some(root)) = (markers.first(), root_issue) {
                tracing::info!(
                    repo = %repo_name,
                    root_issue = root,
                    marker = marker.marker_type.as_str(),
                    "Marker detected, building resume message"
                );
                let agent_type = event
                    .payload
                    .pointer("/comment/user/login")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let comment_url = event
                    .payload
                    .pointer("/comment/html_url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let resume_msg = crate::resume_message::ResumeMessage::from_marker(
                    root,
                    marker,
                    summary,
                    agent_type,
                    comment_url,
                );
                resume_msg.to_prompt()
            } else {
                // Comment without markers — might be human direction or external
                let event_type = normalized_event_type(&event.payload);
                if let Some(root) = root_issue {
                    let resume_msg = crate::resume_message::ResumeMessage::from_human_comment(
                        root,
                        comment_body,
                        event
                            .payload
                            .pointer("/comment/html_url")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    );
                    resume_msg.to_prompt()
                } else {
                    format!(
                        "[GithubClaw Event] type={}\n\n{}",
                        event_type,
                        serde_json::to_string_pretty(&event.payload).unwrap_or_default()
                    )
                }
            }
        } else {
            // Non-comment event (issues.opened, pull_request.*, check_run.*, etc.)
            let event_type = normalized_event_type(&event.payload);
            if let Some(root) = root_issue {
                let summary = event_summary(&event.payload);
                let resume_msg = crate::resume_message::ResumeMessage::from_issue_event(
                    root, event_type, summary,
                );
                resume_msg.to_prompt()
            } else {
                format!(
                    "[GithubClaw Event] type={}\n\n{}",
                    event_type,
                    serde_json::to_string_pretty(&event.payload).unwrap_or_default()
                )
            }
        };

        // 3. Check rate limiter before sending to orchestrator.
        if state.rate_limiter.is_orchestrator_paused() {
            tracing::warn!("Rate limited (orchestrator paused), sleeping...");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            continue;
        }

        // 4. Check orchestrator capacity.
        if !state
            .process_manager
            .has_capacity_for(crate::process_manager::ProcessKind::Orchestrator)
            .await
        {
            tracing::info!(
                repo = %repo_name,
                "Orchestrator capacity full, waiting..."
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            continue;
        }

        // 5. Spawn the orchestrator as a Claude Code subprocess.
        let issue_id = root_issue.unwrap_or(0);

        let registry = state.registry.read().await;
        let repo_dir = match registry.get(repo_name) {
            Some(entry) => entry.local_path.clone(),
            None => {
                error!(repo = %repo_name, "Repo not in registry");
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        drop(registry);

        let mut reserved_event = {
            let mut queues = state.queues.lock().await;
            let queue = match get_or_create_queue(&mut queues, &state.githubclaw_home, repo_name) {
                Ok(q) => q,
                Err(e) => {
                    error!(repo = %repo_name, "Failed to get queue for reserve: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    continue;
                }
            };

            match queue.reserve_next() {
                Ok(Some(event)) => event,
                Ok(None) => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
                    continue;
                }
                Err(e) => {
                    error!(repo = %repo_name, "Failed to reserve queue head: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    continue;
                }
            }
        };

        // Build environment
        // Use a deterministic session name so --resume works across invocations.
        let session_name = format!("githubclaw-{}-{}", repo_name.replace('/', "-"), issue_id);
        let mut env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if issue_id > 0 {
            env.insert("GITHUBCLAW_ROOT_ISSUE".to_string(), issue_id.to_string());
        }
        env.insert("GITHUBCLAW_REPO".to_string(), repo_name.to_string());
        env.insert(
            "GITHUBCLAW_EVENT_ID".to_string(),
            reserved_event.event_id.clone(),
        );

        // Inject gh wrapper PATH
        let spawner = crate::agents::spawner::AgentSpawner::new(
            &repo_dir,
            crate::constants::DEFAULT_AGENT_MAX_TURNS,
        );
        // Load orchestrator agent def to get full env
        let orch_def = match crate::agents::parser::load_agent_definition(
            repo_name,
            "orchestrator",
            Some(&state.githubclaw_home),
        ) {
            Ok(def) => def,
            Err(err) => {
                error!(
                    repo = %repo_name,
                    "Failed to load orchestrator agent definition: {}",
                    err
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        env.insert("GITHUBCLAW_SESSION_NAME".to_string(), session_name.clone());
        let stored_session_id = if issue_id > 0 {
            state.session_store.load(repo_name, issue_id).ok().flatten()
        } else {
            None
        };
        if let Some(session_id) = stored_session_id.as_ref() {
            env.insert("GITHUBCLAW_SESSION_ID".to_string(), session_id.clone());
        }

        let (prompt_assembler, prompt_path) = match assemble_orchestrator_prompt_file(
            &state.githubclaw_home,
            repo_name,
            &orch_def,
            &orchestrator_prompt,
        ) {
            Ok(result) => result,
            Err(err) => {
                error!(repo = %repo_name, "{}", err);
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        let full_env = spawner.build_env(&orch_def, &prompt_path, &orchestrator_prompt, Some(&env));
        env = full_env;
        let cmd_args = match spawner.build_resume_command(&orch_def) {
            Ok(cmd) => cmd,
            Err(err) => {
                error!(repo = %repo_name, "Failed to build orchestrator command: {}", err);
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        info!(
            repo = %repo_name,
            issue = issue_id,
            session = %session_name,
            "Spawning Orchestrator subprocess"
        );

        // Spawn orchestrator
        let program = cmd_args[0].clone();
        let args = cmd_args[1..].to_vec();
        let started_at = runtime_timestamp();

        if issue_id > 0 {
            let mut snapshot = load_or_new_runtime_snapshot(state, repo_name, issue_id);
            snapshot.note_agent_started(
                "orchestrator",
                &started_at,
                format!("Processing queued event for issue #{}", issue_id),
            );
            save_runtime_snapshot(state, repo_name, &snapshot);
        }

        let spawn_result = tokio::process::Command::new(&program)
            .args(&args)
            .envs(&env)
            .current_dir(&repo_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        match spawn_result {
            Ok(child) => {
                let pid = child.id().unwrap_or(0);

                // Register with process manager
                state
                    .process_manager
                    .register(
                        pid,
                        crate::process_manager::ProcessKind::Orchestrator,
                        repo_name,
                        &format!("orchestrator-issue-{}", issue_id),
                        crate::constants::DEFAULT_PROCESS_TIMEOUT_SECONDS,
                    )
                    .await;

                {
                    let mut queues = state.queues.lock().await;
                    if let Ok(queue) =
                        get_or_create_queue(&mut queues, &state.githubclaw_home, repo_name)
                    {
                        if let Err(err) = queue.attach_processor_pid(&mut reserved_event, pid) {
                            warn!(
                                repo = %repo_name,
                                pid = pid,
                                "Failed to attach orchestrator pid to inflight reservation: {}",
                                err
                            );
                        }
                    }
                }

                let output = child.wait_with_output().await;
                match output {
                    Ok(output) => {
                        let exit_code = output.status.code().unwrap_or(-1);
                        state.process_manager.report_exit(pid, exit_code).await;

                        let stdout = if orch_def.backend == "codex" {
                            let metadata = parse_codex_exec_output(&output.stdout);
                            if issue_id > 0 {
                                if let Some(thread_id) = metadata.thread_id.as_ref() {
                                    if let Err(err) =
                                        state.session_store.save(repo_name, issue_id, thread_id)
                                    {
                                        warn!(
                                            repo = %repo_name,
                                            issue = issue_id,
                                            error = %err,
                                            "Failed to persist Codex session id",
                                        );
                                    }
                                }
                            }
                            metadata.last_agent_message.unwrap_or_else(|| {
                                String::from_utf8_lossy(&output.stdout).trim().to_string()
                            })
                        } else {
                            String::from_utf8_lossy(&output.stdout).trim().to_string()
                        };
                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

                        if exit_code == 0 {
                            info!(
                                repo = %repo_name,
                                pid = pid,
                                event_id = %reserved_event.event_id,
                                stdout = %stdout,
                                "Orchestrator completed successfully"
                            );
                            let mut queues = state.queues.lock().await;
                            if let Ok(queue) =
                                get_or_create_queue(&mut queues, &state.githubclaw_home, repo_name)
                            {
                                if let Err(err) = queue.ack_reserved(&reserved_event) {
                                    error!(
                                        repo = %repo_name,
                                        event_id = %reserved_event.event_id,
                                        "Failed to ack reserved event: {}",
                                        err
                                    );
                                }
                            }
                        } else {
                            warn!(
                                repo = %repo_name,
                                pid = pid,
                                exit_code = exit_code,
                                event_id = %reserved_event.event_id,
                                stdout = %stdout,
                                stderr = %stderr,
                                "Orchestrator exited with non-zero code"
                            );
                            state.rate_limiter.report_orchestrator_rate_limit();
                            let mut queues = state.queues.lock().await;
                            if let Ok(queue) =
                                get_or_create_queue(&mut queues, &state.githubclaw_home, repo_name)
                            {
                                if let Err(err) = queue.nack_reserved(&mut reserved_event) {
                                    error!(
                                        repo = %repo_name,
                                        event_id = %reserved_event.event_id,
                                        "Failed to nack reserved event: {}",
                                        err
                                    );
                                }
                            }
                        }

                        if issue_id > 0 {
                            let mut snapshot =
                                load_or_new_runtime_snapshot(state, repo_name, issue_id);
                            let detail = if exit_code == 0 {
                                format!("Completed issue #{}", issue_id)
                            } else {
                                format!(
                                    "Exited with status {} while processing issue #{}",
                                    exit_code, issue_id
                                )
                            };
                            snapshot.note_agent_finished(
                                "orchestrator",
                                &started_at,
                                exit_code == 0,
                                detail,
                            );
                            save_runtime_snapshot(state, repo_name, &snapshot);
                        }
                    }
                    Err(e) => {
                        error!(
                            repo = %repo_name,
                            pid = pid,
                            event_id = %reserved_event.event_id,
                            "Failed to wait on orchestrator: {}",
                            e
                        );
                        state.process_manager.report_exit(pid, 1).await;
                        state.rate_limiter.report_orchestrator_rate_limit();

                        let mut queues = state.queues.lock().await;
                        if let Ok(queue) =
                            get_or_create_queue(&mut queues, &state.githubclaw_home, repo_name)
                        {
                            if let Err(err) = queue.nack_reserved(&mut reserved_event) {
                                error!(
                                    repo = %repo_name,
                                    event_id = %reserved_event.event_id,
                                    "Failed to nack reserved event after wait error: {}",
                                    err
                                );
                            }
                        }

                        if issue_id > 0 {
                            let mut snapshot =
                                load_or_new_runtime_snapshot(state, repo_name, issue_id);
                            snapshot.note_agent_finished(
                                "orchestrator",
                                &started_at,
                                false,
                                format!("Failed to wait for issue #{}: {}", issue_id, e),
                            );
                            save_runtime_snapshot(state, repo_name, &snapshot);
                        }
                    }
                }
            }
            Err(e) => {
                if issue_id > 0 {
                    let mut snapshot = load_or_new_runtime_snapshot(state, repo_name, issue_id);
                    snapshot.note_agent_finished(
                        "orchestrator",
                        &started_at,
                        false,
                        format!("Failed to spawn while processing issue #{}", issue_id),
                    );
                    save_runtime_snapshot(state, repo_name, &snapshot);
                }
                error!(
                    repo = %repo_name,
                    "Failed to spawn orchestrator: {}",
                    e,
                );
                state.rate_limiter.report_orchestrator_rate_limit();
                let mut queues = state.queues.lock().await;
                if let Ok(queue) =
                    get_or_create_queue(&mut queues, &state.githubclaw_home, repo_name)
                {
                    if let Err(err) = queue.nack_reserved(&mut reserved_event) {
                        error!(
                            repo = %repo_name,
                            event_id = %reserved_event.event_id,
                            "Failed to nack reserved event after spawn failure: {}",
                            err
                        );
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                continue;
            }
        }
        drop(prompt_assembler);
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::parser::AgentDefinition;
    use axum::body::Body;
    use axum::http::Request;
    use hmac::{Hmac, Mac};
    use http_body_util::BodyExt;
    use sha2::Sha256;
    use std::collections::HashMap;
    use tempfile::TempDir;
    use tower::ServiceExt;

    const TEST_SECRET: &str = "test-webhook-secret";

    /// Compute a valid HMAC SHA-256 signature for testing.
    fn sign_payload(payload: &[u8], secret: &str) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
        mac.update(payload);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    /// Build a ServerState with a single registered repo for testing.
    fn make_test_state(tmp: &TempDir) -> Arc<ServerState> {
        let mut registry = HashMap::new();
        registry.insert(
            "owner/repo".to_string(),
            RegistryEntry {
                local_path: tmp.path().to_string_lossy().to_string(),
                socket_path: String::new(),
            },
        );
        let scheduler_path = tmp.path().join("scheduled.json");
        Arc::new(ServerState {
            webhook_secret: TEST_SECRET.to_string(),
            registry: RwLock::new(registry),
            started_repos: RwLock::new(HashSet::new()),
            queues: Mutex::new(HashMap::new()),
            githubclaw_home: tmp.path().to_path_buf(),
            process_manager: Arc::new(ProcessManager::new(
                crate::constants::DEFAULT_MAX_CONCURRENT_AGENTS,
            )),
            scheduler: Mutex::new(ScheduledEventManager::new(scheduler_path)),
            rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::default()),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            issue_router: crate::issue_router::IssueRouter::new(tmp.path().join("sessions")),
            session_store: crate::session_store::SessionStore::with_base_dir(
                tmp.path().join("sessions"),
            ),
        })
    }

    /// Build a minimal webhook payload JSON for a given repo.
    fn make_payload(repo_full_name: &str, action: &str) -> Value {
        serde_json::json!({
            "action": action,
            "repository": {
                "full_name": repo_full_name,
            },
        })
    }

    /// Helper: read the full response body as a string.
    async fn body_string(response: axum::http::Response<Body>) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn write_test_orchestrator_prompt_layers(tmp: &TempDir, repo_name: &str) {
        let profile_dir = crate::config::profile_dir_from_home(tmp.path(), "default");
        let repo_dir = crate::config::repo_dir_from_home(tmp.path(), repo_name);

        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::create_dir_all(&repo_dir).unwrap();
        std::fs::write(profile_dir.join("global-prompt.md"), "GLOBAL LAYER").unwrap();
        std::fs::write(repo_dir.join("VALUE.md"), "VALUE LAYER").unwrap();
        std::fs::write(repo_dir.join("config.yaml"), "profile: default\n").unwrap();
    }

    // ---------------------------------------------------------------
    // 1. Health endpoint returns 200 with status ok
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_health_returns_200_with_status_ok() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body: Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["registered_repos"], 1);
    }

    // ---------------------------------------------------------------
    // 2. Webhook without signature returns 403
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_webhook_without_signature_returns_403() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let payload = serde_json::to_vec(&make_payload("owner/repo", "opened")).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("Content-Type", "application/json")
                    .header("X-Github-Event", "issues")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // ---------------------------------------------------------------
    // 3. Webhook with invalid signature returns 403
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_webhook_with_invalid_signature_returns_403() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let payload = serde_json::to_vec(&make_payload("owner/repo", "opened")).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("Content-Type", "application/json")
                    .header("X-Github-Event", "issues")
                    .header(
                        "X-Hub-Signature-256",
                        "sha256=0000000000000000000000000000000000000000000000000000000000000000",
                    )
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // ---------------------------------------------------------------
    // 4. Webhook with valid signature for registered repo returns 202
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_webhook_valid_signature_registered_repo_returns_202() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let payload = serde_json::to_vec(&make_payload("owner/repo", "opened")).unwrap();
        let signature = sign_payload(&payload, TEST_SECRET);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("Content-Type", "application/json")
                    .header("X-Github-Event", "issues")
                    .header("X-Hub-Signature-256", &signature)
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let text = body_string(response).await;
        assert_eq!(text, "Event queued");
    }

    // ---------------------------------------------------------------
    // 5. Webhook for unregistered repo returns 200 (ignored)
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_webhook_unregistered_repo_returns_200() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let payload = serde_json::to_vec(&make_payload("other/repo", "opened")).unwrap();
        let signature = sign_payload(&payload, TEST_SECRET);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("Content-Type", "application/json")
                    .header("X-Github-Event", "issues")
                    .header("X-Hub-Signature-256", &signature)
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let text = body_string(response).await;
        assert!(text.contains("Ignored"));
    }

    // ---------------------------------------------------------------
    // 6. annotate_fork_status adds flag for unapproved fork PR
    // ---------------------------------------------------------------
    #[test]
    fn test_annotate_fork_status_adds_flag_for_unapproved_fork() {
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "number": 42,
                "labels": [],
                "head": {
                    "repo": {
                        "fork": true,
                        "full_name": "attacker/repo",
                    }
                }
            }
        });

        let result = annotate_fork_status(payload);
        assert_eq!(result["_githubclaw_fork_unapproved"], true);
    }

    // ---------------------------------------------------------------
    // 7. annotate_fork_status does NOT add flag for non-fork PR
    // ---------------------------------------------------------------
    #[test]
    fn test_annotate_fork_status_no_flag_for_non_fork() {
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "number": 10,
                "labels": [],
                "head": {
                    "repo": {
                        "fork": false,
                        "full_name": "owner/repo",
                    }
                }
            }
        });

        let result = annotate_fork_status(payload);
        assert!(result.get("_githubclaw_fork_unapproved").is_none());
    }

    // ---------------------------------------------------------------
    // 8. annotate_fork_status does NOT add flag when label present
    // ---------------------------------------------------------------
    #[test]
    fn test_annotate_fork_status_no_flag_when_approved_label() {
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "number": 42,
                "labels": [
                    { "name": "githubclaw-approved" }
                ],
                "head": {
                    "repo": {
                        "fork": true,
                        "full_name": "contributor/repo",
                    }
                }
            }
        });

        let result = annotate_fork_status(payload);
        assert!(result.get("_githubclaw_fork_unapproved").is_none());
    }

    // ---------------------------------------------------------------
    // 9. load_registry parses nested format
    // ---------------------------------------------------------------
    #[test]
    fn test_load_registry_parses_nested_format() {
        let tmp = TempDir::new().unwrap();
        let registry_path = tmp.path().join("registry.json");
        std::fs::write(
            &registry_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "repos": {
                    "owner/repo": {
                        "local_path": "/home/user/repo",
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let registry = load_registry(&registry_path);
        assert_eq!(registry.len(), 1);
        assert!(registry.contains_key("owner/repo"));
        assert_eq!(registry["owner/repo"].local_path, "/home/user/repo");
    }

    // ---------------------------------------------------------------
    // 9b. load_registry parses flat format
    // ---------------------------------------------------------------
    #[test]
    fn test_load_registry_parses_flat_format() {
        let tmp = TempDir::new().unwrap();
        let registry_path = tmp.path().join("registry.json");
        std::fs::write(
            &registry_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "owner/repo": {
                    "local_path": "/home/user/repo",
                },
                "org/other": {
                    "local_path": "/home/user/other",
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let registry = load_registry(&registry_path);
        assert_eq!(registry.len(), 2);
        assert!(registry.contains_key("owner/repo"));
        assert!(registry.contains_key("org/other"));
    }

    // ---------------------------------------------------------------
    // 9c. load_registry returns empty for missing file
    // ---------------------------------------------------------------
    #[test]
    fn test_load_registry_returns_empty_for_missing_file() {
        let registry = load_registry(Path::new("/nonexistent/registry.json"));
        assert!(registry.is_empty());
    }

    #[test]
    fn test_get_or_create_queue_uses_repo_local_path() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("repo");
        let mut registry = HashMap::new();
        registry.insert(
            "owner/repo".to_string(),
            RegistryEntry {
                local_path: repo_root.to_string_lossy().to_string(),
                socket_path: String::new(),
            },
        );
        let mut queues = HashMap::new();

        let queue = get_or_create_queue(&mut queues, tmp.path(), "owner/repo").unwrap();
        queue
            .enqueue(serde_json::json!({"ok": true}), "test")
            .unwrap();

        assert!(crate::config::queue_dir_for_repo_from_home(tmp.path(), "owner/repo").exists());
        assert_eq!(queues.get("owner/repo").unwrap().size(), 1);
    }

    #[test]
    fn test_get_or_create_queue_uses_fallback_for_empty_local_path() {
        let tmp = TempDir::new().unwrap();
        let mut registry = HashMap::new();
        registry.insert(
            "owner/repo".to_string(),
            RegistryEntry {
                local_path: String::new(),
                socket_path: String::new(),
            },
        );
        let mut queues = HashMap::new();

        let queue = get_or_create_queue(&mut queues, tmp.path(), "owner/repo").unwrap();
        queue
            .enqueue(serde_json::json!({"ok": true}), "test")
            .unwrap();

        assert!(crate::config::queue_dir_for_repo_from_home(tmp.path(), "owner/repo").exists());
        assert_eq!(queues.get("owner/repo").unwrap().size(), 1);
    }

    // ---------------------------------------------------------------
    // 10. load_webhook_secret reads and trims file
    // ---------------------------------------------------------------
    #[test]
    fn test_load_webhook_secret_reads_and_trims() {
        let tmp = TempDir::new().unwrap();
        let secret_path = tmp.path().join("webhook_secret");
        std::fs::write(&secret_path, "  my-secret-value\n  ").unwrap();

        let secret = load_webhook_secret(&secret_path).unwrap();
        assert_eq!(secret, "my-secret-value");
    }

    // ---------------------------------------------------------------
    // 10b. load_webhook_secret returns error for missing file
    // ---------------------------------------------------------------
    #[test]
    fn test_load_webhook_secret_error_for_missing_file() {
        let result = load_webhook_secret(Path::new("/nonexistent/secret"));
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // 11. Webhook enqueues event with correct label
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_webhook_enqueues_with_correct_event_label() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let state_clone = Arc::clone(&state);
        let app = create_router(state);

        let payload = serde_json::to_vec(&make_payload("owner/repo", "opened")).unwrap();
        let signature = sign_payload(&payload, TEST_SECRET);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("Content-Type", "application/json")
                    .header("X-Github-Event", "issues")
                    .header("X-GitHub-Delivery", "delivery-123")
                    .header("X-Hub-Signature-256", &signature)
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Verify the queue has one event
        let queues = state_clone.queues.lock().await;
        let queue = queues.get("owner/repo").unwrap();
        assert_eq!(queue.size(), 1);
        let event = queue.peek().unwrap().unwrap();
        assert_eq!(event.event_id, "delivery-123");
        assert_eq!(event.event_type, "issues_opened");
    }

    // ---------------------------------------------------------------
    // 12. annotate_fork_status passes through non-PR payloads
    // ---------------------------------------------------------------
    #[test]
    fn test_annotate_fork_status_passes_through_non_pr_payload() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": { "number": 5 },
        });

        let result = annotate_fork_status(payload.clone());
        assert_eq!(result, payload);
        assert!(result.get("_githubclaw_fork_unapproved").is_none());
    }

    // ---------------------------------------------------------------
    // 13. execute_dispatch returns error for unknown agent_type
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_execute_dispatch_rejects_unknown_agent_type() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(crate::config::profile_agents_dir_from_home(
            tmp.path(),
            crate::config::DEFAULT_PROFILE_NAME,
        ))
        .unwrap();

        let state = make_test_state(&tmp);
        let dispatch = DispatchAction {
            agent_type: "nonexistent_agent".to_string(),
            issue_ref: "owner/repo#1".to_string(),
            task_context: "Do something".to_string(),
        };

        let event_payload = serde_json::json!({
            "action": "opened",
            "repository": { "full_name": "owner/repo" },
        });

        let result = execute_dispatch(&state, &dispatch, &event_payload, "owner/repo").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("No agent definition found"),
            "Expected 'No agent definition found' but got: {}",
            err,
        );
    }

    // ---------------------------------------------------------------
    // 14. execute_dispatch blocks unapproved fork PR
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_execute_dispatch_blocks_unapproved_fork_pr() {
        let tmp = TempDir::new().unwrap();
        // Create a valid agent definition so we get past the agent validation step.
        let agents_dir = crate::config::profile_agents_dir_from_home(
            tmp.path(),
            crate::config::DEFAULT_PROFILE_NAME,
        );
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("coder.md"),
            "---\nbackend: codex\n---\n\nDo work.\n",
        )
        .unwrap();

        let state = make_test_state(&tmp);
        let dispatch = DispatchAction {
            agent_type: "coder".to_string(),
            issue_ref: "owner/repo#42".to_string(),
            task_context: "Fix the bug".to_string(),
        };

        // Fork PR without approved label — should be blocked.
        let event_payload = serde_json::json!({
            "action": "opened",
            "repository": { "full_name": "owner/repo" },
            "pull_request": {
                "number": 42,
                "labels": [],
                "head": {
                    "repo": {
                        "fork": true,
                        "full_name": "attacker/repo",
                    }
                }
            }
        });

        let result = execute_dispatch(&state, &dispatch, &event_payload, "owner/repo").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("Fork PR gate blocked"),
            "Expected 'Fork PR gate blocked' but got: {}",
            err,
        );
    }

    // ---------------------------------------------------------------
    // 15. is_actionable_event filtering
    // ---------------------------------------------------------------
    #[test]
    fn test_is_actionable_event() {
        // issues.opened → actionable
        assert!(is_actionable_event(&serde_json::json!({
            "_githubclaw_event_type": "issues",
            "action": "opened"
        })));

        // issues.labeled → NOT actionable (unless githubclaw-approved)
        assert!(!is_actionable_event(&serde_json::json!({
            "_githubclaw_event_type": "issues",
            "action": "labeled",
            "label": { "name": "bug" }
        })));

        // issues.labeled with githubclaw-approved → actionable
        assert!(is_actionable_event(&serde_json::json!({
            "_githubclaw_event_type": "issues",
            "action": "labeled",
            "label": { "name": "githubclaw-approved" }
        })));

        // check_run.completed failure → actionable
        assert!(is_actionable_event(&serde_json::json!({
            "_githubclaw_event_type": "check_run",
            "action": "completed",
            "check_run": { "conclusion": "failure" }
        })));

        // check_run.completed success → NOT actionable
        assert!(!is_actionable_event(&serde_json::json!({
            "_githubclaw_event_type": "check_run",
            "action": "completed",
            "check_run": { "conclusion": "success" }
        })));

        // check_run.created → NOT actionable
        assert!(!is_actionable_event(&serde_json::json!({
            "_githubclaw_event_type": "check_run",
            "action": "created"
        })));

        // label.created → NOT actionable
        assert!(!is_actionable_event(&serde_json::json!({
            "_githubclaw_event_type": "label",
            "action": "created"
        })));
    }

    #[test]
    fn test_normalized_event_type_maps_virtual_bootstrap_issue() {
        let event = serde_json::json!({
            "type": "virtual_bootstrap",
            "item_type": "issue",
            "data": { "number": 56, "title": "Bootstrapped issue" }
        });

        assert_eq!(normalized_event_type(&event), "issues");
    }

    #[test]
    fn test_normalized_event_type_maps_virtual_bootstrap_pr() {
        let event = serde_json::json!({
            "type": "virtual_bootstrap",
            "item_type": "pull_request",
            "data": { "number": 57, "title": "Refactoring .githubclaw" }
        });

        assert_eq!(normalized_event_type(&event), "pull_request");
    }

    #[test]
    fn test_event_summary_reads_virtual_bootstrap_title() {
        let event = serde_json::json!({
            "type": "virtual_bootstrap",
            "item_type": "issue",
            "data": { "number": 56, "title": "[BUG] orchestrator exited with non-zero code" }
        });

        assert_eq!(
            event_summary(&event),
            Some("[BUG] orchestrator exited with non-zero code".to_string())
        );
    }

    #[test]
    fn test_event_summary_reads_virtual_bootstrap_pr_title() {
        let event = serde_json::json!({
            "type": "virtual_bootstrap",
            "item_type": "pull_request",
            "data": { "number": 57, "title": "Refactoring .githubclaw" }
        });

        assert_eq!(
            event_summary(&event),
            Some("Refactoring .githubclaw".to_string())
        );
    }

    #[test]
    fn test_parse_codex_exec_output_extracts_thread_id_and_last_agent_message() {
        let stdout = br#"{"type":"thread.started","thread_id":"thread-123"}
{"type":"item.completed","item":{"id":"item_0","type":"error","message":"warning"}}
{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"FIRST"}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"SECOND"}}"#;

        let metadata = parse_codex_exec_output(stdout);
        assert_eq!(
            metadata,
            CodexExecMetadata {
                thread_id: Some("thread-123".into()),
                last_agent_message: Some("SECOND".into()),
            }
        );
    }

    #[test]
    fn test_assemble_orchestrator_prompt_file_includes_full_layers() {
        let tmp = TempDir::new().unwrap();
        let repo_name = "owner/repo";
        write_test_orchestrator_prompt_layers(&tmp, repo_name);

        let orch_def = AgentDefinition {
            name: "orchestrator".into(),
            backend: "codex".into(),
            git_author_name: "GithubClaw Orchestrator".into(),
            git_author_email: "orchestrator@githubclaw.local".into(),
            timeout: None,
            tools: HashMap::new(),
            instruction_body: "AGENT INSTRUCTION LAYER".into(),
        };

        let (assembler, prompt_path) =
            assemble_orchestrator_prompt_file(tmp.path(), repo_name, &orch_def, "EVENT TASK LAYER")
                .unwrap();

        let prompt = std::fs::read_to_string(&prompt_path).unwrap();
        assert!(prompt.contains("GLOBAL LAYER"));
        assert!(prompt.contains("VALUE LAYER"));
        assert!(prompt.contains("AGENT INSTRUCTION LAYER"));
        assert!(prompt.contains("EVENT TASK LAYER"));

        drop(assembler);
        assert!(!prompt_path.exists());
    }
}

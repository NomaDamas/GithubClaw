//! Integration tests for GithubClaw
//!
//! Tests the full pipeline: webhook receipt -> queue -> orchestrator -> dispatch
//! Uses mock data (no actual GitHub API calls)

use std::sync::Arc;
use tokio::sync::Mutex;
use std::collections::HashMap;
use tempfile::TempDir;

use githubclaw::config::GlobalConfig;
use githubclaw::queue::DiskPersistedQueue;
use githubclaw::server::{ServerState, RegistryEntry, create_router};
use githubclaw::signature::verify_webhook_signature;
use githubclaw::orchestrator::schema::Action;
use githubclaw::process_manager::{ProcessManager, check_fork_pr_gate};
use githubclaw::scheduler::ScheduledEventManager;
use githubclaw::agents::parser::parse_agent_file;
use githubclaw::agents::prompt_assembler::PromptAssembler;
use githubclaw::rate_limiter::RateLimiter;

fn sign_payload(payload: &[u8], secret: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(payload);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Test: Full webhook -> enqueue -> dequeue cycle
#[tokio::test]
async fn test_webhook_to_queue_roundtrip() {
    let tmp = TempDir::new().unwrap();
    // Create a test state with a registered repo
    let secret = "test-secret-123";
    let mut registry = HashMap::new();
    registry.insert("owner/repo".to_string(), RegistryEntry {
        local_path: tmp.path().to_string_lossy().to_string(),
        socket_path: String::new(),
    });

    let state = Arc::new(ServerState {
        webhook_secret: secret.to_string(),
        registry: tokio::sync::RwLock::new(registry),
        started_repos: tokio::sync::RwLock::new(std::collections::HashSet::new()),
        queues: Mutex::new(HashMap::new()),
        githubclaw_home: tmp.path().to_path_buf(),
        process_manager: Arc::new(ProcessManager::new(5)),
        scheduler: Mutex::new(ScheduledEventManager::new(tmp.path().join("scheduled.json"))),
        rate_limiter: Arc::new(RateLimiter::default()),
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        orchestrator_backend: githubclaw::orchestrator::session::OrchestratorBackend::Codex,
        orchestrators: Mutex::new(HashMap::new()),
    });

    let app = create_router(state.clone());

    // Build a valid webhook payload
    let payload = serde_json::json!({
        "action": "opened",
        "repository": { "full_name": "owner/repo" },
        "issue": { "number": 42, "title": "Test issue" }
    });
    let body = serde_json::to_vec(&payload).unwrap();
    let sig = sign_payload(&body, secret);

    // Send webhook
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let response = app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("X-Hub-Signature-256", &sig)
            .header("X-Github-Event", "issues")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    ).await.unwrap();

    assert_eq!(response.status(), 202);

    // Verify event was queued
    let queues = state.queues.lock().await;
    let queue = queues.get("owner/repo").unwrap();
    assert_eq!(queue.size(), 1);

    // Peek and verify payload
    let event = queue.peek().unwrap().unwrap();
    let repo_name: &str = event.payload["repository"]["full_name"].as_str().unwrap();
    assert_eq!(repo_name, "owner/repo");
}

/// Test: ActionList parsing from various model outputs
#[test]
fn test_action_list_parsing_variants() {
    use githubclaw::orchestrator::session::OrchestratorSession;

    // Valid JSON
    let json = r##"{"actions":[{"type":"dispatch","agent_type":"coder","issue_ref":"#42","task_context":"Fix bug"}],"reasoning":"test"}"##;
    let result = OrchestratorSession::extract_action_list(json);
    assert!(result.validate().is_ok());
    assert_eq!(result.actions.len(), 1);

    // JSON in code fences
    let fenced = format!("Here's my decision:\n```json\n{}\n```", json);
    let result = OrchestratorSession::extract_action_list(&fenced);
    assert!(result.validate().is_ok());

    // Garbage text falls back to no_action
    let garbage = "I'm not sure what to do about this event.";
    let result = OrchestratorSession::extract_action_list(garbage);
    assert!(result.validate().is_ok());
    assert!(matches!(&result.actions[0], Action::NoAction { .. }));
}

/// Test: Agent parser -> prompt assembler -> spawner pipeline
#[test]
fn test_agent_pipeline() {
    let tmp = TempDir::new().unwrap();
    let agents_dir = tmp.path().join(".githubclaw").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    // Write agent definition
    std::fs::write(agents_dir.join("coder.md"), r#"---
backend: claude-code
git_author_name: GithubClaw Coder
git_author_email: coder@githubclaw.local
tools:
  claude-code:
    allowed: [Bash, Read, Write, Edit]
    disallowed: []
---

# Coder Agent

You are the Coder agent.
"#).unwrap();

    // Write global-prompt.md and VALUE.md
    let gc_dir = tmp.path().join(".githubclaw");
    std::fs::write(gc_dir.join("global-prompt.md"), "# Agent Roster\n\nCommon rules.").unwrap();
    std::fs::write(gc_dir.join("VALUE.md"), "# Mission\n\nBuild great software.").unwrap();

    // Parse agent
    let agent = parse_agent_file(&agents_dir.join("coder.md")).unwrap();
    assert_eq!(agent.name, "coder");
    assert_eq!(agent.backend, "claude-code");
    assert_eq!(agent.git_author_name, "GithubClaw Coder");

    // Assemble prompt
    let mut assembler = PromptAssembler::new(tmp.path());
    let prompt_file = assembler.assemble(&agent, "Fix the null check in auth.rs").unwrap();
    assert!(prompt_file.exists());
    let content = std::fs::read_to_string(&prompt_file).unwrap();
    assert!(content.contains("Agent Roster"));
    assert!(content.contains("Mission"));
    assert!(content.contains("Coder Agent"));
    assert!(content.contains("Fix the null check"));

    // Build command
    let spawner = githubclaw::agents::spawner::AgentSpawner::new(tmp.path(), 200);
    let cmd = spawner.build_command(&agent, &prompt_file, "Fix the null check").unwrap();
    assert!(cmd.iter().any(|a| a == "claude"));

    // Build env
    let env = spawner.build_env(&agent, &prompt_file, "Fix the null check", None);
    assert_eq!(env.get("GIT_AUTHOR_NAME").unwrap(), "GithubClaw Coder");
    assert_eq!(env.get("GITHUBCLAW_AGENT_TYPE").unwrap(), "coder");

    // Cleanup
    assembler.cleanup_all();
    assert!(!prompt_file.exists());
}

/// Test: Fork PR gate end-to-end
#[test]
fn test_fork_pr_gate_full_scenario() {
    // External fork PR without approval
    let fork_payload = serde_json::json!({
        "pull_request": {
            "number": 99,
            "head": { "repo": { "fork": true, "full_name": "attacker/repo" } },
            "labels": []
        }
    });

    // Security reviewer is allowed
    assert!(check_fork_pr_gate(&fork_payload, "security_reviewer"));

    // Coder is blocked
    assert!(!check_fork_pr_gate(&fork_payload, "coder"));

    // After human approval
    let approved_payload = serde_json::json!({
        "pull_request": {
            "number": 99,
            "head": { "repo": { "fork": true, "full_name": "attacker/repo" } },
            "labels": [{ "name": "githubclaw-approved" }]
        }
    });

    // Now coder is allowed
    assert!(check_fork_pr_gate(&approved_payload, "coder"));
}

/// Test: Queue disk persistence survives reconstruction
#[test]
fn test_queue_persistence() {
    let tmp = TempDir::new().unwrap();
    let queue_dir = tmp.path().join("queue");

    // Create queue, add events
    {
        let q = DiskPersistedQueue::new(&queue_dir, 3).unwrap();
        q.enqueue(serde_json::json!({"event": 1}), "test_event").unwrap();
        q.enqueue(serde_json::json!({"event": 2}), "test_event").unwrap();
        q.enqueue(serde_json::json!({"event": 3}), "test_event").unwrap();
    }

    // Reconstruct from same directory (simulates restart)
    {
        let q = DiskPersistedQueue::new(&queue_dir, 3).unwrap();
        assert_eq!(q.size(), 3);

        let e1 = q.dequeue().unwrap().unwrap();
        assert_eq!(e1.payload["event"], 1);

        let e2 = q.dequeue().unwrap().unwrap();
        assert_eq!(e2.payload["event"], 2);
    }
}

/// Test: Scheduler persistence roundtrip
#[test]
fn test_scheduler_persistence() {
    let tmp = TempDir::new().unwrap();
    let json_path = tmp.path().join("scheduled.json");

    // Create and add events
    {
        let mut mgr = ScheduledEventManager::new(&json_path);
        mgr.create_event(
            "owner/repo",
            chrono::Utc::now() + chrono::Duration::hours(1),
            serde_json::json!({"type": "visionary_daily"}),
            true,
            None,
            "Daily Visionary summary",
        );
        mgr.save().unwrap();
    }

    // Reload from disk
    {
        let mut mgr = ScheduledEventManager::new(&json_path);
        mgr.load().unwrap();
        let events = mgr.list_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].repo, "owner/repo");
        assert_eq!(events[0].description, "Daily Visionary summary");
    }
}

/// Test: Config validation resets invalid values
#[test]
fn test_config_validation() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.yaml");
    std::fs::write(&config_path, "port: 0\nmax_concurrent_agents: -1\n").unwrap();

    let config = GlobalConfig::load(Some(&config_path)).unwrap();
    assert_eq!(config.port, 8000); // Reset to default
    assert_eq!(config.max_concurrent_agents, 5); // Reset to default
}

/// Test: Signature verification
#[test]
fn test_signature_verification_e2e() {
    let secret = "my-webhook-secret";
    let payload = b"test payload body";

    // Compute correct signature
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(payload);
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    assert!(verify_webhook_signature(payload, &sig, secret));
    assert!(!verify_webhook_signature(payload, "sha256=wrong", secret));
    assert!(!verify_webhook_signature(payload, "", secret));
}

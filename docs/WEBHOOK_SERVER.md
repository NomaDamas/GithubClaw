# Webhook Server

The webhook server is the central nervous system of GithubClaw — a persistent Rust axum process that receives GitHub events, routes them to per-repo orchestrators, manages all child processes, and executes dispatch instructions.

## Responsibilities

1. **Event reception**: Receive GitHub webhook POST requests via public tunnel
2. **Signature verification**: Validate `X-Hub-Signature-256` on every request
3. **Registry routing**: Route events to correct repo based on `repository` field in payload
4. **Queue management**: Disk-persisted serial FIFO queues per repo
5. **Orchestrator IPC**: Deliver events to orchestrator child processes via Unix sockets
6. **Prompt assembly**: Mechanically assemble 4-layer agent prompts from config files
7. **Frontmatter parsing**: YAML parser for agent tool permissions — never trusted to LLM
8. **Process lifecycle**: Spawn, monitor exit codes, idle timeout, crash detection for all child processes
9. **Dispatch execution**: Translate orchestrator structured output into CLI spawn commands
10. **Fork PR gate**: Mechanical `githubclaw-approved` label check from webhook payload fields
11. **Scheduled events**: asyncio timer loop checking `~/.githubclaw/scheduled.json` every 60s
12. **Rate limit handling**: Three-tier detection, hibernation, and timer-based recovery
13. **Concurrency throttle**: `max_concurrent_agents` limit — queue dispatches when full
14. **Bootstrap**: Inject virtual events for existing issues/PRs on first start

## HTTP Endpoints

### External (public via tunnel, port configurable)

```
POST /webhook
  - Receives GitHub App webhook payloads
  - Validates X-Hub-Signature-256
  - Discards events from repos not in registry.json
  - Checks fork PR gate before queuing
  - Persists to disk-backed serial queue

GET /setup/install/callback
  - Minimal hosted setup callback for GitHub App installs
  - Accepts `installation_id` from the setup redirect
  - Mints a short-lived single-use `claim_proof`
  - Returns the exact handoff payload for `POST /register`

POST /register
  - Claims or updates one installation's hosted-proxy tunnel URL
  - Uses one-time `claim_proof` for the first claim
  - Uses rotating `update_secret` for later updates
  - Rotates `update_secret` on every successful update
  - Rejects unsafe or malformed tunnel URLs
  - See `docs/HOSTED_PROXY.md` for the full contract
```

No internal HTTP endpoints needed — scheduled events use tokio timers in-process.

## Event Queue

```
Per-repo queue:  .githubclaw/queue/
├── 001_issue_opened.json
├── 002_issue_comment.json
├── 003_virtual_bootstrap.json    (bootstrap)
├── 004_scheduled_fired.json      (from asyncio timer)
├── 005_error_feedback.json       (invalid agent_type correction)
└── ...

Dead-letter:     .githubclaw/queue/dead/
├── failed_event_after_max_retry.json
└── ...
```

- Events processed FIFO, one at a time per repo
- Disk persistence survives crash/restart
- `max_retry` configurable — exceeded events move to dead-letter
- Bootstrap virtual events block normal operation until drained

## Prompt Assembly (4-Layer)

Webhook server reads files and concatenates deterministically:

```
Layer 1: .githubclaw/global-prompt.md     (agent roster, common rules)
Layer 2: .githubclaw/VALUE.md             (project north star, read fresh)
Layer 3: .githubclaw/agents/{type}.md     (instruction body, minus frontmatter)
Layer 4: task_context from orchestrator    (minimal pointer + brief summary)
```

Result written to temp file, passed to CLI:
- Claude Code: `claude -p --append-system-prompt-file /tmp/task_xyz.md ...`
- Codex: `cat /tmp/task_xyz.md | codex exec -`

Temp file cleaned up after agent exits.

## Frontmatter Parsing

Agent definition files have YAML frontmatter parsed mechanically by Rust (serde_yaml):

```yaml
---
backend: codex                          # or claude-code
timeout: 7200                           # optional override
git_author_name: GithubClaw Coder
git_author_email: coder@githubclaw.local
tools:
  claude-code:
    allowed: [Bash, Read, Write, Edit, Glob, Grep]
    disallowed: []
  codex:
    allowed: [shell, file_read, file_write]
    disallowed: []
---
```

Webhook server extracts these values and maps them to CLI flags at spawn time. The orchestrator never sees or handles tool permissions.

## Spawn Templates

Overridable shell scripts in `.githubclaw/`:

```bash
# spawn_claude.sh (default)
claude -p \
  --dangerously-skip-permissions \
  --allowedTools "${ALLOWED_TOOLS}" \
  --disallowedTools "${DISALLOWED_TOOLS}" \
  --max-turns ${MAX_TURNS:-200} \
  --append-system-prompt-file "${PROMPT_FILE}" \
  "${TASK_PROMPT}"

# spawn_codex.sh (default)
cat "${PROMPT_FILE}" | codex exec - \
  --approval-mode full-auto \
  --sandbox off
```

Users can customize these for special flags, environment variables, or alternative CLI configurations.

## Process Management

All child processes (orchestrators + workers) managed uniformly:

| Lifecycle Event | Action |
|----------------|--------|
| Spawn | Fork process, register PID, start monitoring |
| Running | Monitor exit code, track elapsed time |
| Normal exit (code 0) | Log completion, clean up temp files |
| Crash (non-zero exit) | Inject failure event into orchestrator queue |
| Timeout (global 2h) | Kill process, inject failure event |
| Idle timeout (orchestrator only) | Save session, shut down process |
| Rate limit detected | Pause dispatches, alert user via `gh issue create` |

## Fork PR Gate

Enforced mechanically from webhook payload fields before any dispatch execution:

```rust
fn check_fork_pr_gate(event_payload: &WebhookPayload, agent_type: &str) -> bool {
    let Some(pr) = &event_payload.pull_request else { return true };
    let Some(head_repo) = &pr.head.repo else { return true };

    if !head_repo.fork {
        return true; // Not a fork PR, allow
    }

    let has_approval = pr.labels.iter()
        .any(|l| l.name == "githubclaw-approved");
    if has_approval {
        return true; // Approved, allow
    }

    if agent_type == "security_reviewer" {
        return true; // Security reviewer is always allowed (read-only)
    }

    false // Block, inject corrective feedback to orchestrator
}
```

## Rate Limit Handling

Three-tier response, all handled in Rust (no LLM calls):

| Tier | Trigger | Response |
|------|---------|----------|
| Worker limit | Agent exit code indicates API rate limit | Pause new dispatches, alert user via `gh issue create`, queue events |
| Orchestrator limit | Orchestrator session fails to respond | Full hibernate — stop feeding events, queue to disk |
| Both exhausted | All API calls failing | Full hibernate + alert, webhook server stays alive for event reception |

Recovery: tokio timer periodically attempts lightweight API probe. On success, resume normal operation and drain queued events.

## Scheduled Events

```rust
async fn scheduled_event_loop(state: Arc<ServerState>) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let now = Utc::now();
        let mut scheduler = state.scheduler.lock().await;
        for event in scheduler.due_events(now) {
            state.inject_into_queue(&event.repo, &event.payload).await;
            if event.one_shot {
                scheduler.remove(&event.event_id);
            }
        }
        scheduler.save().unwrap();
    }
}
```

- Persisted to `~/.githubclaw/scheduled.json`
- Supports both recurring (Visionary daily) and one-shot ("check #42 in 2h")
- `cancel_event` removes from list by `event_id`
- On webhook server restart, list reloaded from JSON and stale events fired immediately

## Shutdown

```bash
githubclaw stop          # Graceful drain: stop accepting webhooks,
                         # wait for running agents to finish (configurable max wait),
                         # then shut down

githubclaw stop --force  # Immediate kill of all child processes
```

## Bootstrap

On first start for a repo (no prior queue state):

1. Scan existing issues: `gh issue list --state open --json number,title,labels`
2. Scan existing PRs: `gh pr list --state open --json number,title,labels`
3. Create virtual event per item, inject into serial queue
4. Process all virtual events before accepting real-time webhooks (FIFO blocking)

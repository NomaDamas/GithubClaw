# Directory Layout

## Global Configuration (`~/.githubclaw/`)

Per-user, shared across all repos. Created during initial `cargo install githubclaw` setup (or via pre-built binary).

```
~/.githubclaw/
├── config.yaml                 # Webhook server settings
├── hosted_proxy_state.json     # Hosted proxy tunnel registration + claim replay state
├── registry.json               # Repo → local path mapping
├── scheduled.json              # All scheduled events across repos
├── secrets/
│   ├── webhook_secret          # GitHub App HMAC secret
│   └── twitter_credentials     # Platform API keys (extensible)
├── sessions/                    # Persisted orchestrator sessions
│   └── {repo-name}/
│       └── session_state.json   # Message history + state for resume
└── logs/
    └── webhook_server.log      # Server-level logs
```

### config.yaml

```yaml
server:
  port: 8000
  host: "0.0.0.0"

process:
  max_concurrent_agents: 5
  global_timeout: 7200              # 2 hours in seconds
  orchestrator_idle_timeout: 1800   # 30 minutes
  drain_timeout: 300                # 5 minutes for graceful shutdown

rate_limit:
  recovery_probe_interval: 300     # 5 minutes between probes

queue:
  max_retry: 3

event_subscription:
  - issues
  - issue_comment
  - pull_request
  - pull_request_review
  - pull_request_review_comment
  - discussion
  - discussion_comment
  - label
  - milestone
  - projects_v2_item
  - check_suite
  - check_run
```

### registry.json

```json
{
  "repos": {
    "owner/my-project": {
      "local_path": "/Users/jeffrey/Projects/my-project",
      "socket_path": "/tmp/githubclaw-my-project.sock"
    },
    "owner/another-repo": {
      "local_path": "/Users/jeffrey/Projects/another-repo",
      "socket_path": "/tmp/githubclaw-another-repo.sock"
    }
  }
}
```

### hosted_proxy_state.json

```json
{
  "version": 1,
  "installations": {
    "123456": {
      "tunnel_url": "https://abc123.trycloudflare.com",
      "update_secret_verifier": "sha256:...",
      "created_at": "2026-03-12T00:00:00Z",
      "updated_at": "2026-03-12T01:00:00Z",
      "consumed_claim_proofs": {
        "sha256:...": "2026-03-12T00:00:05Z"
      }
    }
  }
}
```

### scheduled.json

```json
{
  "events": [
    {
      "event_id": "evt_visionary_daily",
      "repo": "owner/my-project",
      "trigger_at": "2026-03-11T09:00:00Z",
      "recurring": "0 9 * * *",
      "payload": {"type": "cron", "trigger": "visionary_daily"},
      "context": "Daily Visionary summary"
    },
    {
      "event_id": "evt_check_42",
      "repo": "owner/my-project",
      "trigger_at": "2026-03-10T16:00:00Z",
      "recurring": null,
      "payload": {"type": "follow_up", "issue_ref": "#42"},
      "context": "Check if Coder completed the fix"
    }
  ]
}
```

## Per-Repo Configuration (`.githubclaw/`)

Created by `githubclaw init`, lives inside the target repo. User controls what gets committed via `.gitignore`.

```
.githubclaw/
├── orchestrator.md             # Orchestrator system prompt (user-editable)
├── global-prompt.md            # Agent roster, common rules, handoff conventions
├── VALUE.md                    # Project north star / mission statement
├── agents/                     # Agent definition files
│   ├── cs.md
│   ├── bug_tracker.md
│   ├── librarian.md
│   ├── project_manager.md
│   ├── coder.md
│   ├── qa.md
│   ├── reviewer.md
│   ├── contents_marketer.md
│   ├── visionary.md
│   └── security_reviewer.md
├── ai_instructions/            # Shared skill files (composable modules)
│   ├── playwright_testing.md
│   ├── github_projects_v2.md
│   └── twitter_publishing.md
├── spawn_claude.sh             # Claude Code spawn template (overridable)
├── spawn_codex.sh              # Codex spawn template (overridable)
├── memory.md                   # Orchestrator long-term memory (sectioned)
├── config.yaml                 # Per-repo overrides (allowed_read_paths, etc.)
├── logs/                       # Orchestrator decision logs
│   ├── 2026-03-10.jsonl
│   └── ...
├── queue/                      # Disk-persisted event queue
│   ├── 001_event.json
│   ├── 002_event.json
│   └── dead/                   # Dead-letter queue
│       └── failed_event.json
└── .gitignore                  # User-controlled
```

### Recommended .gitignore

```gitignore
# Always ignore
secrets/
queue/
logs/
memory.md

# User's choice — may want to commit for version control
# orchestrator.md
# global-prompt.md
# VALUE.md
# agents/
# ai_instructions/
# spawn_claude.sh
# spawn_codex.sh
# config.yaml
```

## File Ownership

| File | Who reads | Who writes |
|------|-----------|------------|
| `orchestrator.md` | Orchestrator (session init) | User (manual edit) |
| `global-prompt.md` | Orchestrator (every event) | User (manual edit) |
| `VALUE.md` | Webhook server (prompt assembly) | User (manual edit) |
| `agents/*.md` | Webhook server (frontmatter + body) | User (manual edit) |
| `ai_instructions/*.md` | Claude Code / Codex (native skill system) | User (manual edit) |
| `spawn_*.sh` | Webhook server (process spawn) | User (manual edit) |
| `memory.md` | Orchestrator (selective read) | Orchestrator (write_memory tool) |
| `config.yaml` (per-repo) | Webhook server (read_file whitelist, etc.) | User (manual edit) |
| `logs/*.jsonl` | Visionary, user, Claude Code (on request) | Orchestrator (via webhook server logging) |
| `queue/*` | Webhook server | Webhook server |
| `~/.githubclaw/config.yaml` | Webhook server | User / setup agent |
| `~/.githubclaw/hosted_proxy_state.json` | Hosted proxy registration handler | Hosted proxy registration handler |
| `~/.githubclaw/registry.json` | Webhook server | Setup agent |
| `~/.githubclaw/scheduled.json` | Webhook server (tokio timer) | Webhook server |
| `~/.githubclaw/secrets/*` | Webhook server (env var injection) | User / setup agent |

## Runtime Files

```
/tmp/
├── githubclaw-my-project.sock      # Unix socket for orchestrator IPC
├── githubclaw-another-repo.sock    # Unix socket per repo
├── githubclaw_prompt_abc123.md     # Temp prompt file for agent spawn
└── ...                              # Temp files cleaned up after agent exit
```

## CLI Commands

```bash
cargo install githubclaw            # Install from crates.io (or download pre-built binary)

githubclaw init                     # Scaffold .githubclaw/ in current repo
githubclaw start                    # Start webhook server (daemonize)
githubclaw stop                     # Graceful drain shutdown
githubclaw stop --force             # Immediate kill
githubclaw status                   # Show running processes, registered repos, queue sizes
githubclaw logs                     # View webhook server logs
githubclaw logs --follow            # Tail webhook server logs
```

## Setup Flow

```
1. cargo install githubclaw
2. cd /path/to/your/repo
3. githubclaw init                  # Scaffolds .githubclaw/
4. [Coding agent assists with:]
   - GitHub App creation (browser auth flow)
   - Tunnel setup (Cloudflare Tunnel, ngrok, etc.)
   - Webhook URL registration
   - launchd/systemd service registration
   - registry.json update
   - Discussion categories creation ("Roadmap", "Content Drafts")
5. Edit VALUE.md with project mission
6. Customize agent prompts as needed
7. githubclaw start
```

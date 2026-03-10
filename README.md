# GithubClaw

**Near-autonomous AI agents that manage your open-source project end-to-end.**

GithubClaw treats GitHub as the single source of truth. A webhook server receives events, an orchestrator classifies them, and specialized AI agents handle everything from triaging issues to writing code to reviewing PRs — all visible through normal GitHub workflows. You stay in control through branch protection, mission-level configuration, and fully editable agent prompts.

## Quick Start

```bash
pip install githubclaw            # or: uv add githubclaw

cd /path/to/your-repo
githubclaw init                   # scaffold .githubclaw/ directory
```

1. Edit `.githubclaw/VALUE.md` with your project's mission statement.
2. Create a GitHub App configured for webhook delivery only (see GitHub docs on creating a GitHub App). Store the webhook secret in `~/.githubclaw/secrets/webhook_secret`.
3. Set up a tunnel (Cloudflare Tunnel, ngrok, or similar) pointing to `localhost:8000`.
4. Start the server:

```bash
githubclaw start
```

## How It Works

```
                       GitHub
                         |
                   (webhook events)
                         |
                         v
            +------------------------+
            |    Webhook Server      |
            |    FastAPI + Python    |
            |                        |
            | - Signature verify     |
            | - Event routing        |
            | - Process management   |
            | - Prompt assembly      |
            +------------------------+
                |              |
         (Unix socket)   (CLI spawn)
                |              |
                v              v
        +-------------+  +-------------+
        | Orchestrator |  |   Worker    |
        | (per-repo)   |  |   Agents    |
        | Claude Agent |  | Claude Code |
        | SDK          |  |   / Codex   |
        +-------------+  +-------------+
                               |
                         (gh CLI / git)
                               |
                               v
                            GitHub
```

Events flow in a loop: GitHub fires a webhook, the server routes it to a per-repo orchestrator, the orchestrator dispatches a worker agent, the agent acts on GitHub, and the resulting event re-enters the loop.

## Agent Team

| Agent | Role | Trigger |
|-------|------|---------|
| **CS** | Triage issues, answer questions, close duplicates | New issues, external comments |
| **Bug Tracker** | Reproduce and diagnose bugs (never fixes them) | Issues labeled as bugs |
| **Librarian** | Maintain docs, README, and guides | PR merges with feature changes |
| **Project Manager** | Decompose tasks, prioritize, detect blockers | Large or new issues needing breakdown |
| **Coder** | Write code, open PRs, fix CI failures | Implementation tasks |
| **QA** | E2E testing with Playwright + VLM screenshots | After Coder opens PR and CI passes |
| **Reviewer** | Code review, request changes, merge to dev | After QA passes |
| **Contents Marketer** | Draft tweets, blog posts, announcements | Notable feature merges or cron |
| **Visionary** | Daily summaries, roadmap proposals, strategic ideas | Daily cron |
| **Security Reviewer** | Read-only audit of fork PR diffs | Fork PRs detected |

## CLI Commands

| Command | Description |
|---------|-------------|
| `githubclaw init` | Scaffold `.githubclaw/` in the current repo |
| `githubclaw start` | Start the webhook server (daemonized) |
| `githubclaw stop` | Graceful drain shutdown |
| `githubclaw stop --force` | Immediate kill |
| `githubclaw status` | Show running processes |
| `githubclaw logs` | Tail webhook server and orchestrator logs |

## Directory Layout

```
.githubclaw/
├── orchestrator.md          # Orchestrator system prompt
├── global-prompt.md         # Common rules, agent roster, handoff conventions
├── VALUE.md                 # Project mission / north star
├── agents/                  # One prompt file per agent (YAML frontmatter + markdown)
│   ├── cs.md
│   ├── bug_tracker.md
│   ├── coder.md
│   ├── qa.md
│   ├── reviewer.md
│   └── ...
├── ai_instructions/         # Shared skill modules (Playwright, GitHub Projects, etc.)
├── config.yaml              # Per-repo overrides
├── memory.md                # Orchestrator long-term memory
├── logs/                    # Decision logs (JSONL)
└── queue/                   # Disk-persisted event queue
```

Global config lives at `~/.githubclaw/` (webhook server settings, repo registry, secrets, scheduled events).

## Configuration

- **VALUE.md** -- Your project's mission statement. Every agent reads this to align decisions with your goals.
- **Agent prompts** -- Fully customizable in `.githubclaw/agents/`. Each file has YAML frontmatter (backend, git author, allowed tools) and a markdown instruction body. Your edits are never overwritten by upgrades.
- **config.yaml** -- Per-repo overrides (allowed read paths, timeouts, etc.) in `.githubclaw/config.yaml`. Global settings in `~/.githubclaw/config.yaml`.

## Architecture

All agent PRs target `dev`. The Reviewer agent can approve and merge to `dev`, but `dev -> main` always requires human approval via branch protection.

For detailed specs, see:

- `docs/ARCHITECTURE.md` -- System components, event flow, technology stack
- `docs/AGENTS.md` -- Full agent roster with triggers, scopes, and interaction patterns
- `docs/DIRECTORY_LAYOUT.md` -- Complete file listing and ownership matrix

## Requirements

- Python 3.11+
- `gh` CLI (authenticated)
- Claude Code or Codex CLI
- A tunnel service (Cloudflare Tunnel, ngrok, etc.)

## License

MIT

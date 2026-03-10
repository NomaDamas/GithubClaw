# GithubClaw Architecture

## Overview

GithubClaw is a system of near-autonomous AI agents that manage open-source projects end-to-end using **GitHub as the single source of truth**. A Python webhook server receives GitHub events and routes them to a stateful orchestrator, which classifies each event and dispatches specialized worker agents (Claude Code / Codex CLI processes) to handle tasks autonomously.

## Core Philosophy

- **GitHub is everything**: Issues, PRs, Discussions, Projects, comments — all communication, memory, and project management flows through GitHub.
- **Simplicity over complexity**: LLM judgment over mechanical rules, except at security boundaries.
- **Radical transparency**: Every decision and action is auditable — orchestrator logs, branded agent comments, per-agent git author identities.
- **Trust the model**: Behavioral constraints via instruction prompts; mechanical enforcement only where consequences are irreversible.
- **User sovereignty**: All config files are user-editable, customizations are never overwritten by upgrades.

## System Components

```
                         GitHub
                           |
                     (webhook events)
                           |
                           v
              +------------------------+
              |    Webhook Server      |  (Global, one per user)
              |    FastAPI + Python    |  (~/.githubclaw/)
              |                        |
              | - Signature verify     |
              | - Registry routing     |
              | - Serial event queues  |
              | - Process management   |
              | - Prompt assembly      |
              | - Scheduled events     |
              | - Rate limit handling  |
              +------------------------+
                  |              |
           (Unix socket)    (CLI spawn)
                  |              |
                  v              v
          +-------------+  +-------------+
          | Orchestrator |  |   Worker    |
          | (per-repo)   |  |   Agents    |
          | Agent SDK    |  | Claude Code |
          |              |  |   / Codex   |
          | - Classify   |  |             |
          | - Route      |  | - CS        |
          | - Schedule   |  | - Bug Track |
          | - Memory     |  | - Librarian |
          +-------------+  | - PM        |
                            | - Coder     |
                            | - QA        |
                            | - Reviewer  |
                            | - Marketer  |
                            | - Visionary |
                            | - Security  |
                            +-------------+
                                  |
                            (gh CLI / git)
                                  |
                                  v
                               GitHub
```

## Process Architecture

Flat process tree — webhook server manages all child processes as siblings:

```
webhook server (FastAPI, persistent)
├── orchestrator-repoA     (Agent SDK, Unix socket IPC)
├── orchestrator-repoB     (Agent SDK, Unix socket IPC)
├── coder-repoA-issue42    (Claude Code / Codex CLI)
├── qa-repoA-pr88          (Claude Code / Codex CLI)
├── bugtracker-repoB-#12   (Claude Code / Codex CLI)
└── ...
```

- **Webhook server**: Always-on, daemonized via launchd (macOS) / systemd (Linux)
- **Orchestrators**: Hybrid lifecycle — start on first event, stay alive while processing, idle timeout shutdown, session persistence for resume
- **Worker agents**: Stateless, fresh CLI spawn per task, exit after completion

## Event Flow (Happy Path)

1. GitHub event fires (e.g., new issue created)
2. Webhook server receives POST, verifies `X-Hub-Signature-256`
3. Routes to correct repo queue via `registry.json`
4. Delivers event as user message turn to repo's orchestrator via Unix socket
5. Orchestrator re-reads `global-prompt.md` + gathers context via scoped tools
6. Orchestrator produces structured output: `[{type: "dispatch", agent_type: "cs", task_context: "..."}]`
7. Webhook server parses agent frontmatter, assembles 4-layer prompt, spawns CLI process
8. Agent works autonomously (gh, git, Playwright, etc.)
9. Agent posts branded status comment on GitHub, exits
10. Status comment triggers new webhook event → cycle repeats

## Git Flow

```
feature/#42 ─── PR ──→ dev ─── PR ──→ main
(agent work)        (auto-merge)     (human review)
```

- All agent PRs target `dev` branch
- Reviewer agent can approve and merge to `dev`
- `dev → main` requires human approval via branch protection
- Per-agent git worktrees for parallel isolation
- Branch naming: `Feature/#145`, `Fix/#203`, etc.

## Technology Stack

| Component | Technology |
|-----------|-----------|
| Webhook server | Python, FastAPI, asyncio |
| CLI | Python, Typer |
| Orchestrator | Claude Agent SDK (abstraction layer for future OpenAI support) |
| Worker agents | Claude Code CLI / Codex CLI |
| IPC | Unix sockets |
| Scheduling | asyncio timers + `~/.githubclaw/scheduled.json` |
| Daemonization | launchd (macOS) / systemd (Linux) |
| Browser testing | Playwright + VLM (Vision Language Model) |
| GitHub API | `gh` CLI with PAT |
| Webhook delivery | GitHub App (webhook only, not for API auth) |
| Tunneling | User's choice (Cloudflare Tunnel, ngrok, etc.) |

## Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Agent execution | Claude Code / Codex CLI | Instruction-following + full tool access, easy to implement |
| Orchestrator SDK | Claude Agent SDK (v1) | Session persistence, context compaction, tool use |
| Inter-agent comms | Async, stateless, GitHub trail | Simplicity — stateful coordination too complex |
| Task lifecycle | No explicit tracking | LLM judgment from accumulated context + GitHub state |
| Behavioral enforcement | Instruction prompts (except security gates) | Complexity/autonomy tradeoff — trust the model |
| Monitoring | GitHub + logs (no dashboard) | GitHub IS the dashboard |
| Prompt iteration | Live on dev branch | No dry-run complexity |
| Upgrades | User customizations always win | User sovereignty |

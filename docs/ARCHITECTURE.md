# GithubClaw Architecture

## Overview

GithubClaw is a system of near-autonomous AI agents that manage open-source projects end-to-end using **GitHub as the single source of truth**. A Rust webhook server receives GitHub events and launches or resumes an orchestrator session, which classifies each event and directly dispatches specialized worker agents via `githubclaw dispatch`.

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
              |    axum + Rust         |  (~/.githubclaw/)
              |                        |
              | - Signature verify     |
              | - Registry routing     |
              | - Serial event queues  |
              | - Process management   |
              | - Prompt assembly      |
              | - Scheduled events     |
              | - Rate limit handling  |
              +------------------------+
                  |
             (CLI spawn)
                  |
                  v
          +-------------------+  githubclaw dispatch  +-------------+
          |  Orchestrator     |---------------------->|   Worker    |
          |  Claude Code /    |                       |   Agents    |
          |  Codex session    |                       | Claude Code |
          |                   |                       |   / Codex   |
          | - Classify        |                       +-------------+
          | - Read state      |                              |
          | - Route           |                        (gh CLI / git)
          +-------------------+                              |
                                                           v
                                                        GitHub
```

## Process Architecture

Flat process tree — webhook server manages all child processes as siblings:

```
webhook server (axum, persistent)
├── orchestrator-repoA     (Claude Code / Codex session)
├── orchestrator-repoB     (Claude Code / Codex session)
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
4. Spawns or resumes the repo's orchestrator session with the latest event prompt
5. Orchestrator re-reads prompts, gathers context via built-in CLI tools, and decides what to do
6. Orchestrator directly calls `githubclaw dispatch <agent> --issue N --prompt "..."`
7. Webhook server parses agent frontmatter, assembles 4-layer prompt, spawns worker CLI process
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
| Webhook server | Rust, axum, tokio |
| CLI | Rust, clap |
| Orchestrator | Claude Code / Codex CLI |
| Worker agents | Claude Code CLI / Codex CLI |
| Scheduling | tokio timers + `~/.githubclaw/scheduled.json` |
| Daemonization | launchd (macOS) / systemd (Linux) |
| Browser testing | Playwright + VLM (Vision Language Model) |
| GitHub API | `gh` CLI with PAT |
| Webhook delivery | GitHub App (webhook only, not for API auth) |
| Tunneling | User's choice (Cloudflare Tunnel, ngrok, etc.) |
| Serialization | serde + serde_yaml + serde_json |
| Config | config (YAML) via serde_yaml |

## Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Agent execution | Claude Code / Codex CLI | Instruction-following + full tool access, easy to implement |
| Orchestrator runtime | Claude Code / Codex CLI session | Reuse battle-tested tools and let the session dispatch directly |
| Inter-agent comms | Async, stateless, GitHub trail | Simplicity — stateful coordination too complex |
| Task lifecycle | No explicit tracking | LLM judgment from accumulated context + GitHub state |
| Behavioral enforcement | Instruction prompts (except security gates) | Complexity/autonomy tradeoff — trust the model |
| Monitoring | GitHub + logs (no dashboard) | GitHub IS the dashboard |
| Prompt iteration | Live on dev branch | No dry-run complexity |
| Upgrades | User customizations always win | User sovereignty |

# GithubClaw

**Near-autonomous AI agents that manage your open-source project end-to-end.**

GithubClaw treats GitHub as the single source of truth. A webhook server receives events, an orchestrator classifies them, and specialized AI agents handle everything from triaging issues to writing code to reviewing PRs — all visible through normal GitHub workflows. You stay in control through branch protection, mission-level configuration, and fully editable agent prompts.

## Quick Start

```bash
cargo install --path .            # install from the current checkout
# or, after crates.io release:
# cargo install githubclaw

cd /path/to/your-repo
githubclaw init                   # scaffold .githubclaw/ directory
```

Then follow the setup steps below.

### 1. Edit your project mission

Open `.githubclaw/VALUE.md` and describe what the project is about. Every agent reads this to align decisions with your goals.

### 2. Set up a public tunnel

GithubClaw needs a public URL so GitHub can deliver webhooks to your local machine. Pick any tunnel service:

```bash
# Option A: Cloudflare Tunnel (recommended, free)
cloudflared tunnel --url http://localhost:8000

# Option B: ngrok
ngrok http 8000
```

Note the public URL (e.g. `https://your-tunnel.trycloudflare.com`).

### 3. Create a GitHub App

1. Go to **GitHub Settings > Developer settings > GitHub Apps > New GitHub App**.
2. Fill in:
   - **GitHub App name**: `GithubClaw` (or any name you like)
   - **Homepage URL**: your repo URL
   - **Webhook URL**: your tunnel URL + `/webhook` (e.g. `https://your-tunnel.trycloudflare.com/webhook`)
   - **Webhook secret**: copy the value from `~/.githubclaw/secrets/webhook_secret` (generated during `githubclaw init`)
3. Under **Permissions**, grant:
   - **Repository permissions**:
     - Issues: Read & Write
     - Pull requests: Read & Write
     - Contents: Read & Write
     - Discussions: Read & Write
     - Projects: Read & Write
     - Checks: Read-only
     - Metadata: Read-only
   - **Organization permissions**: None needed
4. Under **Subscribe to events**, check:
   - Issues, Issue comment
   - Pull request, Pull request review, Pull request review comment
   - Discussion, Discussion comment
   - Check suite, Check run
   - Label, Milestone, Projects v2 item
5. Set **Where can this GitHub App be installed?** to "Only on this account".
6. Click **Create GitHub App**.

### 4. Install the GitHub App

1. After creation, click **Install App** in the sidebar.
2. Choose **Only select repositories** and pick the repo(s) you want GithubClaw to manage.
3. Click **Install**.

### 5. Start the server

```bash
githubclaw start
```

Verify it's running:

```bash
githubclaw status          # show server status + registered repos
curl http://localhost:8000/health   # should return {"status":"ok"}
```

To test the webhook delivery, open an issue on your repo. You should see it appear in the queue and get processed by the orchestrator.

### Hosted proxy registration

If you run GithubClaw behind a hosted proxy, set the GitHub App setup URL to:

```text
https://your-hosted-proxy.example.com/setup/claim-proof
```

After GitHub redirects back with `installation_id`, mint a proof and register the tunnel:

```bash
curl "https://your-hosted-proxy.example.com/setup/claim-proof?installation_id=123456"

curl -X POST "https://your-hosted-proxy.example.com/register" \
  -H "Content-Type: application/json" \
  -d '{
    "installation_id": 123456,
    "tunnel_url": "https://abc123.trycloudflare.com",
    "claim_proof": "cp_v1.opaque_proof_id.opaque_secret"
  }'
```

Persist the returned `update_secret`. Every later tunnel update must send that secret to `/register`, and every successful update rotates it.

## How It Works

```
                       GitHub
                         |
                   (webhook events)
                         |
                         v
            +------------------------+
            |    Webhook Server      |
            |    axum + tokio        |
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
        | Codex /      |  | Codex /     |
        | Claude Code  |  | Claude Code |
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

- Rust 1.75+ (`cargo install --path .` from this repo, or `cargo install githubclaw` after crates.io release)
- `gh` CLI (authenticated)
- Claude Code or Codex CLI
- A tunnel service (Cloudflare Tunnel, ngrok, etc.)

## License

MIT

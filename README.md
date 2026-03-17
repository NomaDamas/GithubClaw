# GithubClaw

**Near-autonomous AI agents that manage your open-source project end-to-end.**

GithubClaw treats GitHub as the single source of truth. A webhook server receives events, an orchestrator classifies them, and specialized AI agents handle everything from triaging issues to writing code to reviewing PRs — all visible through normal GitHub workflows. You stay in control through branch protection, mission-level configuration, and fully editable agent prompts.

## Quick Start

```bash
cargo install --path .            # install from the current checkout
# or, after crates.io release:
# cargo install githubclaw

cd /path/to/your-repo
githubclaw init                   # initialize ~/.githubclaw for this repo
```

Then follow the setup steps below.

### 1. macOS only: grant Full Disk Access to your terminal app

The most reliable way to run GithubClaw on macOS is to grant **Full Disk Access** to Terminal or iTerm, then use `githubclaw start`.

On macOS, `githubclaw start` uses a tmux-backed `githubclaw serve` session behind the scenes instead of relying on `launchd` as the default runtime.

Recommended setup:

1. In `System Settings -> Privacy & Security -> Full Disk Access`, enable access for Terminal or iTerm.
2. Keep managed repositories and worktrees in an unprotected path such as `~/Projects` when possible.
3. Start the server:

```bash
githubclaw start
```

If you use the project `Makefile`, the equivalent is still:

```bash
make start
```

Useful tmux commands:

```bash
tmux attach -t githubclaw
tmux ls
```

If you want to start the tmux session explicitly yourself, you can still run:

```bash
make serve-tmux
```

### 2. Edit your project mission

Open `~/.githubclaw/repos/<owner_repo>/VALUE.md` and describe what the project is about. Every agent reads this to align decisions with your goals.

### 3. Set up a public tunnel

GithubClaw needs a public URL so GitHub can deliver webhooks to your local machine. Pick any tunnel service:

```bash
# Option A: Cloudflare Tunnel (recommended, free)
cloudflared tunnel --url http://localhost:8000

# Option B: ngrok
ngrok http 8000
```

Note the public URL (e.g. `https://your-tunnel.trycloudflare.com`).

### 4. Create a GitHub App

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

### 5. Install the GitHub App

1. After creation, click **Install App** in the sidebar.
2. Choose **Only select repositories** and pick the repo(s) you want GithubClaw to manage.
3. Click **Install**.

### 6. Start the server

On macOS:

```bash
githubclaw start
```

Or with `make`:

```bash
make start
```

On Linux, use the background daemon:

```bash
githubclaw start
```

Verify it's running:

```bash
tmux ls                    # macOS: confirm the githubclaw session exists
githubclaw status          # macOS tmux mode or Linux daemon mode
curl http://localhost:8000/health   # should return {"status":"ok"}
```

To test the webhook delivery, open an issue on your repo. You should see it appear in the queue and get processed by the orchestrator.

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
                |
           (CLI spawn)
                |
                v
        +-----------------------+
        | Orchestrator Session  |
        | Claude Code / Codex   |
        |                       |
        | - inspect context     |
        | - classify event      |
        | - call dispatch CLI   |
        +-----------------------+
                |
          githubclaw dispatch
                |
                v
        +-------------+
        |   Worker    |
        |   Agents    |
        | Codex /     |
        | Claude Code |
        +-------------+
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
| **Orchestrator** | Classify issues, analyze context, dispatch the next agent via `githubclaw dispatch` | Every actionable GitHub event |
| **Bug Reproducer** | Reproduce and diagnose bugs without fixing them | Bug issues |
| **Vision-gap Analyst** | Analyze feature and refactoring requests before human approval | Feature and refactoring issues |
| **Verifier** | Write tests first and perform end-to-end validation | Before implementation and before merge |
| **Implementer** | Write code to satisfy the verified plan and tests | Approved implementation work |
| **Reviewer** | Review PRs and gate progress to `dev` | After implementation updates |

## CLI Commands

| Command | Description |
|---------|-------------|
| `githubclaw init` | Initialize global profile/repo/runtime layout in `~/.githubclaw` |
| `githubclaw start` | Start the webhook server using the recommended runtime for this OS; on macOS this starts a tmux-backed session |
| `githubclaw serve` | Run the webhook server inline in the current shell |
| `githubclaw stop` | Stop the webhook server in daemon mode or stop the recommended tmux-backed macOS runtime |
| `githubclaw stop --force` | Immediate kill |
| `githubclaw status` | Show server status and registered repos for daemon or tmux mode |
| `githubclaw logs` | Tail daemon logs or attach to tmux-backed inline output |

## Directory Layout

```
~/.githubclaw/
├── config.yaml
├── registry.json
├── profiles/
│   └── default/
│       ├── orchestrator.md
│       ├── global-prompt.md
│       └── agents/
├── repos/
│   └── owner_repo/
│       ├── VALUE.md
│       ├── memory.md
│       ├── config.yaml
│       └── agents/
└── runtime/
    └── owner_repo/
        ├── queue/
        ├── logs/
        └── dispatch_receipts/
```

GithubClaw now keeps both configuration and runtime state under `~/.githubclaw/`.

## Configuration

- **Profiles** -- Shared prompts and agent definitions live under `~/.githubclaw/profiles/<profile>/`.
- **Repo overrides** -- Per-repo VALUE, memory, config, and agent overrides live under `~/.githubclaw/repos/<owner_repo>/`.
- **Runtime data** -- Queue, logs, receipts, and other operational artifacts live under `~/.githubclaw/runtime/<owner_repo>/`.
- **Agent spawning** -- `claude-code` and `codex` use built-in Rust launch paths in the MVP. Repo-local spawn script overrides are not part of the runtime contract.

## Architecture

All agent PRs target `dev`. The Reviewer agent can approve and merge to `dev`, but `dev -> main` always requires human approval via branch protection.

For detailed specs, see:

- `docs/ARCHITECTURE.md` -- Current system design and runtime model

## Requirements

- Rust 1.75+ (`cargo install --path .` from this repo, or `cargo install githubclaw` after crates.io release)
- `gh` CLI (authenticated)
- Claude Code or Codex CLI
- A tunnel service (Cloudflare Tunnel, ngrok, etc.)

## License

MIT

# GithubClaw V2 Architecture

> Complete redesign: Issue Request pipeline, 6-agent system, Claude Code subprocess orchestration.

## Design Principles

1. **GitHub is SSOT** — All state transitions recorded as GitHub comments with HTML markers
2. **GitHub event-driven** — Webhook events trigger all automated transitions; minimize custom signaling
3. **Subscription-first** — Use Claude Code / Codex CLI subscriptions (OAuth), not direct API calls
4. **Operator sovereignty** — TUI for private operator control; GitHub for public-facing interaction
5. **Stand on giants' shoulders** — Reuse GitHub infrastructure (labels, sub-issues, branch protection) over custom builds

## Agent System (6 Types)

### Orchestrator
- **Runtime**: Claude Code subprocess (`--resume` pattern)
- **Lifecycle**: One session per root issue (including sub-issues)
- **Session ID**: Persisted at `~/.githubclaw/sessions/<repo>/<issue-id>/session_id`
- **Responsibilities**:
  - Classify incoming issues (Bug / Feature / Refactoring)
  - Decompose issues into GitHub sub-issues (autonomous, post-approval)
  - Dispatch agents via `githubclaw dispatch <agent> --issue N --prompt "..."`
  - Interpret human feedback on `stuck` situations and decide recovery path
- **Lifecycle pattern**: Start on event → dispatch agents → exit → resume on marker webhook

### Implementer
- **Runtime**: Claude Code / Codex subprocess (spawned by GithubClaw server)
- **Responsibilities**: Write code, open PRs, fix CI failures
- **Loop**: Unlimited retries until Verifier tests pass; max 10 rounds with Reviewer

### Verifier
- **Runtime**: Claude Code / Codex subprocess
- **Responsibilities**:
  - Write test code (TDD — tests before implementation)
  - Post-implementation e2e validation: "use the product as a user would"
    - Web: Playwright, click through, screenshots, Chrome DevTools
    - API: curl with real data
    - CLI: execute and verify output
  - e2e test format specified per-project in `RepoConfig`

### Reviewer
- **Runtime**: Claude Code / Codex subprocess
- **Responsibilities**:
  - Code review with priority: (1) JTBD / problem resolution from Issue Request, (2) clean code, edge cases, security (project-configurable)
  - Pass: post `reviewed` marker; Fail: comment with feedback, no marker (loop continues)
  - Security issues treated as normal review failures (no special path)

### Vision-gap Analyst
- **Runtime**: Claude Code / Codex subprocess
- **Trigger**: Auto-executed when issue classified as Feature or Refactoring
- **Input**: `.githubclaw/VALUE.md` (project vision/philosophy)
- **Output**: Analysis report on vision alignment; does NOT have reject authority
- **Report included in**: Human escalation context for Interactive Session

### Bug Reproducer
- **Runtime**: Claude Code / Codex subprocess
- **Responsibilities**:
  - Reproduce bugs in isolated Docker containers (Linux, macOS, Windows)
  - Manage OS-specific base setups (docker-compose)
  - Generate structured reproduction report
- **Report schema**:
  ```
  reproduced: bool
  environment: { os, docker_image, deps }
  reproduction_steps: [commands]
  stack_trace: string (optional)
  minimal_script: string
  analysis: string (root cause hypothesis)
  ```
- **Retry**: Max 3 additional info requests to reporter; close issue after 3 failures

### Removed Agents
PM, Marketer, CS, Librarian, Security Reviewer — roles absorbed or eliminated.

## Issue Request Pipeline

### Classification
Every incoming issue is classified into one of three groups:

| Category | Auto-approve? | Must answer |
|----------|--------------|-------------|
| **Bug** | Yes, if reproduced | Can it be reproduced? Root cause? Fix approach? |
| **Feature** | No — human escalation | What problem does it solve? When is it beneficial? Aligns with vision? |
| **Refactoring** | No — human escalation | All features maintained? Why refactor? What benefit? Exactly what/how? |

**Key principle**: Feature specifies GOAL only (no HOW). Bug and Refactoring specify both GOAL and HOW.

### Problem Concretization
Before any issue is approved, the "real problem" must be fully defined using:
- **Problem Framing**: Who is affected, in what context, what causes friction?
- **JTBD (Jobs to be Done)**: What is the user actually trying to accomplish?
- **5 Whys**: What is the root cause?

Example: "Add dark mode toggle" → Real problem: "Current UI reduces readability in low-light environments"

### Bug Flow
```
Issue received → Orchestrator classifies as Bug
  → Bug Reproducer: attempt reproduction in Docker containers
    → reproduced=true:
        → Orchestrator posts `approved` marker → implementation pipeline
    → reproduced=false:
        → Request additional info from reporter (max 3 times)
        → Still fails after 3 attempts → close issue with reason
```

### Feature / Refactoring Flow
```
Issue received → Orchestrator classifies as Feature/Refactoring
  → Vision-gap Analyst auto-executes → report ready
  → Issue enters "awaiting interactive session" state
  → Operator opens TUI → Interactive Session:
      - Orchestrator presents analysis (classification, Problem Framing, JTBD, 5 Whys)
      - Operator provides feedback, direction correction
      - Iterate until all required questions answered
      - Operator approves/rejects
  → Approved: final report posted as GitHub comment with `approved` marker
  → Rejected: issue closed with reason
  → `approved` webhook triggers implementation pipeline
```

## Implementation Pipeline

### Sequence
```
1. Orchestrator creates git worktree (per root issue, branch: Feature/#123)
2. Orchestrator decomposes into GitHub sub-issues (autonomous)
3. For each sub-issue (sequential):
   a. Verifier → write test code
   b. Implementer → implement (loop until tests pass, unlimited)
   c. Reviewer → code review (max 10 rounds)
      - Fail: Implementer fixes → tests must re-pass → re-review
      - Pass: `reviewed` marker posted
   d. Next sub-issue
4. All sub-issues complete → Verifier → e2e validation (project-specific)
5. `verified` marker → feature branch → dev auto-merge (no squash, history preserved)
```

### Loop Limits
| Loop | Limit | On exceed |
|------|-------|-----------|
| Implementer ↔ Verifier tests | 10 | `stuck` marker + human escalation |
| Implementer ↔ Reviewer | 10 | `stuck` marker + human escalation |

### Stuck Recovery
1. GithubClaw posts `stuck` marker on PR + label
2. Operator provides direction via PR comment (visible in TUI Monitoring tab)
3. Webhook re-enters Orchestrator (`--resume`)
4. Orchestrator interprets human comment → decides recovery path:
   - Restart Implementer with new approach
   - Modify Verifier tests
   - Re-decompose sub-issues
   - Any other path deemed appropriate
5. Loop counter resets

### Parallelism
- **Between issues**: Parallel (independent worktrees)
- **Between sub-issues**: Sequential (within same root issue branch)

## GitHub HTML Marker System

All agent-to-system communication uses HTML comments in GitHub issue/PR comments.

### State Transition Markers
```html
<!-- githubclaw:approved -->
<!-- githubclaw:reproduced reproduced=true -->
<!-- githubclaw:reproduced reproduced=false -->
<!-- githubclaw:reviewed -->
<!-- githubclaw:verified -->
<!-- githubclaw:stuck -->
```

### Summary Markers
Every agent comment MUST include:
```html
<!-- githubclaw:summary -->
Structured summary content here...
<!-- /githubclaw:summary -->
```
GithubClaw parses this section for Orchestrator resume messages.

### Rejection
No marker needed — close the issue with a reason comment.

### Ref Injection
All GitHub write operations automatically include `ref #N` (root issue number).
Enforced at system level via `gh` wrapper script (not prompt-based).

## Orchestrator Lifecycle (Claude Code Subprocess)

### Event → Dispatch → Resume Pattern
```
1. Webhook event arrives → GithubClaw server enqueues
2. GithubClaw spawns Orchestrator Claude Code subprocess
   - New issue: fresh session
   - Existing issue: --resume with session_id
3. GithubClaw passes normalized message as prompt:
   {
     event_type: "marker_detected",
     marker: "verified",
     issue: 123,
     comment_url: "...",
     agent: "verifier",
     summary: "..." (extracted from githubclaw:summary tags)
   }
4. Orchestrator processes → dispatches agents via `githubclaw dispatch`
5. Orchestrator exits (fire-and-forget)
6. Agent works → posts marker on GitHub → webhook fires
7. GithubClaw resumes Orchestrator with new normalized message
8. Repeat until issue resolved
```

### Interrupt Handling
New webhook events for an in-progress issue:
- Wait for current Orchestrator turn to complete
- Append new event as next `--resume` prompt

## Infrastructure

### gh Wrapper Script
- Injected at PATH front by `spawner.rs`
- Reads `GITHUBCLAW_ROOT_ISSUE` environment variable
- Auto-appends `ref #N` to all write commands (`issue comment`, `pr create`, `pr comment`, etc.)
- Enforces `DENIED_PATHS` security boundary
- Delegates to real `gh` binary

### Dispatch CLI
```bash
githubclaw dispatch <agent_type> --issue <N> --prompt "<instructions>"
```
- Orchestrator calls this via Bash tool
- GithubClaw server receives request → spawns agent subprocess
- Central process management: tracking, timeout, kill, concurrency limits
- Agent reads GitHub context (issues, comments) independently via gh CLI

### Release CLI
```bash
githubclaw release
```
- Triggered by operator (TUI or terminal)
- Creates release branch from dev
- Spawns Orchestrator to analyze changes → generate dogfooding checklist → create release→main PR
- Prepares dogfooding environment
- Final merge: operator approves in GitHub web UI (branch protection, GithubClaw does NOT merge)

### Rate Limiting (Reused from V1)
| Tier | Trigger | Response |
|------|---------|----------|
| **Tier 1: WorkerLimited** | Worker agent non-zero exit | Pause new dispatches |
| **Tier 2: OrchestratorLimited** | Orchestrator subprocess non-zero exit | Stop event feeding |
| **Tier 3: FullHibernate** | Both triggered | All processing paused, operator alert |

- Recovery: `gh api rate_limit` probe every 5 minutes
- Detection adapted: "API failure" → "subprocess exit code"

### Session Persistence
```
~/.githubclaw/sessions/<repo>/<issue-id>/session_id
```

### TUI (ratatui + tokio)

All UI text MUST be in English.

#### Issue Request Tab
- Left panel: Issues awaiting interactive session (Feature/Refactoring only; Bugs auto-processed)
- Right panel: **Embedded Claude Code / Codex interactive session** (PTY-based)
  - Real terminal session, not a chat widget — operator interacts as if running `claude` directly
  - Session includes: Orchestrator analysis, Vision-gap Analyst report, issue context
  - Operator discusses, refines problem definition, approves/rejects
- Keybindings: `j/k` navigate issues, `Enter` open session, `Ctrl+a` approve, `Ctrl+r` reject

#### Monitoring Tab
- Left panel: Active agent sessions + rate limit status + queue depth
- Right panel: **Session detail with full agent execution history**
  - Shows all agents that have run for the selected issue (completed + currently running)
  - Timeline view: `Bug Reproducer ✓ → Verifier ✓ → Implementer (running 3/10) → Reviewer (queued)`
  - Live log tail for currently running agent
  - `[c]` keybinding to compose PR comment for mid-process feedback

#### Release Tab
- Single-pane view:
  - Release branch status, included issues, PR link
  - Dogfooding checklist (generated by Orchestrator)
  - **`[r]` Run app**: Executes project-specific launch script (`RepoConfig.dogfood_command`) to start the built app directly for manual dogfooding
  - `[o]` Open PR in browser
  - `[g]` Run `githubclaw release` to initiate release pipeline

#### General
- Keyboard-centric with mouse support (click to select issues, tabs)
- Integrated into `githubclaw` binary (`githubclaw tui`)

## Configuration Changes

### RepoConfig Additions
- `e2e_test_format`: Project-specific e2e validation configuration
- `reviewer_priorities`: Ordered list of review focus areas (2nd priority, after JTBD)
- `dogfood_command`: Shell command to launch the app for manual dogfooding (e.g., `cargo run --release`, `npm start`, `docker-compose up`)

### Removed
- `orchestrator.md` system prompt (no longer custom API calls)
- 12 scoped tool definitions in `tools.rs` (Claude Code has built-in tools)
- `OrchestratorSession` struct and Anthropic API client

## Migration Strategy

- **Full rewrite** (not incremental)
- Existing 164 tests: most will be rewritten
- Execution via `/ouroboros:ralph` spec-driven loop

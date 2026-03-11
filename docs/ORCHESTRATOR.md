# Orchestrator

The orchestrator is a per-repo Rust process that calls the Anthropic API directly with an agentic tool loop, running as a child process of the webhook server. It is the **pure decision-maker** — it classifies events, gathers context, and produces structured dispatch output. It never executes dispatches, spawns agents, or performs destructive actions.

## Architecture

- **Runtime**: Direct Anthropic API client (Rust, reqwest) with agentic tool loop — call model, execute tools, repeat until final structured output
- **Process**: Separate child process per repo, managed by webhook server
- **IPC**: Unix socket at `/tmp/githubclaw-{repo_name}.sock`
- **Lifecycle**: Hybrid — start on first event, stay alive while processing, idle timeout shutdown, session persistence for resume

## Communication Flow

```
Webhook Server                     Orchestrator
     |                                  |
     |── event JSON ──────────────────> |  (via Unix socket)
     |                                  |
     |                                  |  1. Re-read global-prompt.md
     |                                  |  2. Gather context (scoped tools)
     |                                  |  3. Classify event
     |                                  |  4. Produce structured output
     |                                  |
     |<── structured output JSON ────── |
     |                                  |
     (execute actions)                  (wait for next event)
```

## Scoped Custom Tools

The orchestrator has **no raw shell access**. Only these registered custom functions:

| Tool | Description | Access |
|------|-------------|--------|
| `get_issue(number)` | Get issue details | Read-only |
| `list_issues(state, labels, since)` | List issues | Read-only |
| `get_pr(number)` | Get PR details | Read-only |
| `list_prs(state)` | List PRs | Read-only |
| `get_pr_diff(number)` | Get PR diff | Read-only |
| `get_discussion(number)` | Get Discussion details | Read-only |
| `get_ci_status(run_id)` | Get CI run status | Read-only |
| `search_issues(query)` | Search issues | Read-only |
| `read_file(path)` | Read file from filesystem | Scoped to repo + `.githubclaw/` |
| `read_memory(section)` | Read memory section | `.githubclaw/memory.md` |
| `write_memory(section, content)` | Write memory section | `.githubclaw/memory.md` only |
| `web_search(query)` | Search the web | Read-only |

All GitHub query tools are implemented as Rust functions calling `gh` CLI internally via `tokio::process::Command`. `read_file` enforced via whitelist — repo directory + `.githubclaw/` only, excludes `~/.githubclaw/secrets/`, `~/.ssh/`, etc. User can expand allowed paths in `.githubclaw/config.yaml` (orchestrator cannot modify this config).

## Structured Output Schema

Every orchestrator response conforms to a heterogeneous action list:

```json
{
  "actions": [
    {
      "type": "dispatch",
      "agent_type": "coder",
      "issue_ref": "#42",
      "task_context": "Fix null check in auth.py — see Bug Tracker analysis in issue comments"
    },
    {
      "type": "schedule_event",
      "event_id": "evt_check_42",
      "trigger_at": "2026-03-10T16:00:00Z",
      "repo": "owner/repo",
      "payload": {"type": "follow_up", "issue_ref": "#42"},
      "context": "Check if Coder completed the fix"
    },
    {
      "type": "cancel_event",
      "event_id": "evt_old_123"
    }
  ],
  "reasoning": "Bug #42 confirmed by Bug Tracker. Dispatching Coder for fix, scheduling follow-up in 2 hours."
}
```

### Action Types

| Type | Fields | Description |
|------|--------|-------------|
| `no_action` | `reasoning` | Do nothing (singleton, mutually exclusive with others) |
| `dispatch` | `agent_type`, `issue_ref`, `task_context` | Dispatch a worker agent |
| `schedule_event` | `event_id`, `trigger_at`, `repo`, `payload`, `context` | Schedule future synthetic event |
| `cancel_event` | `event_id` | Cancel a previously scheduled event |

- Multiple actions can be combined in one response (except `no_action`)
- Webhook server processes each action independently (best-effort, no rollback)
- Invalid `agent_type` triggers error feedback synthetic event for self-correction

## System Prompt

Lives in `.githubclaw/orchestrator.md` — user-editable. Contains:

- **Classification principles**: High-level routing guidance
- **Few-shot examples**: Event → action output examples
- **Workflow templates**: General progression patterns (bug lifecycle, feature lifecycle)
- **Rules**: "Always verify current state before acting", "Respect direct human requests with high priority"

Baked into the Anthropic API system prompt at session creation. On session resume after idle timeout, recency bias from dynamic file re-reads overrides any stale system prompt content.

## Dynamic Re-reads

On **every event**, before classification:

| File | Purpose |
|------|---------|
| `.githubclaw/global-prompt.md` | Current agent roster (names, roles, handoff conventions) |
| `.githubclaw/memory.md` (selective) | Relevant institutional knowledge sections |

These ensure the orchestrator always has the latest roster and accumulated wisdom, even after session compaction.

## Long-term Memory

`.githubclaw/memory.md` — structured by topic sections:

```markdown
## Contributors
- contributor-x: PRs often need extra QA scrutiny, tends to skip tests
- contributor-y: reliable, usually touches payments module

## Recurring Bugs
- auth.py token expiry: has broken 3 times, approach carefully
- CSS layout in Safari: recurring, check with QA Playwright

## Architectural Patterns
- All API endpoints go through middleware chain in middleware.py
- Database migrations must be backwards-compatible

## Workflow Heuristics
- Large PRs (>500 lines) benefit from PM sub-task breakdown first
- Issues mentioning "payments" tend to cascade into 3-4 related bugs
```

- `read_memory("Contributors")` reads only that section
- `write_memory("Recurring Bugs", "...")` updates only that section
- Survives session compaction and restarts
- Other agents can also read it for context

## Context Gathering

The orchestrator gathers context for its **own classification decision**, not to relay to agents:

1. Read `global-prompt.md` for current roster
2. Read relevant `memory.md` sections
3. Query GitHub via scoped tools (`get_issue`, `get_pr`, `get_ci_status`, etc.)
4. Check current repo state via `read_file` if needed

Task context in dispatch output is **minimal** — just a pointer and brief summary. Agents independently read full details via `gh`/`git`.

## Error Handling

| Scenario | Response |
|----------|----------|
| Invalid agent_type output | Webhook server injects corrective feedback event |
| Tool call failure (gh timeout, etc.) | Event re-queued, retried up to `max_retry` |
| Max retry exceeded | Event dead-lettered, logged |
| Rate limit hit | Webhook server enters hibernate, queues events to disk |
| Session crash | Webhook server detects exit code, cold-starts on next event with session resume |

## Orchestrator vs. Worker Agents

| Aspect | Orchestrator | Worker Agents |
|--------|-------------|---------------|
| Runtime | Anthropic API + agentic tool loop (Rust) | Claude Code / Codex CLI |
| Lifecycle | Long-running, stateful | Stateless, per-task |
| Shell access | None (scoped custom tools) | Full (YOLO mode) |
| Output | Structured JSON actions | GitHub comments, PRs, commits |
| Memory | Accumulated context + memory.md | Fresh each spawn, reads GitHub |
| Managed by | Webhook server | Webhook server |

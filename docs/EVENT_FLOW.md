# Event Flow

## Event Lifecycle

```
GitHub Event
    │
    ▼
[Webhook Server: Signature Verify]
    │
    ├── Invalid signature → Reject (403)
    ├── Repo not in registry → Discard + log
    │
    ▼
[Webhook Server: Fork PR Check]
    │
    ├── Fork PR without label → Queue with "unapproved fork" annotation
    │
    ▼
[Disk-Persisted Queue]
    │
    ▼
[Webhook Server: Deliver to Orchestrator via Unix Socket]
    │
    ├── Orchestrator idle → Cold-start, resume session
    │
    ▼
[Orchestrator: Process Event]
    │
    ├── Re-read global-prompt.md (roster)
    ├── Read relevant memory.md sections
    ├── Gather context (scoped tools)
    ├── Classify event
    │
    ▼
[Orchestrator: Structured Output]
    │
    ├── no_action → Log reasoning, done
    ├── dispatch → Webhook server spawns agent(s)
    ├── schedule_event → Webhook server persists to scheduled.json
    ├── cancel_event → Webhook server removes from scheduled.json
    │   (multiple actions can be combined)
    │
    ▼
[Webhook Server: Execute Dispatches]
    │
    ├── Validate agent_type against .githubclaw/agents/
    │   ├── Not found → Error feedback event to orchestrator
    │   │
    ├── Check max_concurrent_agents
    │   ├── Limit reached → Queue dispatch, execute when slot frees
    │   │
    ├── Check fork PR gate (for execution-capable agents)
    │   ├── Blocked → Error feedback event to orchestrator
    │   │
    ├── Parse frontmatter (YAML)
    ├── Assemble 4-layer prompt
    ├── Write temp file
    ├── Spawn CLI process
    │
    ▼
[Worker Agent: Execute Task]
    │
    ├── Verify current state (gh/git)
    ├── Create worktree from dev
    ├── Perform work
    ├── Post branded status comment on GitHub
    ├── Update Projects board
    ├── Clean up worktree
    ├── Exit
    │
    ▼
[Webhook Server: Monitor Exit]
    │
    ├── Exit code 0 → Clean up temp files
    ├── Non-zero exit → Inject failure event to orchestrator queue
    ├── Timeout (2h) → Kill, inject failure event
    │
    ▼
[Agent's GitHub Activity Triggers New Webhook Events]
    │
    ▼
[Cycle repeats]
```

## Workflow Examples

### Bug Fix (End-to-End)

```
1. Human files issue: "App crashes when token expires"
   Event: issues.opened

2. Orchestrator → dispatch CS
   CS labels as "bug", replies: "Thanks for reporting, investigating."
   Events: issue_comment.created, issues.labeled

3. Orchestrator → dispatch Bug Tracker
   Bug Tracker reproduces crash, finds null check missing in auth.py:203
   Posts analysis as issue comment
   Event: issue_comment.created

4. Orchestrator → dispatch Coder
   Coder creates worktree (Fix/#42), adds null check, opens PR to dev
   Events: pull_request.opened

5. CI runs automatically (GitHub Actions)
   Event: check_run.completed (success)

6. Orchestrator → dispatch QA
   QA runs test suite + Playwright E2E, verifies fix
   Posts results on PR
   Event: pull_request.comment

7. Orchestrator → dispatch Reviewer
   Reviewer approves + merges to dev
   Events: pull_request_review.submitted, pull_request.closed (merged)

8. Orchestrator → dispatch Librarian (if doc update needed)
   Librarian opens doc-update PR

9. Human reviews dev → main PR when ready
```

### Fork PR (Security Flow)

```
1. External contributor opens PR from fork
   Event: pull_request.opened (head.repo.fork = true)

2. Webhook server detects fork PR, queues event

3. Orchestrator → dispatch Security Reviewer (read-only)
   Security Reviewer audits diff, posts checklist report on PR
   Event: issue_comment.created (PR comment)

4. Human maintainer reviews security report
   Applies "githubclaw-approved" label
   Event: pull_request.labeled

5. Webhook server verifies label in payload → gate lifts

6. Orchestrator → dispatch QA, Reviewer (normal flow)
   Standard review cycle proceeds
```

### Proactive Visionary (Cron)

```
1. asyncio timer fires daily at configured time
   Synthetic event injected into queue

2. Orchestrator → dispatch Visionary
   Visionary reads orchestrator logs for daily activity
   Queries GitHub for additional details
   Posts summary + proposals to "Roadmap" Discussion
   Event: discussion.created

3. Human replies with feedback
   Event: discussion_comment.created

4. Orchestrator classifies → may dispatch Visionary again, or PM for action items
```

### CI Failure Fix

```
1. Coder opens PR, CI fails
   Event: check_run.completed (conclusion: failure)

2. Orchestrator → dispatch Coder
   Task context: "CI failed on PR #88, run_id 12345. Use gh run view 12345 --log-failed"

3. Coder reads failure logs, fixes code, pushes new commits
   Event: check_run.completed (triggers again)

4. If pass → QA dispatched
   If fail again → Coder re-dispatched (orchestrator detects loop → escalate if needed)
```

### Content Marketing

```
1. Notable feature PR merged to dev
   Event: pull_request.closed (merged)

2. Orchestrator → dispatch Marketer
   Marketer drafts tweet about new feature
   Posts to Discussion ("Content Drafts") with metadata:
     "Platform: X/Twitter | Draft below | Reply 'approved' to publish"
   Event: discussion.created

3. Human reviews draft, maybe edits, replies "approved"
   Event: discussion_comment.created

4. Orchestrator → dispatch Marketer (re-spawn)
   Marketer reads Discussion thread, uses human-revised version
   Publishes to X/Twitter via API
   Posts confirmation comment on Discussion
```

## Synthetic Event Types

Events generated internally (not from GitHub webhooks):

| Type | Source | Purpose |
|------|--------|---------|
| `virtual_bootstrap` | Bootstrap scan | Process existing issues/PRs on first start |
| `scheduled_fired` | asyncio timer | Cron events (Visionary daily, orchestrator follow-ups) |
| `error_feedback` | Webhook server | Invalid agent_type correction, fork PR gate rejection |
| `failure_injection` | Process monitor | Agent crash/timeout notification |
| `rate_limit_recovery` | Recovery probe | "Are we back?" test event |

All synthetic events enter the same serial queue as real webhook events. The orchestrator processes them identically — classification from scratch, no special handling.

## Event Subscription (Default)

```yaml
# .githubclaw/config.yaml
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

Excluded by default: `push`, `star`, `fork`, `watch`, `deployment`, `release`, `member`, `repository`, `package`. User can add/remove via config.

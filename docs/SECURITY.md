# Security

## Security Model

GithubClaw's security model balances full agent autonomy (YOLO mode) with mechanical enforcement at critical boundaries. The philosophy: **trust the model for behavioral constraints, enforce mechanically where consequences are irreversible.**

## Defense Layers

```
Layer 1: Webhook Signature Verification
        (rejects forged payloads)
            │
Layer 2: Fork PR Mechanical Gate
        (blocks untrusted code execution)
            │
Layer 3: Orchestrator Tool Scoping
        (no shell, read-only, whitelisted paths)
            │
Layer 4: Security Reviewer Agent
        (read-only audit of fork PR diffs)
            │
Layer 5: Human Approval Labels
        (githubclaw-approved for forks, branch protection for main)
            │
Layer 6: Git Flow Buffer
        (dev branch absorbs mistakes, main requires human review)
            │
Layer 7: Agent Instructions
        (behavioral constraints via prompts)
```

## Webhook Signature Verification

Every incoming POST to the webhook endpoint is verified:

```rust
use hmac::{Hmac, Mac};
use sha2::Sha256;

fn verify_signature(payload_body: &[u8], signature_header: &str, secret: &str) -> bool {
    let Some(hex_sig) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(payload_body);
    let Ok(sig_bytes) = hex::decode(hex_sig) else {
        return false;
    };
    mac.verify_slice(&sig_bytes).is_ok()
}
```

- Webhook secret stored in `~/.githubclaw/secrets/webhook_secret`
- Set during GitHub App creation
- Requests without valid `X-Hub-Signature-256` are rejected (403)

## Hosted Proxy Registration Boundary

The hosted-proxy registration endpoints have a separate narrow trust boundary:

- the first claim uses a short-lived single-use `claim_proof` bound to one `installation_id`
- `GET /setup/install/callback` is the trusted issuance path for that proof after the GitHub App setup redirect
- success returns an `update_secret` scoped to that same `installation_id`
- later updates authenticate by presenting the latest `update_secret`
- every successful update rotates the secret immediately
- replayed, expired, or mismatched proofs must fail

This boundary is intentionally minimal. It bootstraps installation registration only; it does not replace webhook HMAC verification, fork gating, orchestrator isolation, or branch protection.

See [HOSTED_PROXY.md](/Users/jeffrey/Projects/GithubClaw-worktrees/issue-11-hosted-claim-proof/docs/HOSTED_PROXY.md) for the exact setup callback and `POST /register` contract.

## Fork PR Gate

**Mechanical enforcement** — not dependent on LLM judgment:

### Detection (from webhook payload)
```rust
let head_repo = &payload.pull_request.as_ref().unwrap().head.repo;
let is_fork = head_repo.as_ref().map_or(false, |r| r.fork);
let is_approved = payload.pull_request.as_ref().unwrap()
    .labels.iter().any(|l| l.name == "githubclaw-approved");
```

### Enforcement
- Fork PR detected + no approval label → Security Reviewer (read-only) dispatched
- If orchestrator attempts to dispatch execution-capable agent on unapproved fork PR → **blocked by webhook server**, corrective feedback injected
- `githubclaw-approved` label applied by human after reviewing Security Reviewer's report → gate lifts, normal workflow proceeds

### Security Reviewer Checklist

The Security Reviewer agent performs a structured audit with explicit categories:

1. **Secret exfiltration**: Code reading env vars, `~/.githubclaw/secrets/`, or posting data to external URLs
2. **Agent definition tampering**: Modifications to `.githubclaw/` files (agent prompts, config, orchestrator.md)
3. **Obfuscated shell commands**: Base64-encoded commands, eval(), string concatenation constructing shell payloads
4. **Dependency hijacking**: Version bumps to packages with known compromises, new dependencies from untrusted sources
5. **CI/CD manipulation**: Changes to workflow files, build scripts, or deployment configs
6. **Unicode tricks**: Bidirectional override characters, homoglyph attacks that make code appear different than it executes
7. **General catch-all**: Any other suspicious patterns not covered above

Output: Per-category pass/fail + reasoning, plus any additional concerns.

## Orchestrator Isolation

The orchestrator is the most privileged LLM component — it sees all events and decides all routing. Its isolation is critical:

### No Raw Shell Access
Custom tools only. Cannot execute arbitrary commands.

### Read-Only GitHub Tools
High-level functions (`get_issue`, `list_prs`, etc.) — no raw `gh` CLI access that could execute mutations.

### Filesystem Whitelist
`read_file` scoped to:
- Target repo directory
- `.githubclaw/` within the repo

Explicitly excluded:
- `~/.githubclaw/secrets/`
- `~/.ssh/`
- `~/.aws/`
- Any path outside the whitelist

User can expand via `allowed_read_paths` in `.githubclaw/config.yaml`. Orchestrator **cannot modify** this config (no `write_file` tool, no shell).

### Write Scope
Only `write_memory` — limited to `.githubclaw/memory.md`.

### Structured Output Only
Dispatch decisions flow through the webhook server's execution layer. The orchestrator cannot directly spawn processes, make API calls, or perform any side-effecting action beyond memory writes.

## Worker Agent Risks (Accepted)

Worker agents run in **full YOLO mode** with unrestricted shell access. This is an accepted tradeoff:

### Indirect Prompt Injection
- **Risk**: Malicious GitHub issue/comment content could manipulate agent behavior
- **Mitigation**: Model instruction-following robustness, dev branch buffer, human review gate on main
- **Hard rule**: Agents must **never directly execute code snippets from issue/comment bodies** — they write their own code based on understanding

### Agent Shell Access
- **Risk**: Agents could theoretically read secrets, modify other files, etc.
- **Mitigation**: Instruction prompts constraining behavior, worktree isolation, branded status reporting (anomalies visible on GitHub)
- **Choice**: Complexity/autonomy tradeoff — mechanical enforcement would require containers, breaking OAuth/auth flows

### Single PAT
- **Risk**: All agents act as the same GitHub identity
- **Mitigation**: Agent signature in comments, per-agent git author identity in commits. Agent actions distinguishable at the comment/commit level, not at the GitHub API audit level.

## Credential Management

```
~/.githubclaw/
├── secrets/
│   ├── webhook_secret          # GitHub App webhook verification secret
│   └── twitter_credentials     # Platform API keys (Marketer)
├── config.yaml                 # Non-sensitive config (no secrets here)
└── ...
```

- All secrets in `~/.githubclaw/secrets/` — never in per-repo `.githubclaw/`
- GitHub API access: `gh` CLI pre-authenticated with PAT on host machine
- Orchestrator `read_file` whitelist explicitly excludes `~/.githubclaw/secrets/`
- Platform credentials injected as environment variables at agent spawn time by webhook server

## Branch Protection

```
main branch:
  ├── Require pull request reviews ✓
  ├── Require review from someone other than last pusher ✓
  ├── Require status checks to pass ✓
  └── Restrict who can push ✓

dev branch:
  └── No branch protection (instruction-enforced pipeline: QA → Reviewer → merge)
```

`dev → main` PR requires human approval. This is the final safety gate for all agent-produced code.

## Security Boundaries Summary

| Boundary | Enforcement | Mechanism |
|----------|-------------|-----------|
| Webhook authenticity | Mechanical | HMAC signature verification |
| Fork PR execution | Mechanical | Payload field check + label gate |
| Orchestrator shell | Mechanical | Custom tools only, no Bash |
| Orchestrator file read | Mechanical | Path whitelist in Rust |
| Orchestrator file write | Mechanical | Only memory.md via dedicated tool |
| Agent tool permissions | Mechanical | CLI flags from parsed frontmatter |
| Agent behavioral scope | Instruction | Prompt-based constraints |
| Code landing on main | Mechanical | Branch protection (human review) |
| Agent git identity | Mechanical | git -c flags at commit time |
| Secrets isolation | Mechanical | Filesystem whitelist excludes secrets dir |

## Bug report

The GithubClaw sandbox flow looks unhealthy.

### Expected behavior
- Opening this issue should trigger the normal GithubClaw product pipeline.
- The system should either delegate follow-up work to an appropriate agent or ask for more information.
- The workflow must not spiral into duplicate dispatches or a noisy loop.

### Actual behavior
- Need product-E2E verification to confirm the runtime path.

### Steps to reproduce
1. Open this issue in the sandbox repository.
2. Wait for GithubClaw to ingest the webhook and react.

### Environment
- Repo: GithubClaw-Sandbox
- Scope: product-level E2E only
- Note: treat this as a disposable sandbox fixture, not a real customer bug

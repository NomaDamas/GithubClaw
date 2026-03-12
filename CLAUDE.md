# GithubClaw

Rust binary that receives GitHub webhooks and dispatches AI agents (Codex / Claude Code) to manage open-source projects autonomously.

## Build & Test

```bash
make check    # format + lint + test (run before every PR)
make ci       # full CI pipeline locally
make serve    # run webhook server in foreground (dev)
make release  # build release binary
make help     # show all targets
```

## Project Structure

```
src/
├── main.rs              # CLI entry point
├── cli.rs               # init, start, stop, status, logs, serve
├── server.rs            # axum webhook server + event drain loop + dispatch
├── config.rs            # GlobalConfig / RepoConfig (YAML)
├── queue.rs             # disk-persisted FIFO queue + dead-letter
├── signature.rs         # HMAC SHA-256 webhook verification
├── process_manager.rs   # child process spawn/monitor/kill
├── scheduler.rs         # cron + one-shot scheduled events
├── rate_limiter.rs      # three-tier rate limit handling
├── constants.rs         # all magic numbers
├── errors.rs            # error types
├── agents/
│   ├── parser.rs        # YAML frontmatter parser for agent .md files
│   ├── prompt_assembler.rs  # 4-layer prompt builder
│   └── spawner.rs       # CLI command + env builder for agent spawn
└── orchestrator/
    ├── schema.rs        # ActionList / Action types (serde)
    ├── session.rs       # spawns Codex/Claude Code as orchestrator
    └── tools.rs         # 12 scoped tools (gh CLI, file, memory)
```

## Key Conventions

- Tests live in `#[cfg(test)] mod tests` at the bottom of each module
- Integration tests in `tests/integration_test.rs`
- Agent definitions in `defaults/agents/*.md` — embedded via `include_str!`
- Specs in `docs/` (7 files) — the source of truth for all behavior
- No Python anywhere — pure Rust

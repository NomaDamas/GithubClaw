# Agents

## For AI coding agents working on this repo

### Quick reference

```bash
make check          # must pass before committing
cargo test          # 168 tests, all must pass
cargo clippy -- -D warnings   # zero warnings required
```

### Rules

- Follow the specs in `docs/` — they define every behavior
- Write tests first, then implement
- Don't add dependencies without good reason
- Keep modules focused — one concern per file
- Agent definitions in `defaults/agents/` are embedded at compile time via `include_str!`

### Architecture in one sentence

Rust webhook server receives GitHub events → spawns Codex/Claude Code as orchestrator → orchestrator returns structured ActionList JSON → server dispatches worker agents (also Codex/Claude Code) per the ActionList.

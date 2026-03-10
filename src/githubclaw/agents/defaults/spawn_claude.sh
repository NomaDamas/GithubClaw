#!/usr/bin/env bash
# GithubClaw — Claude Code spawn template
# This script is called by the webhook server to spawn a Claude Code agent.
#
# Environment variables provided by the webhook server:
#   PROMPT_FILE       — Path to the assembled 4-layer prompt temp file
#   TASK_PROMPT       — The task prompt string (brief task description)
#   ALLOWED_TOOLS     — Comma-separated list of allowed tools
#   DISALLOWED_TOOLS  — Comma-separated list of disallowed tools
#   MAX_TURNS         — Maximum conversation turns (default: 200)
#   GIT_AUTHOR_NAME   — Git author name for this agent
#   GIT_AUTHOR_EMAIL  — Git author email for this agent
#   GIT_COMMITTER_NAME  — Git committer name
#   GIT_COMMITTER_EMAIL — Git committer email
#   GITHUBCLAW_AGENT_TYPE — Agent type name (e.g. "coder")
#   GITHUBCLAW_BACKEND    — Backend name ("claude-code")
#   GITHUBCLAW_REPO_ROOT  — Path to the repository root
#
# You may customize this script to add additional flags, environment variables,
# or pre/post-processing steps. The webhook server will use this script if it
# exists in .githubclaw/spawn_claude.sh, otherwise it falls back to built-in
# command construction.

set -euo pipefail

CMD=(claude -p --dangerously-skip-permissions)

if [ -n "${ALLOWED_TOOLS:-}" ]; then
    CMD+=(--allowedTools "${ALLOWED_TOOLS}")
fi

if [ -n "${DISALLOWED_TOOLS:-}" ]; then
    CMD+=(--disallowedTools "${DISALLOWED_TOOLS}")
fi

CMD+=(--max-turns "${MAX_TURNS:-200}")
CMD+=(--append-system-prompt-file "${PROMPT_FILE}")
CMD+=("${TASK_PROMPT}")

exec "${CMD[@]}"

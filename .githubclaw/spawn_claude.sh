#!/usr/bin/env bash
# GithubClaw spawn template for Claude Code.
set -euo pipefail
exec claude -p \
  --dangerously-skip-permissions \
  --allowedTools "${ALLOWED_TOOLS}" \
  --disallowedTools "${DISALLOWED_TOOLS}" \
  --max-turns "${MAX_TURNS:-200}" \
  --append-system-prompt-file "${PROMPT_FILE}" \
  "$TASK_PROMPT"

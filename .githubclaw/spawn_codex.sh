#!/usr/bin/env bash
# GithubClaw spawn template for Codex CLI.
set -euo pipefail
cat "${PROMPT_FILE}" | codex exec - \
  --dangerously-bypass-approvals-and-sandbox

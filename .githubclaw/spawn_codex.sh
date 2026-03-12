#!/usr/bin/env bash
# GithubClaw spawn template for Codex CLI.
# Override this file to customize agent spawn behavior.
set -euo pipefail

cat "${PROMPT_FILE}" | codex exec - \
  --dangerously-bypass-approvals-and-sandbox

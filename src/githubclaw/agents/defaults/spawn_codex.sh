#!/usr/bin/env bash
# GithubClaw — Codex spawn template
# This script is called by the webhook server to spawn a Codex agent.
#
# Environment variables provided by the webhook server:
#   PROMPT_FILE       — Path to the assembled 4-layer prompt temp file
#   TASK_PROMPT       — The task prompt string (not directly used by Codex)
#   ALLOWED_TOOLS     — Comma-separated list of allowed tools (informational)
#   DISALLOWED_TOOLS  — Comma-separated list of disallowed tools (informational)
#   MAX_TURNS         — Maximum conversation turns (informational)
#   GIT_AUTHOR_NAME   — Git author name for this agent
#   GIT_AUTHOR_EMAIL  — Git author email for this agent
#   GIT_COMMITTER_NAME  — Git committer name
#   GIT_COMMITTER_EMAIL — Git committer email
#   GITHUBCLAW_AGENT_TYPE — Agent type name (e.g. "coder")
#   GITHUBCLAW_BACKEND    — Backend name ("codex")
#   GITHUBCLAW_REPO_ROOT  — Path to the repository root
#
# You may customize this script to add additional flags, environment variables,
# or pre/post-processing steps. The webhook server will use this script if it
# exists in .githubclaw/spawn_codex.sh, otherwise it falls back to built-in
# command construction.

set -euo pipefail

exec cat "${PROMPT_FILE}" | codex exec - \
    --approval-mode full-auto \
    --sandbox off

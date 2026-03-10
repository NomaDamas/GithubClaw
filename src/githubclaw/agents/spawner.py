"""Agent CLI spawner.

Builds CLI commands from AgentDefinition + prompt file path and launches
agent subprocesses. Supports both Claude Code and Codex backends, with
overridable shell script templates.
"""

from __future__ import annotations

import logging
import os
import subprocess
from typing import TYPE_CHECKING

from githubclaw.constants import DEFAULT_AGENT_MAX_TURNS
from githubclaw.errors import AgentSpawnError

if TYPE_CHECKING:
    from pathlib import Path

    from githubclaw.agents.parser import AgentDefinition

logger = logging.getLogger(__name__)

# Default spawn command templates (used when no shell script override exists).
_DEFAULT_CLAUDE_TEMPLATE = (
    "claude -p"
    " --dangerously-skip-permissions"
    " --allowedTools {allowed_tools}"
    " --disallowedTools {disallowed_tools}"
    " --max-turns {max_turns}"
    ' --append-system-prompt-file "{prompt_file}"'
    ' "{task_prompt}"'
)

_DEFAULT_CODEX_TEMPLATE = (
    'cat "{prompt_file}" | codex exec - --approval-mode full-auto --sandbox off'
)


class AgentSpawner:
    """Spawns agent CLI processes from definitions and assembled prompts."""

    def __init__(
        self,
        repo_root: Path,
        max_turns: int = DEFAULT_AGENT_MAX_TURNS,
    ) -> None:
        self.repo_root = repo_root
        self.max_turns = max_turns

    def _get_spawn_script(self, backend: str) -> Path | None:
        """Return the overridable spawn script path if it exists."""
        if backend == "claude-code":
            script = self.repo_root / ".githubclaw" / "spawn_claude.sh"
        elif backend == "codex":
            script = self.repo_root / ".githubclaw" / "spawn_codex.sh"
        else:
            return None
        return script if script.exists() else None

    def _build_env(
        self,
        agent_def: AgentDefinition,
        prompt_file: Path,
        task_prompt: str,
        extra_env: dict[str, str] | None = None,
    ) -> dict[str, str]:
        """Build environment variables for the agent subprocess."""
        env = os.environ.copy()

        # Git author identity for this agent.
        env["GIT_AUTHOR_NAME"] = agent_def.git_author_name
        env["GIT_AUTHOR_EMAIL"] = agent_def.git_author_email
        env["GIT_COMMITTER_NAME"] = agent_def.git_author_name
        env["GIT_COMMITTER_EMAIL"] = agent_def.git_author_email

        # Tool permissions (for spawn script templates).
        tools = agent_def.active_tools
        env["ALLOWED_TOOLS"] = ",".join(tools.allowed) if tools.allowed else ""
        env["DISALLOWED_TOOLS"] = ",".join(tools.disallowed) if tools.disallowed else ""

        # Prompt and task info.
        env["PROMPT_FILE"] = str(prompt_file)
        env["TASK_PROMPT"] = task_prompt
        env["MAX_TURNS"] = str(self.max_turns)

        # Agent metadata.
        env["GITHUBCLAW_AGENT_TYPE"] = agent_def.name
        env["GITHUBCLAW_BACKEND"] = agent_def.backend
        env["GITHUBCLAW_REPO_ROOT"] = str(self.repo_root)

        if extra_env:
            env.update(extra_env)

        return env

    def _build_command_from_template(
        self,
        agent_def: AgentDefinition,
        prompt_file: Path,
        task_prompt: str,
    ) -> list[str]:
        """Build the CLI command using built-in templates (no shell script override)."""
        tools = agent_def.active_tools

        if agent_def.backend == "claude-code":
            allowed = ",".join(tools.allowed) if tools.allowed else ""
            disallowed = ",".join(tools.disallowed) if tools.disallowed else ""

            cmd_parts: list[str] = ["claude", "-p", "--dangerously-skip-permissions"]

            if allowed:
                cmd_parts.extend(["--allowedTools", allowed])
            if disallowed:
                cmd_parts.extend(["--disallowedTools", disallowed])

            cmd_parts.extend(["--max-turns", str(self.max_turns)])
            cmd_parts.extend(["--append-system-prompt-file", str(prompt_file)])
            cmd_parts.append(task_prompt)

            return cmd_parts

        elif agent_def.backend == "codex":
            # Codex uses piped stdin, so we need shell=True via the script path.
            # When there's no script override, we build a shell command string.
            return [
                "bash",
                "-c",
                f'cat "{prompt_file}" | codex exec - --approval-mode full-auto --sandbox off',
            ]

        else:
            raise AgentSpawnError(f"Unknown backend: {agent_def.backend!r}")

    def spawn(
        self,
        agent_def: AgentDefinition,
        prompt_file: Path,
        task_prompt: str,
        extra_env: dict[str, str] | None = None,
    ) -> subprocess.Popen[bytes]:
        """Spawn an agent subprocess.

        Args:
            agent_def: Parsed agent definition.
            prompt_file: Path to the assembled prompt temp file.
            task_prompt: The task prompt string (for Claude Code's positional arg).
            extra_env: Additional environment variables (e.g. platform API keys).

        Returns:
            subprocess.Popen handle for the running agent process.
        """
        env = self._build_env(agent_def, prompt_file, task_prompt, extra_env)

        # Check for user-provided spawn script override.
        spawn_script = self._get_spawn_script(agent_def.backend)

        if spawn_script is not None:
            # Use the overridable shell script. All config is passed via env vars.
            cmd: list[str] | str = ["bash", str(spawn_script)]
            logger.info(
                "Spawning %s agent (%s backend) via script: %s",
                agent_def.name,
                agent_def.backend,
                spawn_script,
            )
        else:
            # Use built-in command template.
            cmd = self._build_command_from_template(agent_def, prompt_file, task_prompt)
            logger.info(
                "Spawning %s agent (%s backend) via built-in template",
                agent_def.name,
                agent_def.backend,
            )

        logger.debug("Agent command: %s", cmd)
        logger.debug("Agent working directory: %s", self.repo_root)

        try:
            process = subprocess.Popen(
                cmd,
                cwd=self.repo_root,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
        except (OSError, FileNotFoundError) as exc:
            logger.error(
                "Failed to spawn %s agent (%s backend): %s",
                agent_def.name,
                agent_def.backend,
                exc,
            )
            raise AgentSpawnError(
                f"Could not start {agent_def.backend!r} CLI for agent "
                f"{agent_def.name!r}. Is the binary installed and on PATH? "
                f"Original error: {exc}"
            ) from exc

        logger.info(
            "Agent %s spawned with PID %d (backend=%s, timeout=%s)",
            agent_def.name,
            process.pid,
            agent_def.backend,
            agent_def.timeout or "global default",
        )

        return process

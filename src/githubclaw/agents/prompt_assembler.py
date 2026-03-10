"""4-layer prompt assembly for GithubClaw agents.

Layers:
  1. .githubclaw/global-prompt.md   — agent roster, common rules
  2. .githubclaw/VALUE.md           — project north star (read fresh every time)
  3. Agent instruction body          — from parsed definition (minus frontmatter)
  4. task_context                    — from orchestrator dispatch output

The assembled prompt is written to a temp file and the path is returned.
Cleanup removes the temp file after the agent exits.
"""

from __future__ import annotations

import contextlib
import logging
import os
import tempfile
from pathlib import Path

from githubclaw.agents.parser import AgentDefinition, get_defaults_dir

logger = logging.getLogger(__name__)

# Separator inserted between layers for readability in the assembled prompt.
_LAYER_SEPARATOR = "\n\n---\n\n"


class PromptAssembler:
    """Assembles the 4-layer agent prompt and manages temp file lifecycle."""

    def __init__(self, repo_root: Path) -> None:
        self.repo_root = repo_root
        self._temp_files: list[Path] = []

    def _read_layer_file(self, path: Path) -> str:
        """Read a layer file, returning empty string if missing."""
        if path.exists():
            return path.read_text(encoding="utf-8").strip()
        logger.warning("Prompt layer file not found: %s", path)
        return ""

    def _read_global_prompt(self) -> str:
        """Layer 1: Read .githubclaw/global-prompt.md with fallback to defaults.

        Raises:
            ValueError: If neither repo-local nor built-in global-prompt.md exists.
        """
        repo_path = self.repo_root / ".githubclaw" / "global-prompt.md"
        if repo_path.exists():
            return self._read_layer_file(repo_path)
        # Fall back to built-in default.
        default_path = get_defaults_dir() / "global_prompt.md"
        if default_path.exists():
            return self._read_layer_file(default_path)
        raise ValueError(
            f"global-prompt.md not found in {repo_path} or built-in defaults "
            f"({default_path}). This file is critical — it contains the agent "
            f"roster and common rules. Run `githubclaw init` to generate it."
        )

    def _read_value(self) -> str:
        """Layer 2: Read .githubclaw/VALUE.md fresh every time."""
        return self._read_layer_file(self.repo_root / ".githubclaw" / "VALUE.md")

    def assemble(
        self,
        agent_def: AgentDefinition,
        task_context: str,
    ) -> Path:
        """Assemble the 4-layer prompt and write to a temp file.

        Args:
            agent_def: Parsed agent definition (provides Layer 3 instruction body).
            task_context: Task context string from orchestrator dispatch output (Layer 4).

        Returns:
            Path to the temp file containing the assembled prompt.
        """
        layers: list[str] = []

        # Layer 1: Global prompt (agent roster, common rules).
        global_prompt = self._read_global_prompt()
        if global_prompt:
            layers.append(global_prompt)

        # Layer 2: VALUE.md (project north star).
        value = self._read_value()
        if value:
            layers.append(f"# Project North Star (VALUE.md)\n\n{value}")

        # Layer 3: Agent-specific instruction body.
        if agent_def.instruction_body:
            layers.append(agent_def.instruction_body)

        # Layer 4: Task context from orchestrator.
        if task_context:
            layers.append(f"# Current Task\n\n{task_context}")

        assembled = _LAYER_SEPARATOR.join(layers)

        # Write to temp file.
        fd, temp_path_str = tempfile.mkstemp(
            prefix="githubclaw_prompt_",
            suffix=".md",
        )
        temp_path = Path(temp_path_str)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as f:
                f.write(assembled)
        except Exception:
            # If writing fails, close the fd and clean up.
            with contextlib.suppress(OSError):
                os.close(fd)
            temp_path.unlink(missing_ok=True)
            raise

        self._temp_files.append(temp_path)
        logger.info("Assembled prompt (%d chars) written to %s", len(assembled), temp_path)
        return temp_path

    def cleanup(self, path: Path | None = None) -> None:
        """Remove temp prompt file(s) after agent exits.

        Args:
            path: Specific temp file to remove. If None, removes all tracked temp files.
        """
        if path is not None:
            path.unlink(missing_ok=True)
            if path in self._temp_files:
                self._temp_files.remove(path)
            logger.debug("Cleaned up temp prompt file: %s", path)
        else:
            for p in self._temp_files:
                p.unlink(missing_ok=True)
                logger.debug("Cleaned up temp prompt file: %s", p)
            self._temp_files.clear()

    def cleanup_all(self) -> None:
        """Remove all tracked temp prompt files."""
        self.cleanup()

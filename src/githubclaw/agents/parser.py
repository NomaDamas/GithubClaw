"""Agent definition file parser.

Parses YAML frontmatter from .githubclaw/agents/*.md files and extracts
configuration (backend, git author, tool permissions, timeout) plus the
instruction body (markdown minus frontmatter).
"""

from __future__ import annotations

import logging
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml

logger = logging.getLogger(__name__)

_VALID_BACKENDS = ["codex", "claude-code"]

# Regex to match YAML frontmatter delimited by --- on its own line.
_FRONTMATTER_RE = re.compile(r"\A---\s*\n(.*?\n)---\s*\n", re.DOTALL)


@dataclass
class ToolPermissions:
    """Per-backend tool allow/disallow lists."""

    allowed: list[str] = field(default_factory=list)
    disallowed: list[str] = field(default_factory=list)


@dataclass
class AgentDefinition:
    """Parsed agent definition — frontmatter fields plus instruction body."""

    # Identity
    name: str  # Derived from filename (e.g. "coder" from coder.md)

    # Frontmatter fields
    backend: str = "codex"  # "codex" or "claude-code"
    git_author_name: str = "GithubClaw Agent"
    git_author_email: str = "agent@githubclaw.local"
    timeout: int | None = None  # None means use global default
    tools: dict[str, ToolPermissions] = field(default_factory=dict)

    # Instruction body (markdown minus frontmatter)
    instruction_body: str = ""

    @property
    def active_tools(self) -> ToolPermissions:
        """Return the tool permissions for the configured backend."""
        return self.tools.get(self.backend, ToolPermissions())


def _parse_tools(raw_tools: dict[str, Any] | None) -> dict[str, ToolPermissions]:
    """Parse the tools section from frontmatter YAML."""
    if not raw_tools or not isinstance(raw_tools, dict):
        return {}
    result: dict[str, ToolPermissions] = {}
    for backend_name, perms in raw_tools.items():
        if not isinstance(perms, dict):
            continue
        result[backend_name] = ToolPermissions(
            allowed=perms.get("allowed", []) or [],
            disallowed=perms.get("disallowed", []) or [],
        )
    return result


def parse_agent_file(path: Path) -> AgentDefinition:
    """Parse a single agent definition .md file.

    Args:
        path: Path to the agent definition file (e.g. .githubclaw/agents/coder.md).

    Returns:
        AgentDefinition with frontmatter fields and instruction body populated.

    Raises:
        FileNotFoundError: If the file does not exist.
        ValueError: If the file has malformed frontmatter.
    """
    text = path.read_text(encoding="utf-8")
    name = path.stem  # "coder" from coder.md

    match = _FRONTMATTER_RE.match(text)
    if match is None:
        # No frontmatter — treat entire file as instruction body with defaults.
        return AgentDefinition(name=name, instruction_body=text.strip())

    frontmatter_raw = match.group(1)
    instruction_body = text[match.end() :].strip()

    try:
        fm: dict[str, Any] = yaml.safe_load(frontmatter_raw) or {}
    except yaml.YAMLError as e:
        raise ValueError(f"Malformed YAML frontmatter in {path}: {e}") from e

    if not isinstance(fm, dict):
        raise ValueError(f"Frontmatter must be a YAML mapping in {path}")

    backend = fm.get("backend", "codex")
    if backend not in _VALID_BACKENDS:
        logger.warning(
            "Invalid backend %r in %s; expected one of %s. Defaulting to 'codex'.",
            backend,
            path,
            _VALID_BACKENDS,
        )
        backend = "codex"

    git_author_email = fm.get("git_author_email", "agent@githubclaw.local")
    if "@" not in git_author_email:
        logger.warning(
            "git_author_email %r in %s does not contain '@'; this may cause issues.",
            git_author_email,
            path,
        )

    return AgentDefinition(
        name=name,
        backend=backend,
        git_author_name=fm.get("git_author_name", "GithubClaw Agent"),
        git_author_email=git_author_email,
        timeout=fm.get("timeout"),
        tools=_parse_tools(fm.get("tools")),
        instruction_body=instruction_body,
    )


def list_agent_types(repo_root: Path) -> list[str]:
    """List all available agent type names by scanning .githubclaw/agents/ directory.

    Returns stem names of all .md files found (e.g. ["coder", "qa", "reviewer"]).
    """
    agents_dir = repo_root / ".githubclaw" / "agents"
    if not agents_dir.is_dir():
        return []
    return sorted(p.stem for p in agents_dir.glob("*.md"))


def load_agent_definition(repo_root: Path, agent_type: str) -> AgentDefinition:
    """Load an agent definition by type name from the repo's .githubclaw/agents/ directory.

    Falls back to built-in defaults if the repo doesn't have a custom definition.

    Args:
        repo_root: Path to the repository root.
        agent_type: Agent type name (e.g. "coder", "qa").

    Returns:
        Parsed AgentDefinition.

    Raises:
        FileNotFoundError: If neither repo-local nor default definition exists.
    """
    # Try repo-local first.
    repo_path = repo_root / ".githubclaw" / "agents" / f"{agent_type}.md"
    if repo_path.exists():
        return parse_agent_file(repo_path)

    # Fall back to built-in default.
    default_path = get_default_agent_path(agent_type)
    if default_path.exists():
        return parse_agent_file(default_path)

    raise FileNotFoundError(
        f"No agent definition found for '{agent_type}' in {repo_path} or built-in defaults"
    )


def get_defaults_dir() -> Path:
    """Return the path to the built-in defaults directory."""
    return Path(__file__).parent / "defaults"


def get_default_agent_path(agent_type: str) -> Path:
    """Return the path to a built-in default agent definition file."""
    return get_defaults_dir() / f"{agent_type}.md"


def list_default_agent_types() -> list[str]:
    """List all built-in default agent type names."""
    defaults_dir = get_defaults_dir()
    if not defaults_dir.is_dir():
        return []
    return sorted(
        p.stem
        for p in defaults_dir.glob("*.md")
        if p.stem
        not in ("global_prompt", "value_template", "orchestrator_template", "memory_template")
    )

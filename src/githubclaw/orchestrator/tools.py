"""Scoped custom tools for the orchestrator.

The orchestrator has no raw shell access. All GitHub interactions go through
`gh` CLI subprocess calls. File access is restricted to the repo directory
and `.githubclaw/` config directory via whitelist enforcement.
"""

from __future__ import annotations

import asyncio
import logging
import os
import re
from pathlib import Path
from typing import Any

from githubclaw.constants import (
    GH_CLI_TIMEOUT_SECONDS,
    ISSUES_LIST_LIMIT,
    PRS_LIST_LIMIT,
    SEARCH_QUERY_MAX_LENGTH,
    SEARCH_RESULTS_LIMIT,
)
from githubclaw.orchestrator.sdk_abstraction import ToolDefinition

logger = logging.getLogger(__name__)

# Directories that are always denied, even if they fall under an allowed path.
_DENIED_PATHS: tuple[str, ...] = (
    os.path.expanduser("~/.githubclaw/secrets"),
    os.path.expanduser("~/.ssh"),
    os.path.expanduser("~/.gnupg"),
    os.path.expanduser("~/.aws"),
    os.path.expanduser("~/.config/gh"),
)


# ---------------------------------------------------------------------------
# Tool registry helpers
# ---------------------------------------------------------------------------

_TOOL_REGISTRY: list[dict[str, Any]] = []


def _tool(name: str, description: str, parameters: dict[str, Any]):
    """Decorator to register a function as an orchestrator tool."""

    def decorator(fn: Any) -> Any:
        _TOOL_REGISTRY.append(
            {
                "name": name,
                "description": description,
                "parameters": parameters,
                "factory": fn,
            }
        )
        return fn

    return decorator


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------


async def _run_gh(args: list[str], repo: str, timeout: float = GH_CLI_TIMEOUT_SECONDS) -> str:
    """Run a `gh` CLI command and return its stdout.

    Args:
        args: Arguments to pass after `gh`.
        repo: Repository in 'owner/repo' format, passed as --repo.
        timeout: Subprocess timeout in seconds.

    Returns:
        stdout as a string.

    Raises:
        RuntimeError: If the gh command fails or times out.
    """
    cmd = ["gh", *args, "--repo", repo]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except TimeoutError:
        msg = f"gh command timed out after {timeout}s: {' '.join(cmd)}"
        raise RuntimeError(msg) from None
    except FileNotFoundError:
        msg = "gh CLI not found. Install it from https://cli.github.com/"
        raise RuntimeError(msg) from None

    if proc.returncode != 0:
        error_text = stderr.decode("utf-8", errors="replace").strip()
        msg = f"gh command failed (exit {proc.returncode}): {error_text}"
        raise RuntimeError(msg)

    return stdout.decode("utf-8", errors="replace")


def _resolve_and_check_path(
    path: str,
    repo_dir: str,
    extra_allowed: list[str] | None = None,
) -> Path:
    """Resolve a file path and enforce the whitelist.

    Args:
        path: The requested path (may be relative to repo_dir).
        repo_dir: Absolute path to the repository root.
        extra_allowed: Additional allowed directory prefixes from config.

    Returns:
        Resolved absolute Path.

    Raises:
        PermissionError: If the path falls outside allowed directories.
    """
    # Resolve relative paths against repo_dir
    candidate = Path(path)
    if not candidate.is_absolute():
        candidate = Path(repo_dir) / candidate
    resolved = candidate.resolve()

    # Check denied paths first (takes priority over everything)
    resolved_str = str(resolved)
    for denied in _DENIED_PATHS:
        denied_resolved = str(Path(denied).resolve())
        if resolved_str == denied_resolved or resolved_str.startswith(denied_resolved + "/"):
            msg = f"Access denied: {resolved} is in a restricted directory"
            raise PermissionError(msg)

    # Build allowed prefixes
    allowed_prefixes = [
        str(Path(repo_dir).resolve()),
        str(Path(os.path.expanduser("~/.githubclaw")).resolve()),
    ]
    if extra_allowed:
        for p in extra_allowed:
            allowed_prefixes.append(str(Path(p).resolve()))

    # Check if resolved path is under any allowed prefix
    for prefix in allowed_prefixes:
        if resolved_str == prefix or resolved_str.startswith(prefix + "/"):
            return resolved

    msg = (
        f"Access denied: {resolved} is outside allowed directories. "
        f"Allowed: {', '.join(allowed_prefixes)}"
    )
    raise PermissionError(msg)


def _parse_memory_sections(content: str) -> dict[str, str]:
    """Parse memory.md into sections keyed by heading name.

    Sections are delimited by ## headings. Content before the first heading
    is stored under the key "__preamble__".
    """
    sections: dict[str, str] = {}
    current_section = "__preamble__"
    current_lines: list[str] = []

    for line in content.splitlines():
        heading_match = re.match(r"^##\s+(.+)$", line)
        if heading_match:
            # Save previous section
            sections[current_section] = "\n".join(current_lines).strip()
            current_section = heading_match.group(1).strip()
            current_lines = []
        else:
            current_lines.append(line)

    # Save last section
    sections[current_section] = "\n".join(current_lines).strip()

    # Remove empty preamble
    if not sections.get("__preamble__"):
        sections.pop("__preamble__", None)

    return sections


def _rebuild_memory_file(sections: dict[str, str]) -> str:
    """Rebuild memory.md content from a sections dict."""
    parts: list[str] = []

    # Work on a copy to avoid mutating the caller's dict
    sections = dict(sections)

    # Preamble first (if present)
    preamble = sections.pop("__preamble__", None)
    if preamble:
        parts.append(preamble)

    for heading, body in sections.items():
        parts.append(f"## {heading}")
        if body:
            parts.append(body)
        parts.append("")  # blank line after section

    return "\n".join(parts).strip() + "\n"


# ---------------------------------------------------------------------------
# Tool definitions via decorator
# ---------------------------------------------------------------------------


@_tool(
    name="get_issue",
    description="Get details of a specific GitHub issue by number.",
    parameters={
        "type": "object",
        "properties": {
            "number": {"type": "integer", "description": "The issue number."},
        },
        "required": ["number"],
    },
)
def _get_issue(repo: str, **_kw: Any):
    async def handler(number: int) -> str:
        return await _run_gh(
            [
                "issue",
                "view",
                str(number),
                "--json",
                "number,title,state,body,labels,assignees,comments,author,createdAt,updatedAt",
            ],
            repo,
        )

    return handler


@_tool(
    name="list_issues",
    description="List GitHub issues with optional state, label, and date filters.",
    parameters={
        "type": "object",
        "properties": {
            "state": {
                "type": "string",
                "description": "Filter by state: 'open', 'closed', or 'all'.",
                "default": "open",
            },
            "labels": {
                "type": "string",
                "description": "Comma-separated label names to filter by.",
                "default": "",
            },
            "since": {
                "type": "string",
                "description": "ISO date -- only issues updated on or after this date.",
                "default": "",
            },
        },
        "required": [],
    },
)
def _list_issues(repo: str, **_kw: Any):
    async def handler(state: str = "open", labels: str = "", since: str = "") -> str:
        args = [
            "issue",
            "list",
            "--state",
            state,
            "--json",
            "number,title,state,labels,assignees,author,createdAt,updatedAt",
            "--limit",
            str(ISSUES_LIST_LIMIT),
        ]
        if labels:
            args.extend(["--label", labels])
        if since:
            args.extend(["--search", f"updated:>={since}"])
        return await _run_gh(args, repo)

    return handler


@_tool(
    name="get_pr",
    description="Get details of a specific pull request by number.",
    parameters={
        "type": "object",
        "properties": {
            "number": {"type": "integer", "description": "The PR number."},
        },
        "required": ["number"],
    },
)
def _get_pr(repo: str, **_kw: Any):
    async def handler(number: int) -> str:
        return await _run_gh(
            [
                "pr",
                "view",
                str(number),
                "--json",
                "number,title,state,body,labels,assignees,author,reviews,"
                "headRefName,baseRefName,mergeable,additions,deletions,"
                "createdAt,updatedAt,comments",
            ],
            repo,
        )

    return handler


@_tool(
    name="list_prs",
    description="List pull requests with optional state filter.",
    parameters={
        "type": "object",
        "properties": {
            "state": {
                "type": "string",
                "description": "Filter by state: 'open', 'closed', 'merged', or 'all'.",
                "default": "open",
            },
        },
        "required": [],
    },
)
def _list_prs(repo: str, **_kw: Any):
    async def handler(state: str = "open") -> str:
        return await _run_gh(
            [
                "pr",
                "list",
                "--state",
                state,
                "--json",
                "number,title,state,author,headRefName,baseRefName,createdAt,updatedAt,labels",
                "--limit",
                str(PRS_LIST_LIMIT),
            ],
            repo,
        )

    return handler


@_tool(
    name="get_pr_diff",
    description="Get the full diff of a specific pull request.",
    parameters={
        "type": "object",
        "properties": {
            "number": {"type": "integer", "description": "The PR number."},
        },
        "required": ["number"],
    },
)
def _get_pr_diff(repo: str, **_kw: Any):
    async def handler(number: int) -> str:
        return await _run_gh(["pr", "diff", str(number)], repo)

    return handler


@_tool(
    name="get_discussion",
    description="Get details of a specific GitHub Discussion by number.",
    parameters={
        "type": "object",
        "properties": {
            "number": {"type": "integer", "description": "The discussion number."},
        },
        "required": ["number"],
    },
)
def _get_discussion(repo: str, **_kw: Any):
    async def handler(number: int) -> str:
        return await _run_gh(
            ["api", f"repos/{repo}/discussions/{number}", "--jq", "."],
            repo="",
        )

    return handler


@_tool(
    name="get_ci_status",
    description="Get the status and details of a GitHub Actions workflow run.",
    parameters={
        "type": "object",
        "properties": {
            "run_id": {"type": "string", "description": "The workflow run ID."},
        },
        "required": ["run_id"],
    },
)
def _get_ci_status(repo: str, **_kw: Any):
    async def handler(run_id: str) -> str:
        return await _run_gh(
            [
                "run",
                "view",
                run_id,
                "--json",
                "status,conclusion,name,workflowName,jobs,createdAt,updatedAt",
            ],
            repo,
        )

    return handler


@_tool(
    name="search_issues",
    description="Search issues and PRs in the repository using a query string.",
    parameters={
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Search query (GitHub search syntax).",
            },
        },
        "required": ["query"],
    },
)
def _search_issues(repo: str, **_kw: Any):
    async def handler(query: str) -> str:
        if not query or len(query) > SEARCH_QUERY_MAX_LENGTH:
            return (
                f"Error: search query must be between 1 and {SEARCH_QUERY_MAX_LENGTH} characters "
                f"(got {len(query) if query else 0})."
            )
        return await _run_gh(
            [
                "search",
                "issues",
                "--repo",
                repo,
                "--json",
                "number,title,state,type,author,createdAt,updatedAt",
                "--limit",
                str(SEARCH_RESULTS_LIMIT),
                query,
            ],
            repo="",
        )

    return handler


@_tool(
    name="read_file",
    description=(
        "Read a file from the repository or .githubclaw/ config directory. "
        "Path can be relative to repo root or absolute. Access is restricted "
        "to the repository and .githubclaw/ directories only."
    ),
    parameters={
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "File path (relative to repo root, or absolute).",
            },
        },
        "required": ["path"],
    },
)
def _read_file(repo_dir: str, extra_allowed_paths: list[str] | None = None, **_kw: Any):
    async def handler(path: str) -> str:
        resolved = _resolve_and_check_path(path, repo_dir, extra_allowed_paths)
        if not resolved.is_file():
            msg = f"Not a file or does not exist: {resolved}"
            raise FileNotFoundError(msg)
        return resolved.read_text(encoding="utf-8", errors="replace")

    return handler


@_tool(
    name="read_memory",
    description=(
        "Read a specific section from .githubclaw/memory.md by heading name. "
        "Examples: 'Contributors', 'Recurring Bugs', 'Architectural Patterns'."
    ),
    parameters={
        "type": "object",
        "properties": {
            "section": {
                "type": "string",
                "description": "The section heading name to read.",
            },
        },
        "required": ["section"],
    },
)
def _read_memory(memory_file: str, **_kw: Any):
    async def handler(section: str) -> str:
        mem_path = Path(memory_file)
        if not mem_path.is_file():
            return f"memory.md does not exist yet at {memory_file}"
        content = mem_path.read_text(encoding="utf-8", errors="replace")
        sections = _parse_memory_sections(content)
        if section in sections:
            return sections[section]
        available = [k for k in sections if k != "__preamble__"]
        return (
            f"Section '{section}' not found. "
            f"Available sections: {', '.join(available) if available else '(none)'}"
        )

    return handler


@_tool(
    name="write_memory",
    description=(
        "Write or update a specific section in .githubclaw/memory.md. "
        "Creates the file if it doesn't exist. Replaces the section content."
    ),
    parameters={
        "type": "object",
        "properties": {
            "section": {
                "type": "string",
                "description": "The section heading name to write.",
            },
            "content": {
                "type": "string",
                "description": "The new content for this section.",
            },
        },
        "required": ["section", "content"],
    },
)
def _write_memory(memory_file: str, **_kw: Any):
    async def handler(section: str, content: str) -> str:
        mem_path = Path(memory_file)
        if mem_path.is_file():
            existing = mem_path.read_text(encoding="utf-8", errors="replace")
            sections = _parse_memory_sections(existing)
        else:
            mem_path.parent.mkdir(parents=True, exist_ok=True)
            sections = {}
        sections[section] = content.strip()
        mem_path.write_text(_rebuild_memory_file(sections), encoding="utf-8")
        return f"Updated section '{section}' in memory.md"

    return handler


@_tool(
    name="web_search",
    description=(
        "Search the web for information. Useful for looking up documentation, "
        "CVEs, library release notes, or other external context. "
        "Requires configuration in .githubclaw/config.toml."
    ),
    parameters={
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "The search query string.",
            },
        },
        "required": ["query"],
    },
)
def _web_search(**_kw: Any):
    async def handler(query: str) -> str:
        return (
            "Web search is not configured. To enable web search, set up a search "
            "API provider in your .githubclaw/config.toml under [tools.web_search]. "
            f"Query received: {query!r}"
        )

    return handler


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def build_tools(
    repo: str,
    repo_dir: str,
    githubclaw_dir: str | None = None,
    extra_allowed_paths: list[str] | None = None,
) -> list[ToolDefinition]:
    """Build the list of scoped tool definitions for an orchestrator session.

    Args:
        repo: Repository in 'owner/repo' format for gh CLI calls.
        repo_dir: Absolute path to the local repository clone.
        githubclaw_dir: Path to .githubclaw directory (defaults to {repo_dir}/.githubclaw).
        extra_allowed_paths: Additional paths allowed for read_file (from config).

    Returns:
        List of ToolDefinition instances ready to register with an AgentSDK session.
    """
    if githubclaw_dir is None:
        githubclaw_dir = os.path.join(repo_dir, ".githubclaw")

    memory_file = os.path.join(githubclaw_dir, "memory.md")

    context = {
        "repo": repo,
        "repo_dir": repo_dir,
        "githubclaw_dir": githubclaw_dir,
        "memory_file": memory_file,
        "extra_allowed_paths": extra_allowed_paths,
    }

    tools: list[ToolDefinition] = []
    for entry in _TOOL_REGISTRY:
        handler = entry["factory"](**context)
        tools.append(
            ToolDefinition(
                name=entry["name"],
                description=entry["description"],
                parameters=entry["parameters"],
                handler=handler,
            )
        )
    return tools

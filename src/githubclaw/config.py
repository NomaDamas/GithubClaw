"""Configuration loading for GithubClaw.

Loads from two sources:
- Global: ~/.githubclaw/config.yaml (shared across all repos)
- Per-repo: .githubclaw/config.yaml (repo-specific overrides)
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml

from githubclaw.constants import (
    DEFAULT_CONFIG_DRAIN_TIMEOUT_SECONDS,
    DEFAULT_CONFIG_MAX_CONCURRENT_AGENTS,
    DEFAULT_CONFIG_MAX_RETRY,
    DEFAULT_GLOBAL_TIMEOUT_SECONDS,
    DEFAULT_ORCHESTRATOR_IDLE_TIMEOUT_SECONDS,
    DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS,
    DEFAULT_SERVER_PORT,
)

logger = logging.getLogger(__name__)


GLOBAL_CONFIG_DIR = Path.home() / ".githubclaw"
GLOBAL_CONFIG_PATH = GLOBAL_CONFIG_DIR / "config.yaml"
REPO_CONFIG_DIR_NAME = ".githubclaw"
REPO_CONFIG_FILENAME = "config.yaml"


DEFAULT_EVENT_SUBSCRIPTION: list[str] = [
    "issues",
    "issue_comment",
    "pull_request",
    "pull_request_review",
    "pull_request_review_comment",
    "discussion",
    "discussion_comment",
    "label",
    "milestone",
    "projects_v2_item",
    "check_suite",
    "check_run",
]


@dataclass
class GlobalConfig:
    """Global configuration loaded from ~/.githubclaw/config.yaml."""

    port: int = DEFAULT_SERVER_PORT
    host: str = "0.0.0.0"
    max_concurrent_agents: int = DEFAULT_CONFIG_MAX_CONCURRENT_AGENTS
    global_timeout: int = DEFAULT_GLOBAL_TIMEOUT_SECONDS
    orchestrator_idle_timeout: int = DEFAULT_ORCHESTRATOR_IDLE_TIMEOUT_SECONDS
    drain_timeout: int = DEFAULT_CONFIG_DRAIN_TIMEOUT_SECONDS
    recovery_probe_interval: int = DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS
    max_retry: int = DEFAULT_CONFIG_MAX_RETRY
    event_subscription: list[str] = field(default_factory=lambda: list(DEFAULT_EVENT_SUBSCRIPTION))

    @classmethod
    def load(cls, path: Path | None = None) -> GlobalConfig:
        """Load global config from disk, falling back to defaults for missing keys."""
        config_path = path or GLOBAL_CONFIG_PATH
        raw: dict[str, Any] = {}
        if config_path.exists():
            with open(config_path) as f:
                loaded = yaml.safe_load(f)
                if isinstance(loaded, dict):
                    raw = loaded

        # Support both flat keys and legacy nested keys
        defaults = cls()
        config = cls(
            port=_get_flat_or_nested(raw, "port", ["server", "port"], defaults.port),
            host=_get_flat_or_nested(raw, "host", ["server", "host"], defaults.host),
            max_concurrent_agents=_get_flat_or_nested(
                raw,
                "max_concurrent_agents",
                ["process", "max_concurrent_agents"],
                defaults.max_concurrent_agents,
            ),
            global_timeout=_get_flat_or_nested(
                raw, "global_timeout", ["process", "global_timeout"], defaults.global_timeout
            ),
            orchestrator_idle_timeout=_get_flat_or_nested(
                raw,
                "orchestrator_idle_timeout",
                ["process", "orchestrator_idle_timeout"],
                defaults.orchestrator_idle_timeout,
            ),
            drain_timeout=_get_flat_or_nested(
                raw, "drain_timeout", ["process", "drain_timeout"], defaults.drain_timeout
            ),
            recovery_probe_interval=_get_flat_or_nested(
                raw,
                "recovery_probe_interval",
                ["rate_limit", "recovery_probe_interval"],
                defaults.recovery_probe_interval,
            ),
            max_retry=_get_flat_or_nested(
                raw, "max_retry", ["queue", "max_retry"], defaults.max_retry
            ),
            event_subscription=raw.get("event_subscription", list(DEFAULT_EVENT_SUBSCRIPTION)),
        )
        config.validate()
        return config

    def validate(self) -> None:
        """Validate config values, logging warnings and resetting to defaults for invalid ones."""
        if not (1 <= self.port <= 65535):
            logger.warning(
                "port=%r is out of range (1-65535); defaulting to %d.",
                self.port,
                DEFAULT_SERVER_PORT,
            )
            self.port = DEFAULT_SERVER_PORT

        if self.max_concurrent_agents <= 0:
            logger.warning(
                "max_concurrent_agents=%r must be > 0; defaulting to %d.",
                self.max_concurrent_agents,
                DEFAULT_CONFIG_MAX_CONCURRENT_AGENTS,
            )
            self.max_concurrent_agents = DEFAULT_CONFIG_MAX_CONCURRENT_AGENTS

        if self.global_timeout <= 0:
            logger.warning(
                "global_timeout=%r must be > 0; defaulting to %d.",
                self.global_timeout,
                DEFAULT_GLOBAL_TIMEOUT_SECONDS,
            )
            self.global_timeout = DEFAULT_GLOBAL_TIMEOUT_SECONDS

        if self.drain_timeout <= 0:
            logger.warning(
                "drain_timeout=%r must be > 0; defaulting to %d.",
                self.drain_timeout,
                DEFAULT_CONFIG_DRAIN_TIMEOUT_SECONDS,
            )
            self.drain_timeout = DEFAULT_CONFIG_DRAIN_TIMEOUT_SECONDS

        if self.max_retry < 0:
            logger.warning(
                "max_retry=%r must be >= 0; defaulting to %d.",
                self.max_retry,
                DEFAULT_CONFIG_MAX_RETRY,
            )
            self.max_retry = DEFAULT_CONFIG_MAX_RETRY

        if self.recovery_probe_interval <= 0:
            logger.warning(
                "recovery_probe_interval=%r must be > 0; defaulting to %d.",
                self.recovery_probe_interval,
                DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS,
            )
            self.recovery_probe_interval = DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS

    def to_dict(self) -> dict[str, Any]:
        """Serialize config to a dict suitable for YAML output."""
        return {
            "port": self.port,
            "host": self.host,
            "max_concurrent_agents": self.max_concurrent_agents,
            "global_timeout": self.global_timeout,
            "orchestrator_idle_timeout": self.orchestrator_idle_timeout,
            "drain_timeout": self.drain_timeout,
            "recovery_probe_interval": self.recovery_probe_interval,
            "max_retry": self.max_retry,
            "event_subscription": self.event_subscription,
        }

    def save(self, path: Path | None = None) -> None:
        """Write config to disk as YAML."""
        config_path = path or GLOBAL_CONFIG_PATH
        config_path.parent.mkdir(parents=True, exist_ok=True)
        with open(config_path, "w") as f:
            yaml.dump(self.to_dict(), f, default_flow_style=False, sort_keys=False)


DEFAULT_ALLOWED_READ_PATHS: list[str] = []
DEFAULT_EXCLUDED_READ_PATHS: list[str] = [
    "~/.githubclaw/secrets/",
    "~/.ssh/",
    "~/.aws/",
]


@dataclass
class RepoConfig:
    """Per-repo configuration loaded from .githubclaw/config.yaml."""

    allowed_read_paths: list[str] = field(default_factory=lambda: list(DEFAULT_ALLOWED_READ_PATHS))
    excluded_read_paths: list[str] = field(
        default_factory=lambda: list(DEFAULT_EXCLUDED_READ_PATHS)
    )
    event_subscription: list[str] = field(default_factory=lambda: list(DEFAULT_EVENT_SUBSCRIPTION))

    @classmethod
    def load(cls, repo_root: Path | None = None) -> RepoConfig:
        """Load per-repo config, falling back to defaults for missing keys."""
        if repo_root is None:
            repo_root = Path.cwd()
        config_path = repo_root / REPO_CONFIG_DIR_NAME / REPO_CONFIG_FILENAME
        raw: dict[str, Any] = {}
        if config_path.exists():
            with open(config_path) as f:
                loaded = yaml.safe_load(f)
                if isinstance(loaded, dict):
                    raw = loaded

        return cls(
            allowed_read_paths=raw.get("allowed_read_paths", list(DEFAULT_ALLOWED_READ_PATHS)),
            excluded_read_paths=raw.get("excluded_read_paths", list(DEFAULT_EXCLUDED_READ_PATHS)),
            event_subscription=raw.get("event_subscription", list(DEFAULT_EVENT_SUBSCRIPTION)),
        )

    def to_dict(self) -> dict[str, Any]:
        """Serialize config to a dict suitable for YAML output."""
        return {
            "allowed_read_paths": self.allowed_read_paths,
            "excluded_read_paths": self.excluded_read_paths,
            "event_subscription": self.event_subscription,
        }

    def save(self, repo_root: Path | None = None) -> None:
        """Write per-repo config to disk as YAML."""
        if repo_root is None:
            repo_root = Path.cwd()
        config_path = repo_root / REPO_CONFIG_DIR_NAME / REPO_CONFIG_FILENAME
        config_path.parent.mkdir(parents=True, exist_ok=True)
        with open(config_path, "w") as f:
            yaml.dump(self.to_dict(), f, default_flow_style=False, sort_keys=False)


def _get_flat_or_nested(
    raw: dict[str, Any],
    flat_key: str,
    nested_path: list[str],
    default: Any,
) -> Any:
    """Look up a value first as a flat key, then via a nested path, then fall back to default."""
    if flat_key in raw:
        return raw[flat_key]
    # Try nested path (e.g. ["server", "port"])
    node: Any = raw
    for segment in nested_path:
        if isinstance(node, dict) and segment in node:
            node = node[segment]
        else:
            return default
    return node


def find_repo_root(start: Path | None = None) -> Path | None:
    """Walk up from start to find a directory containing .git/."""
    current = start or Path.cwd()
    while True:
        if (current / ".git").exists():
            return current
        parent = current.parent
        if parent == current:
            return None
        current = parent


def get_pid_file() -> Path:
    """Return the path to the webhook server PID file."""
    return GLOBAL_CONFIG_DIR / "server.pid"


def get_log_file() -> Path:
    """Return the path to the webhook server log file."""
    return GLOBAL_CONFIG_DIR / "logs" / "webhook_server.log"

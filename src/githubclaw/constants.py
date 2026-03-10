"""Centralised constants for GithubClaw.

All magic numbers and default values are defined here so they can be imported
by any module that needs them.  Changing a value here propagates everywhere.
"""

from __future__ import annotations

# ---------------------------------------------------------------------------
# Server (server.py)
# ---------------------------------------------------------------------------

DEFAULT_MAX_CONCURRENT_AGENTS: int = 8
"""Default cap on simultaneous agent processes when not overridden by env."""

GRACEFUL_DRAIN_TIMEOUT_SECONDS: float = 120
"""Seconds the lifespan waits for running processes during shutdown."""

# ---------------------------------------------------------------------------
# Process manager (process_manager.py)
# ---------------------------------------------------------------------------

DEFAULT_PROCESS_TIMEOUT_SECONDS: int = 7200
"""Default per-process wall-clock timeout (2 hours)."""

MONITOR_CHECK_INTERVAL_SECONDS: int = 2
"""Sleep interval between process-monitor ticks."""

SIGTERM_GRACE_PERIOD_SECONDS: float = 10
"""Seconds to wait after SIGTERM before sending SIGKILL."""

GRACEFUL_DRAIN_POLL_SECONDS: float = 1
"""Sleep between polls while draining active processes."""

DEFAULT_GRACEFUL_DRAIN_SECONDS: float = 300
"""Default timeout passed to ``graceful_drain()``."""

# ---------------------------------------------------------------------------
# Scheduler (scheduler.py)
# ---------------------------------------------------------------------------

SCHEDULER_CHECK_INTERVAL_SECONDS: int = 60
"""Seconds between scheduled-event sweeps."""

MAX_SCHEDULED_EVENT_RETRY_COUNT: int = 5
"""Consecutive failures before a scheduled event is dead-lettered."""

MAX_RESCHEDULE_ITERATIONS: int = 1000
"""Safety cap when advancing a recurring event past *now*."""

# ---------------------------------------------------------------------------
# Orchestrator session (orchestrator/session.py)
# ---------------------------------------------------------------------------

DEFAULT_IDLE_TIMEOUT_SECONDS: int = 300
"""Seconds of inactivity before the orchestrator persists and shuts down."""

IDLE_CHECK_INTERVAL_SECONDS: int = 30
"""How often the idle watchdog checks for inactivity."""

SOCKET_MESSAGE_MAX_BYTES: int = 10 * 1024 * 1024
"""10 MB sanity limit on a single Unix-socket message."""

SOCKET_LENGTH_PREFIX_BYTES: int = 8
"""Size of the big-endian length prefix on the socket protocol."""

CONNECTION_TIMEOUT_SECONDS: float = 5.0
"""Timeout when connecting to an orchestrator Unix socket."""

DEFAULT_ORCHESTRATOR_EVENT_TIMEOUT_SECONDS: float = 120.0
"""Default round-trip timeout for ``send_event_to_orchestrator``."""

# ---------------------------------------------------------------------------
# Queue (queue.py)
# ---------------------------------------------------------------------------

DEFAULT_QUEUE_MAX_RETRY: int = 3
"""Default number of nack retries before an event is dead-lettered."""

QUEUE_FILENAME_LABEL_MAX_LENGTH: int = 60
"""Maximum characters kept from the event-type label in a queue filename."""

# ---------------------------------------------------------------------------
# Config defaults (config.py)
# ---------------------------------------------------------------------------

DEFAULT_SERVER_PORT: int = 8000
"""Default HTTP port for the webhook server."""

DEFAULT_CONFIG_MAX_CONCURRENT_AGENTS: int = 5
"""Default max_concurrent_agents in config.yaml (different from env-var default)."""

DEFAULT_GLOBAL_TIMEOUT_SECONDS: int = 7200
"""Default global timeout for processes in config.yaml."""

DEFAULT_ORCHESTRATOR_IDLE_TIMEOUT_SECONDS: int = 1800
"""Default orchestrator idle timeout in config.yaml."""

DEFAULT_CONFIG_DRAIN_TIMEOUT_SECONDS: int = 300
"""Default drain timeout in config.yaml."""

DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS: int = 300
"""Default rate-limit recovery probe interval in config.yaml."""

DEFAULT_CONFIG_MAX_RETRY: int = 3
"""Default queue max-retry in config.yaml."""

# ---------------------------------------------------------------------------
# SDK abstraction (orchestrator/sdk_abstraction.py)
# ---------------------------------------------------------------------------

MESSAGE_HISTORY_WARNING_THRESHOLD: int = 500
"""Warn when the session message history exceeds this count."""

# ---------------------------------------------------------------------------
# Tools (orchestrator/tools.py)
# ---------------------------------------------------------------------------

GH_CLI_TIMEOUT_SECONDS: float = 30.0
"""Default timeout for ``gh`` CLI subprocess calls."""

SEARCH_QUERY_MAX_LENGTH: int = 500
"""Maximum character length accepted by the ``search_issues`` tool."""

ISSUES_LIST_LIMIT: int = 50
"""Maximum number of issues returned by ``list_issues``."""

PRS_LIST_LIMIT: int = 50
"""Maximum number of PRs returned by ``list_prs``."""

SEARCH_RESULTS_LIMIT: int = 30
"""Maximum number of results returned by ``search_issues``."""

# ---------------------------------------------------------------------------
# Spawner (agents/spawner.py)
# ---------------------------------------------------------------------------

DEFAULT_AGENT_MAX_TURNS: int = 200
"""Default max-turns for a spawned agent CLI process."""

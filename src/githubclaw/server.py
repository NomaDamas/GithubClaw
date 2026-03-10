"""GithubClaw webhook server -- FastAPI entry point for uvicorn.

Receives GitHub webhook events, verifies signatures, applies the fork PR gate,
routes events into per-repo disk-persisted queues, and manages the process
lifecycle for orchestrators and workers.

Run with::

    uvicorn githubclaw.server:app --host 0.0.0.0 --port 8742
"""

from __future__ import annotations

import json
import logging
import os
from contextlib import asynccontextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from fastapi import FastAPI, Header, HTTPException, Request, Response

from githubclaw.constants import (
    DEFAULT_MAX_CONCURRENT_AGENTS,
    GRACEFUL_DRAIN_TIMEOUT_SECONDS,
)
from githubclaw.errors import ConfigError
from githubclaw.process_manager import (
    ManagedProcess,
    ProcessManager,
)
from githubclaw.queue import DiskPersistedQueue
from githubclaw.scheduler import ScheduledEventManager
from githubclaw.signature import verify_webhook_signature

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Configuration helpers
# ---------------------------------------------------------------------------

GITHUBCLAW_HOME = Path(os.environ.get("GITHUBCLAW_HOME", Path.home() / ".githubclaw"))
WEBHOOK_SECRET_PATH = GITHUBCLAW_HOME / "secrets" / "webhook_secret"
REGISTRY_PATH = GITHUBCLAW_HOME / "registry.json"

MAX_CONCURRENT_AGENTS = int(
    os.environ.get("GITHUBCLAW_MAX_AGENTS", str(DEFAULT_MAX_CONCURRENT_AGENTS))
)


def _load_webhook_secret() -> str:
    """Read the webhook secret from the secrets directory."""
    try:
        return WEBHOOK_SECRET_PATH.read_text(encoding="utf-8").strip()
    except FileNotFoundError as err:
        logger.error("Webhook secret not found at %s", WEBHOOK_SECRET_PATH)
        raise ConfigError(
            f"Webhook secret file missing: {WEBHOOK_SECRET_PATH}. "
            "Create it during GitHub App setup."
        ) from err


def _load_registry() -> dict[str, dict[str, Any]]:
    """Load the repo registry mapping ``full_name -> config``."""
    if not REGISTRY_PATH.exists():
        logger.warning("Registry file not found at %s -- no repos registered", REGISTRY_PATH)
        return {}
    try:
        data = json.loads(REGISTRY_PATH.read_text(encoding="utf-8"))
        if isinstance(data, list):
            # Support list-of-objects format: [{"full_name": "owner/repo", ...}]
            result: dict[str, dict[str, Any]] = {}
            for entry in data:
                if not isinstance(entry, dict) or "full_name" not in entry:
                    logger.warning(
                        "Skipping malformed registry entry (missing 'full_name'): %s",
                        entry,
                    )
                    continue
                result[entry["full_name"]] = entry
            return result
        # Support nested {"repos": {...}} format from DIRECTORY_LAYOUT.md
        if isinstance(data, dict) and "repos" in data:
            return data["repos"]
        return data
    except (json.JSONDecodeError, KeyError, TypeError) as exc:
        logger.error("Failed to parse registry.json: %s", exc)
        return {}


# ---------------------------------------------------------------------------
# Server state (initialised in lifespan, attached to app.state.githubclaw)
# ---------------------------------------------------------------------------


@dataclass
class ServerState:
    """All mutable state for the running webhook server."""

    webhook_secret: str = ""
    registry: dict[str, dict[str, Any]] = field(default_factory=dict)
    queues: dict[str, DiskPersistedQueue] = field(default_factory=dict)
    process_manager: ProcessManager | None = None
    scheduler: ScheduledEventManager | None = None


def _get_state(request: Request) -> ServerState:
    """Retrieve the ``ServerState`` from an incoming request."""
    return request.app.state.githubclaw


def _get_state_from_app(app: FastAPI) -> ServerState:
    """Retrieve the ``ServerState`` directly from the app (for non-request contexts)."""
    return app.state.githubclaw


def _get_queue(state: ServerState, repo_full_name: str) -> DiskPersistedQueue:
    """Return (or create) the disk-persisted queue for *repo_full_name*."""
    if repo_full_name not in state.queues:
        repo_config = state.registry.get(repo_full_name, {})
        local_path = repo_config.get("local_path")
        if local_path:
            queue_dir = Path(local_path) / ".githubclaw" / "queue"
        else:
            # Fallback if local_path is not set in registry
            slug = repo_full_name.replace("/", "_")
            queue_dir = GITHUBCLAW_HOME / "queues" / slug / "queue"
            logger.warning(
                "No local_path in registry for %s, falling back to %s",
                repo_full_name,
                queue_dir,
            )
        state.queues[repo_full_name] = DiskPersistedQueue(queue_dir)
    return state.queues[repo_full_name]


# ---------------------------------------------------------------------------
# Callbacks for ProcessManager
# ---------------------------------------------------------------------------

# The callbacks below receive the app instance via a closure created in the
# lifespan so they can look up ``ServerState`` without module globals.


def _make_on_process_crash(the_app: FastAPI):
    async def _on_process_crash(managed: ManagedProcess) -> None:
        """Inject a failure event into the orchestrator queue for the repo."""
        logger.error("Injecting failure event for crashed process [%s]", managed.label)
        state = _get_state_from_app(the_app)
        queue = _get_queue(state, managed.repo)
        queue.enqueue(
            {
                "type": "failure_injection",
                "source": "process_manager",
                "label": managed.label,
                "exit_code": managed.exit_code,
                "kind": managed.kind.value,
            },
            event_type="failure_injection",
        )

    return _on_process_crash


def _make_on_process_timeout(the_app: FastAPI):
    async def _on_process_timeout(managed: ManagedProcess) -> None:
        """Inject a timeout failure event."""
        logger.warning("Injecting timeout event for process [%s]", managed.label)
        state = _get_state_from_app(the_app)
        queue = _get_queue(state, managed.repo)
        queue.enqueue(
            {
                "type": "failure_injection",
                "source": "process_manager",
                "reason": "timeout",
                "label": managed.label,
                "kind": managed.kind.value,
            },
            event_type="failure_injection",
        )

    return _on_process_timeout


def _make_on_process_exit(the_app: FastAPI):
    async def _on_process_exit(managed: ManagedProcess) -> None:
        """Log clean exit."""
        logger.info("Process [%s] exited cleanly", managed.label)

    return _on_process_exit


# ---------------------------------------------------------------------------
# Scheduled event callback
# ---------------------------------------------------------------------------


def _make_on_scheduled_fire(the_app: FastAPI):
    async def _on_scheduled_fire(repo: str, payload: dict[str, Any]) -> None:
        """Inject a scheduled event into the repo's queue."""
        state = _get_state_from_app(the_app)
        queue = _get_queue(state, repo)
        queue.enqueue(
            {
                "type": "scheduled_fired",
                "scheduled_payload": payload,
            },
            event_type="scheduled_fired",
        )
        logger.info("Scheduled event injected into queue for %s", repo)

    return _on_scheduled_fire


# ---------------------------------------------------------------------------
# Lifespan
# ---------------------------------------------------------------------------


@asynccontextmanager
async def lifespan(app: FastAPI):
    """Application lifespan: initialise server state on startup, tear down on shutdown."""
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
    )

    state = ServerState(
        webhook_secret=_load_webhook_secret(),
        registry=_load_registry(),
    )
    logger.info("Loaded registry with %d repo(s)", len(state.registry))

    state.process_manager = ProcessManager(
        max_concurrent_agents=MAX_CONCURRENT_AGENTS,
        on_crash=_make_on_process_crash(app),
        on_timeout=_make_on_process_timeout(app),
        on_exit=_make_on_process_exit(app),
    )
    state.process_manager.start_monitor()

    state.scheduler = ScheduledEventManager(on_fire=_make_on_scheduled_fire(app))
    state.scheduler.start()

    # Attach to app.state so endpoints and public helpers can access it.
    app.state.githubclaw = state

    yield  # ---- application runs ----

    # Shutdown
    logger.info("Shutting down GithubClaw webhook server...")
    if state.scheduler:
        await state.scheduler.stop()
    if state.process_manager:
        await state.process_manager.graceful_drain(timeout=GRACEFUL_DRAIN_TIMEOUT_SECONDS)
        await state.process_manager.stop()


# ---------------------------------------------------------------------------
# FastAPI app
# ---------------------------------------------------------------------------

app = FastAPI(
    title="GithubClaw Webhook Server",
    description="Receives GitHub webhook events and routes them to per-repo queues.",
    version="0.1.0",
    lifespan=lifespan,
)


# ---------------------------------------------------------------------------
# Fork PR gate helpers
# ---------------------------------------------------------------------------


def _annotate_fork_status(payload: dict[str, Any]) -> dict[str, Any]:
    """Add a ``_githubclaw_fork_unapproved`` flag to the payload when the
    event is a fork PR without the ``githubclaw-approved`` label.

    This annotation flows through the queue so the orchestrator and dispatch
    logic can act on it.
    """
    pr = payload.get("pull_request", {})
    if not pr:
        return payload

    head_repo = pr.get("head", {}).get("repo", {})
    if not head_repo.get("fork", False):
        return payload

    labels = [label["name"] for label in pr.get("labels", [])]
    if "githubclaw-approved" not in labels:
        payload["_githubclaw_fork_unapproved"] = True
        logger.info(
            "Fork PR #%s from %s queued as unapproved",
            pr.get("number", "?"),
            head_repo.get("full_name", "?"),
        )

    return payload


# ---------------------------------------------------------------------------
# Endpoint
# ---------------------------------------------------------------------------


@app.post("/webhook")
async def webhook(
    request: Request,
    x_hub_signature_256: str | None = Header(None),
    x_github_event: str | None = Header(None),
    x_github_delivery: str | None = Header(None),
) -> Response:
    """Receive a GitHub webhook event.

    1. Verify ``X-Hub-Signature-256``.
    2. Discard events from repos not in ``registry.json``.
    3. Annotate fork PR status.
    4. Enqueue the event to the per-repo disk-persisted queue.
    """
    state = _get_state(request)

    # -- Signature verification ------------------------------------------------
    body = await request.body()

    if not x_hub_signature_256:
        raise HTTPException(status_code=403, detail="Missing X-Hub-Signature-256 header")

    if not verify_webhook_signature(body, x_hub_signature_256, state.webhook_secret):
        logger.warning("Invalid webhook signature (delivery=%s)", x_github_delivery)
        raise HTTPException(status_code=403, detail="Invalid signature")

    # -- Parse payload ---------------------------------------------------------
    try:
        payload: dict[str, Any] = json.loads(body)
    except json.JSONDecodeError as err:
        raise HTTPException(status_code=400, detail="Invalid JSON payload") from err

    # -- Registry check --------------------------------------------------------
    repo_info = payload.get("repository", {})
    repo_full_name: str = repo_info.get("full_name", "")

    if not repo_full_name or repo_full_name not in state.registry:
        logger.debug(
            "Discarding event for unregistered repo: %s (event=%s)",
            repo_full_name,
            x_github_event,
        )
        return Response(status_code=200, content="Ignored: repo not registered")

    # -- Fork PR annotation ----------------------------------------------------
    payload = _annotate_fork_status(payload)

    # -- Enqueue ---------------------------------------------------------------
    event_label = x_github_event or "unknown"
    action = payload.get("action", "")
    if action:
        event_label = f"{event_label}_{action}"

    queue = _get_queue(state, repo_full_name)
    queue.enqueue(payload, event_type=event_label)

    logger.info(
        "Queued event %s for %s (delivery=%s, queue_size=%d)",
        event_label,
        repo_full_name,
        x_github_delivery,
        queue.size(),
    )

    return Response(status_code=202, content="Event queued")


# ---------------------------------------------------------------------------
# Health / status (useful for tunnel probes)
# ---------------------------------------------------------------------------


@app.get("/health")
async def health(request: Request) -> dict[str, Any]:
    state = _get_state(request)
    return {
        "status": "ok",
        "registered_repos": len(state.registry),
        "active_processes": state.process_manager.active_count if state.process_manager else 0,
    }


# ---------------------------------------------------------------------------
# Programmatic access for other GithubClaw components
# ---------------------------------------------------------------------------


def get_process_manager() -> ProcessManager:
    """Return the ProcessManager instance from the running app.

    .. note:: This accesses ``app.state`` -- the server must be started.
    """
    state = _get_state_from_app(app)
    assert state.process_manager is not None, "Server not started"
    return state.process_manager


def get_scheduler() -> ScheduledEventManager:
    """Return the ScheduledEventManager instance from the running app.

    .. note:: This accesses ``app.state`` -- the server must be started.
    """
    state = _get_state_from_app(app)
    assert state.scheduler is not None, "Server not started"
    return state.scheduler


def get_queue_for_repo(repo_full_name: str) -> DiskPersistedQueue:
    """Return the queue for a given repo.

    .. note:: This accesses ``app.state`` -- the server must be started.
    """
    state = _get_state_from_app(app)
    return _get_queue(state, repo_full_name)

"""Asyncio-based scheduled event manager.

Persists events to ``~/.githubclaw/scheduled.json`` and checks every 60
seconds for due events.  Supports both recurring and one-shot events.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import uuid
from dataclasses import asdict, dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from collections.abc import Callable, Coroutine

from githubclaw.constants import (
    MAX_RESCHEDULE_ITERATIONS,
    MAX_SCHEDULED_EVENT_RETRY_COUNT,
    SCHEDULER_CHECK_INTERVAL_SECONDS,
)

logger = logging.getLogger(__name__)

SCHEDULED_JSON = Path.home() / ".githubclaw" / "scheduled.json"
CHECK_INTERVAL_SECONDS = SCHEDULER_CHECK_INTERVAL_SECONDS


@dataclass
class ScheduledEvent:
    """A single scheduled event definition."""

    event_id: str
    repo: str
    trigger_at: str  # ISO-8601 UTC timestamp for next fire
    payload: dict[str, Any]
    one_shot: bool = True
    interval_seconds: int | None = None  # for recurring events
    description: str = ""
    retry_count: int = 0

    def trigger_datetime(self) -> datetime:
        return datetime.fromisoformat(self.trigger_at).replace(tzinfo=UTC)

    def is_due(self, now: datetime | None = None) -> bool:
        now = now or datetime.now(UTC)
        return self.trigger_datetime() <= now


class ScheduledEventManager:
    """Manages scheduled events persisted to disk.

    Call :meth:`start` to begin the asyncio background loop.  Events that
    become due are delivered via the *on_fire* callback provided at
    construction.
    """

    def __init__(
        self,
        on_fire: Callable[[str, dict[str, Any]], Coroutine[Any, Any, None]],
        json_path: str | Path | None = None,
    ) -> None:
        """
        Args:
            on_fire: Async callback ``(repo, payload) -> None`` invoked when
                     a scheduled event fires.
            json_path: Override path for the JSON persistence file (useful
                       for testing).
        """
        self._on_fire = on_fire
        self._json_path = Path(json_path) if json_path else SCHEDULED_JSON
        self._events: list[ScheduledEvent] = []
        self._task: asyncio.Task[None] | None = None
        self._running = False

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _ensure_dir(self) -> None:
        self._json_path.parent.mkdir(parents=True, exist_ok=True)

    def load(self) -> None:
        """Load scheduled events from disk."""
        self._ensure_dir()
        if not self._json_path.exists():
            self._events = []
            return

        try:
            data = json.loads(self._json_path.read_text(encoding="utf-8"))
            self._events = [ScheduledEvent(**item) for item in data]
            logger.info("Loaded %d scheduled event(s) from %s", len(self._events), self._json_path)
        except (json.JSONDecodeError, TypeError, KeyError) as exc:
            logger.error("Failed to load scheduled events: %s", exc)
            self._events = []

    def save(self) -> None:
        """Persist current events list to disk."""
        self._ensure_dir()
        data = [asdict(ev) for ev in self._events]
        tmp_path = self._json_path.with_suffix(".tmp")
        tmp_path.write_text(json.dumps(data, indent=2), encoding="utf-8")
        tmp_path.replace(self._json_path)

    # ------------------------------------------------------------------
    # CRUD
    # ------------------------------------------------------------------

    def create_event(
        self,
        repo: str,
        trigger_at: datetime,
        payload: dict[str, Any],
        one_shot: bool = True,
        interval_seconds: int | None = None,
        description: str = "",
    ) -> str:
        """Create and persist a new scheduled event.

        Returns:
            The generated ``event_id``.
        """
        # Validate interval_seconds for recurring events
        if not one_shot and interval_seconds is None:
            raise ValueError("Recurring events (one_shot=False) require interval_seconds to be set")
        if interval_seconds is not None and interval_seconds <= 0:
            raise ValueError(f"interval_seconds must be positive, got {interval_seconds}")

        event_id = uuid.uuid4().hex[:12]
        event = ScheduledEvent(
            event_id=event_id,
            repo=repo,
            trigger_at=trigger_at.isoformat(),
            payload=payload,
            one_shot=one_shot,
            interval_seconds=interval_seconds,
            description=description,
        )
        self._events.append(event)
        self.save()
        logger.info("Created scheduled event %s for repo %s (due %s)", event_id, repo, trigger_at)
        return event_id

    def cancel_event(self, event_id: str) -> bool:
        """Cancel a scheduled event by its ID.

        Returns:
            True if the event was found and removed.
        """
        before = len(self._events)
        self._events = [ev for ev in self._events if ev.event_id != event_id]
        removed = len(self._events) < before
        if removed:
            self.save()
            logger.info("Cancelled scheduled event %s", event_id)
        else:
            logger.warning("Scheduled event %s not found for cancellation", event_id)
        return removed

    def list_events(self) -> list[ScheduledEvent]:
        """Return a copy of the current events list."""
        return list(self._events)

    # ------------------------------------------------------------------
    # Firing logic
    # ------------------------------------------------------------------

    async def fire_due_events(self) -> int:
        """Check all events and fire those that are due.

        For one-shot events the entry is removed after firing.
        For recurring events the ``trigger_at`` is advanced by
        ``interval_seconds``.

        Returns:
            The number of events fired.
        """
        now = datetime.now(UTC)
        fired = 0
        remaining: list[ScheduledEvent] = []

        for event in self._events:
            if event.is_due(now):
                try:
                    await self._on_fire(event.repo, event.payload)
                    fired += 1
                    event.retry_count = 0
                    logger.info("Fired scheduled event %s for repo %s", event.event_id, event.repo)
                except Exception:
                    event.retry_count += 1
                    if event.retry_count >= MAX_SCHEDULED_EVENT_RETRY_COUNT:
                        logger.error(
                            "Dead-lettering scheduled event %s for repo %s after %d consecutive failures",
                            event.event_id,
                            event.repo,
                            event.retry_count,
                        )
                        # Do not re-add -- event is dead-lettered
                    else:
                        logger.exception(
                            "Error firing scheduled event %s (retry %d/%d)",
                            event.event_id,
                            event.retry_count,
                            MAX_SCHEDULED_EVENT_RETRY_COUNT,
                        )
                        remaining.append(event)
                    continue

                if event.one_shot:
                    # Do not re-add
                    continue

                # Recurring: advance trigger_at
                if event.interval_seconds:
                    next_trigger = datetime.fromisoformat(event.trigger_at).replace(tzinfo=UTC)
                    # Advance past *now* to avoid rapid repeat fires
                    iterations = 0
                    while next_trigger <= now:
                        next_trigger += timedelta(seconds=event.interval_seconds)
                        iterations += 1
                        if iterations >= MAX_RESCHEDULE_ITERATIONS:
                            logger.warning(
                                "Capped recurring advance at %d iterations for event %s",
                                MAX_RESCHEDULE_ITERATIONS,
                                event.event_id,
                            )
                            break
                    event.trigger_at = next_trigger.isoformat()
                    remaining.append(event)
            else:
                remaining.append(event)

        self._events = remaining
        if fired:
            self.save()
        return fired

    # ------------------------------------------------------------------
    # Background loop
    # ------------------------------------------------------------------

    async def _loop(self) -> None:
        """Background loop that checks for due events every 60 seconds."""
        while self._running:
            try:
                await self.fire_due_events()
            except Exception:
                logger.exception("Unhandled error in scheduled event loop")
            await asyncio.sleep(CHECK_INTERVAL_SECONDS)

    def start(self) -> None:
        """Start the background check loop as an asyncio task."""
        if self._task is not None and not self._task.done():
            logger.warning("Scheduled event loop already running")
            return
        self.load()
        self._running = True
        self._task = asyncio.get_event_loop().create_task(self._loop())
        logger.info("Scheduled event loop started (interval=%ds)", CHECK_INTERVAL_SECONDS)

    async def stop(self) -> None:
        """Stop the background loop gracefully."""
        self._running = False
        if self._task is not None:
            self._task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._task
            self._task = None
        logger.info("Scheduled event loop stopped")

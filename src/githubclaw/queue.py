"""Disk-persisted FIFO event queue with dead-letter support.

Queue directory layout per repo:
    .githubclaw/queue/
        001_issues_opened.json
        002_issue_comment_created.json
        ...
    .githubclaw/queue/dead/
        failed_event_after_max_retry.json
        ...
"""

from __future__ import annotations

import json
import logging
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from githubclaw.constants import DEFAULT_QUEUE_MAX_RETRY, QUEUE_FILENAME_LABEL_MAX_LENGTH

logger = logging.getLogger(__name__)


@dataclass
class QueuedEvent:
    """An event stored on disk inside the queue."""

    sequence: int
    filename: str
    payload: dict[str, Any]
    retry_count: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "sequence": self.sequence,
            "payload": self.payload,
            "retry_count": self.retry_count,
        }


class DiskPersistedQueue:
    """FIFO event queue persisted to a per-repo directory on disk.

    Events are written as sequentially-numbered JSON files.  Dequeue reads the
    lowest-numbered file, processes it, and removes it.  Events that exceed
    ``max_retry`` are moved to a ``dead/`` subdirectory.
    """

    def __init__(self, queue_dir: str | Path, max_retry: int = DEFAULT_QUEUE_MAX_RETRY) -> None:
        self.queue_dir = Path(queue_dir)
        self.dead_dir = self.queue_dir / "dead"
        self.max_retry = max_retry

        # Ensure directories exist
        self.queue_dir.mkdir(parents=True, exist_ok=True)
        self.dead_dir.mkdir(parents=True, exist_ok=True)

        # Clean up stale .tmp files left by crashes between write and rename
        for f in self.queue_dir.glob("*.tmp"):
            f.unlink(missing_ok=True)

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _next_sequence(self) -> int:
        """Return the next sequence number based on existing files."""
        existing = self._sorted_event_files()
        if not existing:
            return 1
        # Parse the leading integer from the last filename
        last = existing[-1].stem
        try:
            seq = int(last.split("_", 1)[0])
        except (ValueError, IndexError):
            seq = 0
        return seq + 1

    def _sorted_event_files(self) -> list[Path]:
        """Return event JSON files sorted by filename (i.e. sequence order)."""
        if not self.queue_dir.exists():
            return []
        files = [f for f in self.queue_dir.iterdir() if f.suffix == ".json" and f.is_file()]
        files.sort(key=lambda p: p.name)
        return files

    @staticmethod
    def _safe_label(event_type: str) -> str:
        """Sanitise an event type string for use in a filename."""
        return (
            event_type.replace(".", "_")
            .replace("/", "_")
            .replace(" ", "_")[:QUEUE_FILENAME_LABEL_MAX_LENGTH]
        )

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def enqueue(self, payload: dict[str, Any], event_type: str = "event") -> Path:
        """Persist *payload* as the next event in the queue.

        Args:
            payload: The full event payload dict.
            event_type: A human-readable label embedded in the filename
                        (e.g. ``"issues_opened"``).

        Returns:
            The ``Path`` of the written file.
        """
        seq = self._next_sequence()
        label = self._safe_label(event_type)
        filename = f"{seq:06d}_{label}.json"
        filepath = self.queue_dir / filename

        envelope: dict[str, Any] = {
            "sequence": seq,
            "payload": payload,
            "retry_count": 0,
        }

        # Atomic-ish write: write to a temp file then rename
        tmp_path = filepath.with_suffix(".tmp")
        tmp_path.write_text(json.dumps(envelope, indent=2), encoding="utf-8")
        os.replace(str(tmp_path), str(filepath))

        logger.info("Enqueued event %s -> %s", seq, filepath.name)
        return filepath

    def peek(self) -> QueuedEvent | None:
        """Return the next event without removing it, or ``None`` if empty."""
        files = self._sorted_event_files()
        if not files:
            return None
        return self._load_event(files[0])

    def dequeue(self) -> QueuedEvent | None:
        """Remove and return the next event, or ``None`` if empty."""
        files = self._sorted_event_files()
        if not files:
            return None

        event = self._load_event(files[0])
        files[0].unlink(missing_ok=True)
        logger.info("Dequeued event %s (%s)", event.sequence, files[0].name)
        return event

    def nack(self, event: QueuedEvent, event_type: str = "event") -> None:
        """Re-enqueue *event* with an incremented retry count.

        If the retry count exceeds ``max_retry``, the event is moved to the
        dead-letter directory instead.
        """
        event.retry_count += 1

        if event.retry_count > self.max_retry:
            self._move_to_dead_letter(event, event_type)
            return

        # Re-enqueue at the back of the queue
        seq = self._next_sequence()
        label = self._safe_label(event_type)
        filename = f"{seq:06d}_{label}.json"
        filepath = self.queue_dir / filename

        envelope: dict[str, Any] = {
            "sequence": seq,
            "payload": event.payload,
            "retry_count": event.retry_count,
        }

        tmp_path = filepath.with_suffix(".tmp")
        tmp_path.write_text(json.dumps(envelope, indent=2), encoding="utf-8")
        os.replace(str(tmp_path), str(filepath))
        logger.info(
            "Nacked event (retry %d/%d) -> %s",
            event.retry_count,
            self.max_retry,
            filepath.name,
        )

    def size(self) -> int:
        """Return the number of events currently in the queue."""
        return len(self._sorted_event_files())

    def is_empty(self) -> bool:
        return self.size() == 0

    # ------------------------------------------------------------------
    # Dead-letter
    # ------------------------------------------------------------------

    def _move_to_dead_letter(self, event: QueuedEvent, event_type: str = "event") -> Path:
        label = self._safe_label(event_type)
        filename = f"{event.sequence:06d}_{label}_dead.json"
        filepath = self.dead_dir / filename

        envelope: dict[str, Any] = {
            "sequence": event.sequence,
            "payload": event.payload,
            "retry_count": event.retry_count,
        }
        filepath.write_text(json.dumps(envelope, indent=2), encoding="utf-8")
        logger.warning("Event %s moved to dead-letter queue: %s", event.sequence, filepath.name)
        return filepath

    def dead_letter_count(self) -> int:
        """Return the number of events in the dead-letter directory."""
        if not self.dead_dir.exists():
            return 0
        return sum(1 for f in self.dead_dir.iterdir() if f.suffix == ".json" and f.is_file())

    # ------------------------------------------------------------------
    # Loader
    # ------------------------------------------------------------------

    def _load_event(self, path: Path) -> QueuedEvent:
        data = json.loads(path.read_text(encoding="utf-8"))
        return QueuedEvent(
            sequence=data.get("sequence", 0),
            filename=path.name,
            payload=data.get("payload", {}),
            retry_count=data.get("retry_count", 0),
        )

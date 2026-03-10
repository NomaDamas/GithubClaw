"""Flat process tree manager for orchestrators and worker agents.

All child processes (orchestrators and workers) are managed as siblings of the
webhook server process.  The manager handles spawning, monitoring exit codes,
idle timeouts, crash detection, concurrency throttling, and the fork PR gate.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import signal
import time
from dataclasses import dataclass, field
from enum import StrEnum
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from collections.abc import Callable, Coroutine

from githubclaw.constants import (
    DEFAULT_GRACEFUL_DRAIN_SECONDS,
    DEFAULT_MAX_CONCURRENT_AGENTS,
    DEFAULT_PROCESS_TIMEOUT_SECONDS,
    GRACEFUL_DRAIN_POLL_SECONDS,
    MONITOR_CHECK_INTERVAL_SECONDS,
    SIGTERM_GRACE_PERIOD_SECONDS,
)

logger = logging.getLogger(__name__)


class ProcessKind(StrEnum):
    ORCHESTRATOR = "orchestrator"
    WORKER = "worker"


class ProcessState(StrEnum):
    RUNNING = "running"
    FINISHED = "finished"
    CRASHED = "crashed"
    TIMED_OUT = "timed_out"
    KILLED = "killed"


@dataclass
class ManagedProcess:
    """Bookkeeping record for a child process."""

    pid: int
    process: asyncio.subprocess.Process
    kind: ProcessKind
    repo: str
    label: str  # e.g. "coder-repoA-issue42"
    started_at: float = field(default_factory=time.monotonic)
    timeout_seconds: int = DEFAULT_PROCESS_TIMEOUT_SECONDS
    state: ProcessState = ProcessState.RUNNING
    exit_code: int | None = None


@dataclass
class PendingDispatch:
    """A queued dispatch waiting for a free concurrency slot."""

    cmd: str
    args: list[str]
    kind: ProcessKind
    repo: str
    label: str
    timeout_seconds: int


# Read-only agent types exempt from the fork PR gate
_READ_ONLY_AGENT_TYPES = frozenset({"security_reviewer"})


def check_fork_pr_gate(event_payload: dict[str, Any], agent_type: str) -> bool:
    """Return True if the dispatch is allowed, False if it should be blocked.

    Enforcement rules (from SECURITY.md):
      - Not a fork PR -> allow
      - Fork PR with ``githubclaw-approved`` label -> allow
      - Read-only agents (e.g. security_reviewer) -> always allow
      - Otherwise -> block
    """
    pr = event_payload.get("pull_request", {})
    if not pr:
        return True  # Not a PR event at all

    head_repo = pr.get("head", {}).get("repo", {})
    if not head_repo.get("fork", False):
        return True  # Not from a fork

    labels = [label["name"] for label in pr.get("labels", [])]
    if "githubclaw-approved" in labels:
        return True

    return agent_type in _READ_ONLY_AGENT_TYPES


class ProcessManager:
    """Manages all child processes as a flat sibling tree.

    Provides spawn, monitor, idle-timeout, concurrency throttle, graceful
    drain, and force kill capabilities.
    """

    def __init__(
        self,
        max_concurrent_agents: int = DEFAULT_MAX_CONCURRENT_AGENTS,
        on_crash: Callable[[ManagedProcess], Coroutine[Any, Any, None]] | None = None,
        on_timeout: Callable[[ManagedProcess], Coroutine[Any, Any, None]] | None = None,
        on_exit: Callable[[ManagedProcess], Coroutine[Any, Any, None]] | None = None,
    ) -> None:
        self.max_concurrent_agents = max_concurrent_agents
        self._on_crash = on_crash
        self._on_timeout = on_timeout
        self._on_exit = on_exit

        self._processes: dict[int, ManagedProcess] = {}
        self._monitor_task: asyncio.Task[None] | None = None
        self._running = False

        # Queued dispatches waiting for a free slot
        self._pending_dispatches: asyncio.Queue[PendingDispatch] = asyncio.Queue()

    # ------------------------------------------------------------------
    # Properties
    # ------------------------------------------------------------------

    @property
    def active_count(self) -> int:
        return sum(1 for p in self._processes.values() if p.state == ProcessState.RUNNING)

    @property
    def active_processes(self) -> list[ManagedProcess]:
        return [p for p in self._processes.values() if p.state == ProcessState.RUNNING]

    def has_capacity(self) -> bool:
        return self.active_count < self.max_concurrent_agents

    # ------------------------------------------------------------------
    # Spawn
    # ------------------------------------------------------------------

    async def spawn(
        self,
        cmd: str,
        args: list[str],
        kind: ProcessKind,
        repo: str,
        label: str,
        timeout_seconds: int = DEFAULT_PROCESS_TIMEOUT_SECONDS,
        env: dict[str, str] | None = None,
        cwd: str | None = None,
    ) -> ManagedProcess | None:
        """Spawn a child process.

        If the concurrency limit has been reached the dispatch is queued and
        ``None`` is returned.  The queued dispatch will be executed
        automatically when a slot frees up.
        """
        if not self.has_capacity():
            logger.warning(
                "Concurrency limit (%d) reached, queueing dispatch: %s",
                self.max_concurrent_agents,
                label,
            )
            self._pending_dispatches.put_nowait(
                PendingDispatch(
                    cmd=cmd,
                    args=args,
                    kind=kind,
                    repo=repo,
                    label=label,
                    timeout_seconds=timeout_seconds,
                )
            )
            return None

        return await self._do_spawn(cmd, args, kind, repo, label, timeout_seconds, env, cwd)

    async def _do_spawn(
        self,
        cmd: str,
        args: list[str],
        kind: ProcessKind,
        repo: str,
        label: str,
        timeout_seconds: int,
        env: dict[str, str] | None = None,
        cwd: str | None = None,
    ) -> ManagedProcess:
        import os

        merged_env = {**os.environ, **(env or {})}

        proc = await asyncio.create_subprocess_exec(
            cmd,
            *args,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=merged_env,
            cwd=cwd,
        )

        managed = ManagedProcess(
            pid=proc.pid,
            process=proc,
            kind=kind,
            repo=repo,
            label=label,
            timeout_seconds=timeout_seconds,
        )
        self._processes[proc.pid] = managed
        logger.info("Spawned %s process [%s] pid=%d", kind.value, label, proc.pid)
        return managed

    # ------------------------------------------------------------------
    # Monitoring loop
    # ------------------------------------------------------------------

    async def _monitor_loop(self) -> None:
        """Periodically check child processes for exit / timeout."""
        while self._running:
            now = time.monotonic()
            for managed in list(self._processes.values()):
                if managed.state != ProcessState.RUNNING:
                    continue

                # Check if process has exited
                retcode = managed.process.returncode
                if retcode is not None:
                    managed.exit_code = retcode
                    if retcode == 0:
                        managed.state = ProcessState.FINISHED
                        logger.info(
                            "Process [%s] pid=%d finished (exit 0)", managed.label, managed.pid
                        )
                        if self._on_exit:
                            await self._on_exit(managed)
                    else:
                        managed.state = ProcessState.CRASHED
                        logger.error(
                            "Process [%s] pid=%d crashed (exit %d)",
                            managed.label,
                            managed.pid,
                            retcode,
                        )
                        if self._on_crash:
                            await self._on_crash(managed)

                    # Try to drain a pending dispatch now that a slot is free
                    await self._drain_one_pending()
                    continue

                # Check timeout
                elapsed = now - managed.started_at
                if elapsed > managed.timeout_seconds:
                    logger.warning(
                        "Process [%s] pid=%d timed out after %.0fs",
                        managed.label,
                        managed.pid,
                        elapsed,
                    )
                    await self._kill_process(managed)
                    managed.state = ProcessState.TIMED_OUT
                    if self._on_timeout:
                        await self._on_timeout(managed)
                    await self._drain_one_pending()

            await asyncio.sleep(MONITOR_CHECK_INTERVAL_SECONDS)

    async def _drain_one_pending(self) -> None:
        """If there is a queued dispatch and capacity, spawn it."""
        if self._pending_dispatches.empty() or not self.has_capacity():
            return
        try:
            pending = self._pending_dispatches.get_nowait()
        except asyncio.QueueEmpty:
            return
        logger.info("Draining pending dispatch: %s", pending.label)
        await self._do_spawn(
            pending.cmd,
            pending.args,
            pending.kind,
            pending.repo,
            pending.label,
            pending.timeout_seconds,
        )

    # ------------------------------------------------------------------
    # Kill helpers
    # ------------------------------------------------------------------

    @staticmethod
    async def _kill_process(managed: ManagedProcess) -> None:
        """Send SIGTERM then SIGKILL after a grace period."""
        try:
            managed.process.send_signal(signal.SIGTERM)
        except ProcessLookupError:
            return
        try:
            await asyncio.wait_for(managed.process.wait(), timeout=SIGTERM_GRACE_PERIOD_SECONDS)
        except TimeoutError:
            with contextlib.suppress(ProcessLookupError):
                managed.process.kill()
        managed.exit_code = managed.process.returncode

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    def start_monitor(self) -> None:
        """Start the process monitoring background task."""
        if self._monitor_task is not None and not self._monitor_task.done():
            return
        self._running = True
        self._monitor_task = asyncio.get_event_loop().create_task(self._monitor_loop())
        logger.info("Process monitor started")

    async def graceful_drain(self, timeout: float = DEFAULT_GRACEFUL_DRAIN_SECONDS) -> None:
        """Wait for all running processes to finish, up to *timeout* seconds.

        After the timeout, any remaining processes are killed.
        """
        logger.info(
            "Graceful drain: waiting up to %.0fs for %d process(es)", timeout, self.active_count
        )
        deadline = time.monotonic() + timeout

        while self.active_count > 0 and time.monotonic() < deadline:
            await asyncio.sleep(GRACEFUL_DRAIN_POLL_SECONDS)

        # Kill stragglers
        for managed in list(self._processes.values()):
            if managed.state == ProcessState.RUNNING:
                logger.warning("Force-killing straggler [%s] pid=%d", managed.label, managed.pid)
                await self._kill_process(managed)
                managed.state = ProcessState.KILLED

    async def force_kill_all(self) -> None:
        """Immediately kill all running child processes."""
        logger.warning("Force-killing all %d running process(es)", self.active_count)
        for managed in list(self._processes.values()):
            if managed.state == ProcessState.RUNNING:
                await self._kill_process(managed)
                managed.state = ProcessState.KILLED

    async def stop(self) -> None:
        """Stop the monitor loop."""
        self._running = False
        if self._monitor_task is not None:
            self._monitor_task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._monitor_task
            self._monitor_task = None
        logger.info("Process monitor stopped")

    def get_process(self, pid: int) -> ManagedProcess | None:
        return self._processes.get(pid)

    def all_processes(self) -> list[ManagedProcess]:
        return list(self._processes.values())

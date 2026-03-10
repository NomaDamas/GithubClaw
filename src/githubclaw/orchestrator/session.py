"""Orchestrator session management.

Each repository gets its own OrchestratorSession running as a child process,
communicating with the webhook server via a Unix domain socket. The session
wraps the Agent SDK abstraction layer, manages lifecycle (start, idle timeout,
persist, resume), and dynamically re-reads global-prompt.md on every event.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import os
import signal
import time
from pathlib import Path

from githubclaw.constants import (
    CONNECTION_TIMEOUT_SECONDS,
    DEFAULT_IDLE_TIMEOUT_SECONDS,
    DEFAULT_ORCHESTRATOR_EVENT_TIMEOUT_SECONDS,
    IDLE_CHECK_INTERVAL_SECONDS,
    SOCKET_LENGTH_PREFIX_BYTES,
    SOCKET_MESSAGE_MAX_BYTES,
)
from githubclaw.orchestrator.schema import ActionList, NoAction
from githubclaw.orchestrator.sdk_abstraction import (
    AgentSDK,
    ClaudeAgentSDK,
    SessionConfig,
)
from githubclaw.orchestrator.tools import build_tools

logger = logging.getLogger(__name__)


class OrchestratorSession:
    """Per-repo orchestrator session managing an Agent SDK conversation.

    Attributes:
        repo: Repository in 'owner/repo' format.
        repo_name: Short name derived from repo (e.g. 'owner-repo').
        repo_dir: Absolute path to the local repository clone.
        socket_path: Unix socket path for IPC with the webhook server.
    """

    def __init__(
        self,
        repo: str,
        repo_dir: str,
        *,
        sdk: AgentSDK | None = None,
        system_prompt_path: str | None = None,
        global_prompt_path: str | None = None,
        persistence_dir: str | None = None,
        idle_timeout: int = DEFAULT_IDLE_TIMEOUT_SECONDS,
        extra_allowed_paths: list[str] | None = None,
        model: str = "claude-sonnet-4-20250514",
    ) -> None:
        self.repo = repo
        self.repo_name = repo.replace("/", "-")
        self.repo_dir = repo_dir
        self.socket_path = f"/tmp/githubclaw-{self.repo_name}.sock"

        self._sdk: AgentSDK = sdk or ClaudeAgentSDK()
        self._session_id: str | None = None
        self._model = model
        self._idle_timeout = idle_timeout
        self._last_activity: float = time.monotonic()
        self._idle_task: asyncio.Task[None] | None = None
        self._server: asyncio.Server | None = None
        self._running = False
        self._processing = False

        # Paths
        githubclaw_dir = os.path.join(repo_dir, ".githubclaw")
        self._system_prompt_path = system_prompt_path or os.path.join(
            githubclaw_dir, "orchestrator.md"
        )
        self._global_prompt_path = global_prompt_path or os.path.join(
            githubclaw_dir, "global-prompt.md"
        )
        self._persistence_dir = Path(
            persistence_dir
            or os.path.join(os.path.expanduser("~/.githubclaw"), "sessions", self.repo_name)
        )
        self._extra_allowed_paths = extra_allowed_paths or []

        # Build tools once (closures capture repo/repo_dir)
        self._tools = build_tools(
            repo=repo,
            repo_dir=repo_dir,
            githubclaw_dir=githubclaw_dir,
            extra_allowed_paths=self._extra_allowed_paths,
        )

    def _load_system_prompt(self) -> str:
        """Load the system prompt from the orchestrator.md file."""
        path = Path(self._system_prompt_path)
        if path.is_file():
            return path.read_text(encoding="utf-8", errors="replace")
        logger.warning("System prompt not found at %s, using default.", path)
        return (
            "You are the orchestrator for a GitHub repository managed by GithubClaw. "
            "Classify incoming events and produce structured action output. "
            "Use your tools to gather context before making decisions."
        )

    def _load_global_prompt(self) -> str:
        """Load the global prompt (agent roster, conventions) for dynamic injection."""
        path = Path(self._global_prompt_path)
        if path.is_file():
            return path.read_text(encoding="utf-8", errors="replace")
        logger.debug("Global prompt not found at %s, skipping.", path)
        return ""

    async def _ensure_session(self) -> str:
        """Ensure an Agent SDK session exists, creating or resuming as needed."""
        if self._session_id is not None:
            return self._session_id

        config = SessionConfig(
            model=self._model,
            system_prompt=self._load_system_prompt(),
            tools=self._tools,
            repo_name=self.repo_name,
            persistence_dir=self._persistence_dir,
        )

        # Try to resume from persisted state
        state_file = self._persistence_dir / "session_state.json"
        if state_file.exists():
            try:
                self._session_id = await self._sdk.resume_session(self._persistence_dir, config)
                logger.info("Resumed session for %s: %s", self.repo, self._session_id)
                return self._session_id
            except Exception:
                logger.warning(
                    "Failed to resume session for %s, creating new.",
                    self.repo,
                    exc_info=True,
                )

        # Create fresh session
        self._session_id = await self._sdk.create_session(config)
        logger.info("Created new session for %s: %s", self.repo, self._session_id)
        return self._session_id

    async def process_event(self, event_json: str) -> str:
        """Process a single event and return structured action output as JSON.

        This is the core method called for each incoming webhook event:
        1. Re-read global-prompt.md for dynamic context
        2. Send the event to the Agent SDK session
        3. Parse the response as an ActionList
        4. Return validated JSON

        Args:
            event_json: The raw event payload as a JSON string.

        Returns:
            JSON string conforming to the ActionList schema.
        """
        self._last_activity = time.monotonic()
        self._processing = True
        try:
            session_id = await self._ensure_session()

            # Dynamic re-read of global-prompt.md on every event
            global_prompt = self._load_global_prompt()

            # Instruct the model to produce structured output
            message = (
                f"Process the following GitHub event and respond with a JSON object "
                f"conforming to the ActionList schema (actions + reasoning).\n\n"
                f"Event:\n```json\n{event_json}\n```"
            )

            response = await self._sdk.send_message(
                session_id,
                message,
                dynamic_context=global_prompt,
            )

            # Validate the response against our schema
            try:
                action_list = ActionList.model_validate_json(response.final_text)
                result = action_list.model_dump_json(indent=2)
            except Exception:
                # If the model output isn't valid JSON/schema, try to extract JSON
                result = self._extract_and_validate(response.final_text)

            self._last_activity = time.monotonic()
            return result
        finally:
            self._processing = False

    def _extract_and_validate(self, text: str) -> str:
        """Attempt to extract JSON from model output that may contain markdown fences."""
        import re

        # Try to find JSON in code fences
        json_match = re.search(r"```(?:json)?\s*\n?(.*?)\n?```", text, re.DOTALL)
        candidate = json_match.group(1).strip() if json_match else text.strip()

        try:
            action_list = ActionList.model_validate_json(candidate)
            return action_list.model_dump_json(indent=2)
        except Exception as exc:
            # Last resort: wrap in a no_action with the raw text as reasoning
            logger.error("Failed to parse orchestrator output as ActionList: %s", exc)
            fallback = ActionList(
                actions=[
                    NoAction(
                        reasoning=f"Failed to parse model output: {text[:500]}",
                    )
                ],
                reasoning="Orchestrator produced unparseable output; defaulting to no_action.",
            )
            return fallback.model_dump_json(indent=2)

    async def persist(self) -> None:
        """Persist the current session state to disk."""
        if self._session_id is not None:
            await self._sdk.persist_session(self._session_id, self._persistence_dir)
            logger.info("Persisted session %s", self._session_id)

    async def shutdown(self) -> None:
        """Persist session, stop the socket server, and clean up."""
        self._running = False

        if self._idle_task is not None:
            self._idle_task.cancel()
            self._idle_task = None

        await self.persist()

        if self._session_id is not None:
            await self._sdk.destroy_session(self._session_id)
            self._session_id = None

        if self._server is not None:
            self._server.close()
            await self._server.wait_closed()
            self._server = None

        # Clean up socket file
        with contextlib.suppress(FileNotFoundError):
            os.unlink(self.socket_path)

        logger.info("Orchestrator for %s shut down.", self.repo)

    async def _handle_connection(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
    ) -> None:
        """Handle a single connection on the Unix socket.

        Protocol:
        - Client sends a length-prefixed JSON message: 8-byte big-endian length + payload.
        - Server processes the event and responds with the same framing.
        """
        try:
            # Read length prefix (big-endian)
            length_bytes = await reader.readexactly(SOCKET_LENGTH_PREFIX_BYTES)
            length = int.from_bytes(length_bytes, byteorder="big")

            if length > SOCKET_MESSAGE_MAX_BYTES:
                logger.error("Message too large: %d bytes", length)
                writer.close()
                return

            # Read payload
            payload = await reader.readexactly(length)
            event_json = payload.decode("utf-8")
            logger.debug("Received event (%d bytes) for %s", length, self.repo)

            # Process event
            result = await self.process_event(event_json)

            # Send response with length prefix
            result_bytes = result.encode("utf-8")
            writer.write(len(result_bytes).to_bytes(SOCKET_LENGTH_PREFIX_BYTES, byteorder="big"))
            writer.write(result_bytes)
            await writer.drain()

        except asyncio.IncompleteReadError:
            logger.debug("Client disconnected before sending complete message.")
        except Exception:
            logger.exception("Error handling orchestrator connection for %s", self.repo)
            # Attempt to send error response
            try:
                error_response = json.dumps(
                    {
                        "actions": [
                            {"type": "no_action", "reasoning": "Internal orchestrator error"}
                        ],
                        "reasoning": "Internal error occurred during event processing.",
                    }
                )
                error_bytes = error_response.encode("utf-8")
                writer.write(len(error_bytes).to_bytes(SOCKET_LENGTH_PREFIX_BYTES, byteorder="big"))
                writer.write(error_bytes)
                await writer.drain()
            except Exception:
                logger.warning(
                    "Failed to send error response to client for %s",
                    self.repo,
                    exc_info=True,
                )
        finally:
            writer.close()
            with contextlib.suppress(Exception):
                await writer.wait_closed()

    async def _idle_watchdog(self) -> None:
        """Background task that shuts down the session after idle timeout."""
        while self._running:
            await asyncio.sleep(IDLE_CHECK_INTERVAL_SECONDS)
            elapsed = time.monotonic() - self._last_activity
            if elapsed >= self._idle_timeout and not self._processing:
                logger.info(
                    "Orchestrator for %s idle for %.0fs, shutting down.",
                    self.repo,
                    elapsed,
                )
                await self.shutdown()
                return

    async def serve(self) -> None:
        """Start the Unix socket server and process events until shutdown.

        This is the main entry point when running the orchestrator as a
        child process. It:
        1. Removes any stale socket file
        2. Starts listening on the Unix socket
        3. Starts the idle watchdog
        4. Runs until shutdown is called or a signal is received
        """
        # Clean up stale socket
        with contextlib.suppress(FileNotFoundError):
            os.unlink(self.socket_path)

        self._running = True
        self._last_activity = time.monotonic()

        self._server = await asyncio.start_unix_server(
            self._handle_connection,
            path=self.socket_path,
        )
        logger.info(
            "Orchestrator for %s listening on %s",
            self.repo,
            self.socket_path,
        )

        # Set up signal handlers for graceful shutdown
        loop = asyncio.get_running_loop()
        for sig in (signal.SIGTERM, signal.SIGINT):
            loop.add_signal_handler(sig, lambda: asyncio.ensure_future(self.shutdown()))

        # Start idle watchdog
        self._idle_task = asyncio.create_task(self._idle_watchdog())

        # Serve until shutdown
        try:
            async with self._server:
                await self._server.serve_forever()
        except asyncio.CancelledError:
            pass
        finally:
            if self._running:
                await self.shutdown()


async def send_event_to_orchestrator(
    repo_name: str,
    event_json: str,
    timeout: float = DEFAULT_ORCHESTRATOR_EVENT_TIMEOUT_SECONDS,
) -> str:
    """Send an event to a running orchestrator via its Unix socket.

    This is the client-side function used by the webhook server to communicate
    with an orchestrator child process.

    Args:
        repo_name: The repo name (used to derive socket path).
        event_json: The event payload as a JSON string.
        timeout: Timeout in seconds for the full round-trip.

    Returns:
        The orchestrator's response as a JSON string (ActionList schema).

    Raises:
        ConnectionError: If the orchestrator socket is not available.
        TimeoutError: If the orchestrator doesn't respond in time.
    """
    socket_path = f"/tmp/githubclaw-{repo_name}.sock"

    try:
        reader, writer = await asyncio.wait_for(
            asyncio.open_unix_connection(socket_path),
            timeout=min(CONNECTION_TIMEOUT_SECONDS, timeout / 3),
        )
    except (FileNotFoundError, ConnectionRefusedError) as exc:
        msg = f"Orchestrator for {repo_name} is not running (socket: {socket_path})"
        raise ConnectionError(msg) from exc
    except TimeoutError as exc:
        msg = f"Timed out connecting to orchestrator for {repo_name}"
        raise TimeoutError(msg) from exc

    try:
        # Send length-prefixed message
        payload = event_json.encode("utf-8")
        writer.write(len(payload).to_bytes(SOCKET_LENGTH_PREFIX_BYTES, byteorder="big"))
        writer.write(payload)
        await writer.drain()

        # Read length-prefixed response
        length_bytes = await asyncio.wait_for(
            reader.readexactly(SOCKET_LENGTH_PREFIX_BYTES), timeout=timeout
        )
        length = int.from_bytes(length_bytes, byteorder="big")
        response_bytes = await asyncio.wait_for(reader.readexactly(length), timeout=timeout)

        return response_bytes.decode("utf-8")

    except asyncio.IncompleteReadError as exc:
        msg = f"Orchestrator for {repo_name} disconnected unexpectedly"
        raise ConnectionError(msg) from exc
    except TimeoutError as exc:
        msg = f"Orchestrator for {repo_name} timed out after {timeout}s"
        raise TimeoutError(msg) from exc
    finally:
        writer.close()
        with contextlib.suppress(Exception):
            await writer.wait_closed()

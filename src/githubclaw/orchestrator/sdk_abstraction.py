"""Agent SDK abstraction layer.

Provides an abstract interface for LLM agent sessions with implementations
for Claude Agent SDK (primary) and a stub for OpenAI Agents SDK (future).
Supports session persistence for resume after idle timeout.
"""

from __future__ import annotations

import json
import logging
from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from pathlib import Path

from githubclaw.constants import MESSAGE_HISTORY_WARNING_THRESHOLD

logger = logging.getLogger(__name__)


@dataclass
class ToolDefinition:
    """Definition of a custom tool available to the agent session."""

    name: str
    description: str
    parameters: dict[str, Any]
    handler: Any  # Callable — typed as Any to avoid complex generic signatures


@dataclass
class SessionConfig:
    """Configuration for creating an agent session."""

    model: str = "claude-sonnet-4-20250514"
    system_prompt: str = ""
    tools: list[ToolDefinition] = field(default_factory=list)
    max_tokens: int = 16384
    temperature: float = 0.0
    repo_name: str = ""
    persistence_dir: Path | None = None


@dataclass
class AgentMessage:
    """A message in the agent conversation."""

    role: str  # "user", "assistant", or "tool_result"
    content: str
    tool_calls: list[dict[str, Any]] = field(default_factory=list)
    tool_results: list[dict[str, Any]] = field(default_factory=list)


@dataclass
class AgentResponse:
    """Response from the agent, including any tool calls and the final text."""

    messages: list[AgentMessage]
    final_text: str
    usage: dict[str, int] = field(default_factory=dict)


class AgentSDK(ABC):
    """Abstract base class for agent SDK implementations.

    Provides a uniform interface for creating sessions, sending messages,
    and persisting/resuming sessions across different LLM providers.
    """

    @abstractmethod
    async def create_session(self, config: SessionConfig) -> str:
        """Create a new agent session.

        Args:
            config: Session configuration including model, system prompt, and tools.

        Returns:
            A session ID string that can be used to reference this session.
        """

    @abstractmethod
    async def send_message(
        self,
        session_id: str,
        message: str,
        *,
        dynamic_context: str = "",
    ) -> AgentResponse:
        """Send a message to an existing session and get a response.

        The SDK implementation is responsible for executing any tool calls
        the model makes, feeding results back, and returning only once the
        model produces a final text response (the agentic loop).

        Args:
            session_id: The session ID returned by create_session.
            message: The user message to send.
            dynamic_context: Additional context injected before the message
                (e.g. re-read global-prompt.md content).

        Returns:
            AgentResponse with the full conversation turn including tool calls.
        """

    @abstractmethod
    async def persist_session(self, session_id: str, path: Path) -> None:
        """Persist session state to disk for later resume.

        Args:
            session_id: The session to persist.
            path: Directory to write session state files to.
        """

    @abstractmethod
    async def resume_session(self, path: Path, config: SessionConfig) -> str:
        """Resume a previously persisted session.

        Args:
            path: Directory containing persisted session state.
            config: Session config (tools may have changed since persistence).

        Returns:
            A new session ID for the resumed session.
        """

    @abstractmethod
    async def destroy_session(self, session_id: str) -> None:
        """Destroy a session and free associated resources.

        Args:
            session_id: The session to destroy.
        """


class ClaudeAgentSDK(AgentSDK):
    """Claude Agent SDK implementation (primary).

    Wraps the Anthropic Claude API with tool-use agentic loop, session
    state management, and persistence to disk.
    """

    def __init__(self) -> None:
        self._sessions: dict[str, _ClaudeSession] = {}
        self._next_id: int = 0

    async def create_session(self, config: SessionConfig) -> str:
        session_id = f"claude-{config.repo_name}-{self._next_id}"
        self._next_id += 1

        session = _ClaudeSession(
            session_id=session_id,
            config=config,
            messages=[],
        )
        self._sessions[session_id] = session
        logger.info("Created Claude session %s (model=%s)", session_id, config.model)
        return session_id

    async def send_message(
        self,
        session_id: str,
        message: str,
        *,
        dynamic_context: str = "",
    ) -> AgentResponse:
        session = self._sessions.get(session_id)
        if session is None:
            msg = f"Session {session_id} not found"
            raise KeyError(msg)

        # Build the user message with optional dynamic context prefix
        user_content = message
        if dynamic_context:
            user_content = f"<dynamic_context>\n{dynamic_context}\n</dynamic_context>\n\n{message}"

        session.messages.append(AgentMessage(role="user", content=user_content))

        # Agentic loop: call the model, execute tools, repeat until final text
        response = await self._run_agentic_loop(session)
        return response

    async def persist_session(self, session_id: str, path: Path) -> None:
        session = self._sessions.get(session_id)
        if session is None:
            msg = f"Session {session_id} not found"
            raise KeyError(msg)

        path.mkdir(parents=True, exist_ok=True)
        state_file = path / "session_state.json"

        state = {
            "session_id": session.session_id,
            "model": session.config.model,
            "system_prompt": session.config.system_prompt,
            "repo_name": session.config.repo_name,
            "tool_names": [tool.name for tool in session.config.tools],
            "messages": [
                {
                    "role": m.role,
                    "content": m.content,
                    "tool_calls": m.tool_calls,
                    "tool_results": m.tool_results,
                }
                for m in session.messages
            ],
        }

        state_file.write_text(json.dumps(state, indent=2, default=str))
        logger.info("Persisted session %s to %s", session_id, state_file)

    async def resume_session(self, path: Path, config: SessionConfig) -> str:
        state_file = path / "session_state.json"
        if not state_file.exists():
            msg = f"No session state found at {state_file}"
            raise FileNotFoundError(msg)

        state = json.loads(state_file.read_text())

        # Check if tool names have changed since persistence
        persisted_tool_names = set(state.get("tool_names", []))
        current_tool_names = {tool.name for tool in config.tools}
        if persisted_tool_names and persisted_tool_names != current_tool_names:
            added = current_tool_names - persisted_tool_names
            removed = persisted_tool_names - current_tool_names
            logger.warning(
                "Tool names changed since session was persisted. Added: %s, Removed: %s",
                sorted(added) if added else "none",
                sorted(removed) if removed else "none",
            )

        session_id = f"claude-{config.repo_name}-{self._next_id}"
        self._next_id += 1

        messages = [
            AgentMessage(
                role=m["role"],
                content=m["content"],
                tool_calls=m.get("tool_calls", []),
                tool_results=m.get("tool_results", []),
            )
            for m in state.get("messages", [])
        ]

        session = _ClaudeSession(
            session_id=session_id,
            config=config,
            messages=messages,
        )
        self._sessions[session_id] = session
        logger.info(
            "Resumed session %s from %s (%d messages)",
            session_id,
            state_file,
            len(messages),
        )
        return session_id

    async def destroy_session(self, session_id: str) -> None:
        session = self._sessions.pop(session_id, None)
        if session is not None:
            logger.info("Destroyed session %s", session_id)

    async def _run_agentic_loop(self, session: _ClaudeSession) -> AgentResponse:
        """Execute the agentic tool-use loop until the model produces final text.

        This method calls the Anthropic API, checks for tool_use blocks,
        executes the corresponding tool handlers, feeds results back, and
        repeats until the model responds with only text.
        """
        try:
            import anthropic
        except ImportError as exc:
            msg = (
                "The 'anthropic' package is required for ClaudeAgentSDK. "
                "Install it with: pip install anthropic"
            )
            raise ImportError(msg) from exc

        client = anthropic.AsyncAnthropic()

        # Build Anthropic tool definitions from our ToolDefinitions
        anthropic_tools = [
            {
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
            }
            for tool in session.config.tools
        ]

        # Map tool names to handlers for execution
        tool_handlers: dict[str, Any] = {tool.name: tool.handler for tool in session.config.tools}

        # Build messages for the API call
        api_messages = _build_api_messages(session.messages)

        turn_messages: list[AgentMessage] = []
        total_usage: dict[str, int] = {"input_tokens": 0, "output_tokens": 0}

        while True:
            api_kwargs: dict[str, Any] = {
                "model": session.config.model,
                "max_tokens": session.config.max_tokens,
                "temperature": session.config.temperature,
                "messages": api_messages,
            }
            if session.config.system_prompt:
                api_kwargs["system"] = session.config.system_prompt
            if anthropic_tools:
                api_kwargs["tools"] = anthropic_tools

            response = await client.messages.create(**api_kwargs)

            # Accumulate usage
            if response.usage:
                total_usage["input_tokens"] += response.usage.input_tokens
                total_usage["output_tokens"] += response.usage.output_tokens

            # Check if the model wants to use tools
            tool_use_blocks = [b for b in response.content if b.type == "tool_use"]
            text_blocks = [b for b in response.content if b.type == "text"]
            final_text = "\n".join(b.text for b in text_blocks)

            if not tool_use_blocks:
                # No tool calls — this is the final response
                assistant_msg = AgentMessage(role="assistant", content=final_text)
                session.messages.append(assistant_msg)
                turn_messages.append(assistant_msg)
                break

            # Record the assistant message with tool calls
            tool_calls = [
                {
                    "id": b.id,
                    "name": b.name,
                    "input": b.input,
                }
                for b in tool_use_blocks
            ]
            assistant_msg = AgentMessage(
                role="assistant",
                content=final_text,
                tool_calls=tool_calls,
            )
            session.messages.append(assistant_msg)
            turn_messages.append(assistant_msg)

            # Add the full assistant content block to api_messages
            api_messages.append({"role": "assistant", "content": response.content})

            # Execute each tool and collect results
            tool_result_blocks: list[dict[str, Any]] = []
            tool_results_for_msg: list[dict[str, Any]] = []

            for block in tool_use_blocks:
                handler = tool_handlers.get(block.name)
                if handler is None:
                    result_text = f"Error: Unknown tool '{block.name}'"
                    is_error = True
                else:
                    try:
                        import asyncio

                        if asyncio.iscoroutinefunction(handler):
                            result = await handler(**block.input)
                        else:
                            result = handler(**block.input)
                        result_text = str(result) if not isinstance(result, str) else result
                        is_error = False
                    except Exception as exc:
                        result_text = f"Error executing {block.name}: {exc}"
                        is_error = True
                        logger.warning("Tool %s failed: %s", block.name, exc)

                tool_result_blocks.append(
                    {
                        "type": "tool_result",
                        "tool_use_id": block.id,
                        "content": result_text,
                        **({"is_error": True} if is_error else {}),
                    }
                )
                tool_results_for_msg.append(
                    {
                        "tool_use_id": block.id,
                        "name": block.name,
                        "result": result_text,
                        "is_error": is_error,
                    }
                )

            # Record tool results
            tool_msg = AgentMessage(
                role="tool_result",
                content="",
                tool_results=tool_results_for_msg,
            )
            session.messages.append(tool_msg)
            turn_messages.append(tool_msg)

            # Add tool results to API messages for next iteration
            api_messages.append({"role": "user", "content": tool_result_blocks})

        return AgentResponse(
            messages=turn_messages,
            final_text=final_text,
            usage=total_usage,
        )


@dataclass
class _ClaudeSession:
    """Internal session state for a Claude Agent SDK session."""

    session_id: str
    config: SessionConfig
    messages: list[AgentMessage]


def _build_api_messages(messages: list[AgentMessage]) -> list[dict[str, Any]]:
    """Convert internal message history to Anthropic API message format."""
    if len(messages) > MESSAGE_HISTORY_WARNING_THRESHOLD:
        logger.warning(
            "Message history has %d messages (>%d). Consider compacting the "
            "session to reduce context size and improve performance.",
            len(messages),
            MESSAGE_HISTORY_WARNING_THRESHOLD,
        )

    api_messages: list[dict[str, Any]] = []

    for msg in messages:
        if msg.role == "user":
            api_messages.append({"role": "user", "content": msg.content})
        elif msg.role == "assistant":
            if msg.tool_calls:
                # Reconstruct content blocks: text + tool_use
                content_blocks: list[dict[str, Any]] = []
                if msg.content:
                    content_blocks.append({"type": "text", "text": msg.content})
                for tc in msg.tool_calls:
                    content_blocks.append(
                        {
                            "type": "tool_use",
                            "id": tc["id"],
                            "name": tc["name"],
                            "input": tc["input"],
                        }
                    )
                api_messages.append({"role": "assistant", "content": content_blocks})
            else:
                api_messages.append({"role": "assistant", "content": msg.content})
        elif msg.role == "tool_result":
            tool_result_blocks = [
                {
                    "type": "tool_result",
                    "tool_use_id": tr["tool_use_id"],
                    "content": tr["result"],
                    **({"is_error": True} if tr.get("is_error") else {}),
                }
                for tr in msg.tool_results
            ]
            api_messages.append({"role": "user", "content": tool_result_blocks})

    return api_messages


class OpenAIAgentSDK(AgentSDK):
    """OpenAI Agents SDK implementation (future).

    Placeholder stub — all methods raise NotImplementedError.
    """

    async def create_session(self, config: SessionConfig) -> str:
        raise NotImplementedError("OpenAI Agent SDK support is not yet implemented.")

    async def send_message(
        self,
        session_id: str,
        message: str,
        *,
        dynamic_context: str = "",
    ) -> AgentResponse:
        raise NotImplementedError("OpenAI Agent SDK support is not yet implemented.")

    async def persist_session(self, session_id: str, path: Path) -> None:
        raise NotImplementedError("OpenAI Agent SDK support is not yet implemented.")

    async def resume_session(self, path: Path, config: SessionConfig) -> str:
        raise NotImplementedError("OpenAI Agent SDK support is not yet implemented.")

    async def destroy_session(self, session_id: str) -> None:
        raise NotImplementedError("OpenAI Agent SDK support is not yet implemented.")

"""Structured output schema for orchestrator responses.

Every orchestrator response conforms to an ActionList containing a heterogeneous
list of actions. The special NoAction type is mutually exclusive with all others.
"""

from __future__ import annotations

from enum import StrEnum
from typing import TYPE_CHECKING, Annotated, Any, Literal

from pydantic import BaseModel, Field, model_validator

if TYPE_CHECKING:
    from datetime import datetime


class ActionType(StrEnum):
    """Discriminator enum for action types."""

    NO_ACTION = "no_action"
    DISPATCH = "dispatch"
    SCHEDULE_EVENT = "schedule_event"
    CANCEL_EVENT = "cancel_event"


class NoAction(BaseModel):
    """Explicit decision to take no action, with reasoning."""

    type: Literal[ActionType.NO_ACTION] = ActionType.NO_ACTION
    reasoning: str = Field(..., description="Explanation for why no action is needed.")


class DispatchAction(BaseModel):
    """Dispatch a worker agent to handle a task."""

    type: Literal[ActionType.DISPATCH] = ActionType.DISPATCH
    agent_type: str = Field(
        ...,
        description=(
            "Worker agent type to dispatch (e.g. 'coder', 'cs', 'qa', "
            "'bugtracker', 'reviewer', 'pm', 'librarian', 'marketer', "
            "'visionary', 'security')."
        ),
    )
    issue_ref: str = Field(
        ...,
        description="GitHub issue or PR reference (e.g. '#42').",
    )
    task_context: str = Field(
        ...,
        description="Brief context pointer for the agent — not the full details.",
    )


class ScheduleEventAction(BaseModel):
    """Schedule a future synthetic event to be delivered to the orchestrator."""

    type: Literal[ActionType.SCHEDULE_EVENT] = ActionType.SCHEDULE_EVENT
    event_id: str = Field(
        ...,
        description="Unique identifier for this scheduled event (for cancellation).",
    )
    trigger_at: datetime = Field(
        ...,
        description="ISO-8601 timestamp for when the event should fire.",
    )
    repo: str = Field(
        ...,
        description="Repository in 'owner/repo' format.",
    )
    payload: dict[str, Any] = Field(
        default_factory=dict,
        description="Arbitrary payload delivered as the synthetic event.",
    )
    context: str = Field(
        default="",
        description="Human-readable description of why this event was scheduled.",
    )


class CancelEventAction(BaseModel):
    """Cancel a previously scheduled synthetic event."""

    type: Literal[ActionType.CANCEL_EVENT] = ActionType.CANCEL_EVENT
    event_id: str = Field(
        ...,
        description="The event_id of the scheduled event to cancel.",
    )


# Discriminated union of all action types
Action = Annotated[
    NoAction | DispatchAction | ScheduleEventAction | CancelEventAction,
    Field(discriminator="type"),
]


class ActionList(BaseModel):
    """Top-level orchestrator response: a list of actions plus reasoning.

    Validation rules:
    - If a NoAction is present, it must be the only action (singleton).
    - An empty actions list is not allowed.
    """

    actions: list[Action] = Field(
        ...,
        min_length=1,
        description="One or more actions for the webhook server to execute.",
    )
    reasoning: str = Field(
        default="",
        description="Overall reasoning for the set of actions taken.",
    )

    @model_validator(mode="after")
    def validate_no_action_is_singleton(self) -> ActionList:
        """Ensure no_action is mutually exclusive with other action types."""
        has_no_action = any(isinstance(a, NoAction) for a in self.actions)
        if has_no_action and len(self.actions) > 1:
            msg = "no_action is mutually exclusive — it cannot appear alongside other actions."
            raise ValueError(msg)
        return self

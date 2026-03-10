"""GithubClaw error hierarchy.

All custom exceptions inherit from GithubClawError so callers can catch a
single base class when they want a broad safety net.
"""


class GithubClawError(Exception):
    """Base exception for all GithubClaw errors."""


class ConfigError(GithubClawError):
    """Raised for invalid or missing configuration."""


class AgentSpawnError(GithubClawError):
    """Raised when an agent subprocess cannot be started."""


class WebhookError(GithubClawError):
    """Raised for webhook delivery or verification problems."""


class OrchestratorError(GithubClawError):
    """Raised for orchestrator session failures."""


class QueueError(GithubClawError):
    """Raised for disk-persisted queue errors."""

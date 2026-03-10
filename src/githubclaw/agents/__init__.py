"""GithubClaw agent definition system — parsing, prompt assembly, and spawning."""

from githubclaw.agents.parser import AgentDefinition, ToolPermissions, parse_agent_file
from githubclaw.agents.prompt_assembler import PromptAssembler
from githubclaw.agents.spawner import AgentSpawner

__all__ = [
    "AgentDefinition",
    "AgentSpawner",
    "PromptAssembler",
    "ToolPermissions",
    "parse_agent_file",
]

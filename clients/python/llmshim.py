"""
llmshim Python client — high-level wrapper over the generated OpenAPI client.

Usage:
    from llmshim import LlmShim

    client = LlmShim()
    resp = client.chat("claude-sonnet-4-6", "Hello!")
    print(resp.message.content)

    for event in client.stream("claude-sonnet-4-6", "Write a poem"):
        if event["type"] == "content":
            print(event["text"], end="")
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Generator, Optional

import httpx

from llmshim_api_client.client import Client
from llmshim_api_client.api.default import chat as api_chat, list_models as api_list_models, health_check as api_health
from llmshim_api_client.models import (
    ChatRequest,
    ChatResponse,
    Config,
    ConfigReasoningEffort,
    ErrorResponse,
    Message,
    MessageRole,
    ChatRequestProviderConfig,
)
from llmshim_api_client.types import UNSET


def _make_message(role: str, content: str | None = None, **kwargs) -> Message:
    """Create a Message with proper enum role."""
    return Message(role=MessageRole(role), content=content, **kwargs)


class LlmShim:
    """High-level llmshim client."""

    def __init__(self, base_url: str = "http://localhost:3000", timeout: float = 120.0):
        self._base_url = base_url
        self._timeout = timeout
        self._client = Client(
            base_url=base_url,
            timeout=httpx.Timeout(timeout),
            raise_on_unexpected_status=True,
        )

    def chat(
        self,
        model: str,
        messages: str | list[dict[str, Any]],
        *,
        max_tokens: int | None = None,
        temperature: float | None = None,
        reasoning_effort: str | None = None,
        provider_config: dict[str, Any] | None = None,
    ) -> ChatResponse:
        """Send a chat completion request.

        Args:
            model: Model identifier (e.g., "anthropic/claude-sonnet-4-6" or just "claude-sonnet-4-6")
            messages: A string (single user message) or list of message dicts
            max_tokens: Maximum output tokens
            temperature: Sampling temperature
            reasoning_effort: "low", "medium", or "high"
            provider_config: Raw provider-specific JSON
        """
        if isinstance(messages, str):
            msg_list = [_make_message("user", messages)]
        else:
            msg_list = [_make_message(**m) for m in messages]

        config = Config()
        if max_tokens is not None:
            config.max_tokens = max_tokens
        if temperature is not None:
            config.temperature = temperature
        if reasoning_effort is not None:
            config.reasoning_effort = ConfigReasoningEffort(reasoning_effort)

        pc = UNSET
        if provider_config is not None:
            pc = ChatRequestProviderConfig.from_dict(provider_config)

        req = ChatRequest(
            model=model,
            messages=msg_list,
            config=config,
            provider_config=pc,
        )

        resp = api_chat.sync(client=self._client, body=req)
        if resp is None:
            raise RuntimeError(f"Chat request failed (model={model})")
        if isinstance(resp, ErrorResponse):
            raise RuntimeError(f"{resp.error.code}: {resp.error.message}")
        return resp

    def stream(
        self,
        model: str,
        messages: str | list[dict[str, Any]],
        *,
        max_tokens: int | None = None,
        temperature: float | None = None,
        reasoning_effort: str | None = None,
        provider_config: dict[str, Any] | None = None,
    ) -> Generator[dict[str, Any], None, None]:
        """Stream a chat completion. Yields typed event dicts.

        Event types: content, reasoning, tool_call, usage, done, error
        """
        if isinstance(messages, str):
            msgs = [{"role": "user", "content": messages}]
        else:
            msgs = messages

        body = {"model": model, "messages": msgs}
        config_dict = {}
        if max_tokens is not None:
            config_dict["max_tokens"] = max_tokens
        if temperature is not None:
            config_dict["temperature"] = temperature
        if reasoning_effort is not None:
            config_dict["reasoning_effort"] = reasoning_effort
        if config_dict:
            body["config"] = config_dict
        if provider_config is not None:
            body["provider_config"] = provider_config

        with httpx.stream(
            "POST",
            f"{self._base_url}/v1/chat/stream",
            json=body,
            timeout=self._timeout,
        ) as resp:
            resp.raise_for_status()
            current_event = ""
            for line in resp.iter_lines():
                if line.startswith("event: "):
                    current_event = line[7:]
                elif line.startswith("data: "):
                    data = json.loads(line[6:])
                    data["type"] = current_event or data.get("type", "")
                    yield data

    def models(self) -> list[dict[str, str]]:
        """List available models."""
        resp = api_list_models.sync(client=self._client)
        return [{"id": m.id, "provider": m.provider, "name": m.name} for m in resp.models]

    def health(self) -> dict[str, Any]:
        """Health check."""
        resp = api_health.sync(client=self._client)
        return {"status": resp.status, "providers": resp.providers}

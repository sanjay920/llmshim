"""
llmshim Python client.

Auto-starts the proxy server on first use. No separate server process needed.
"""

from __future__ import annotations

import json
from typing import Any, Generator, Optional

import httpx

from llmshim._server import ensure_server


class Shim:
    """Multi-provider LLM client.

    Automatically starts the llmshim proxy server on first use.
    The server stops when the Python process exits.

    Usage:
        client = Shim()
        resp = client.chat("claude-sonnet-4-6", "Hello!")
        print(resp["message"]["content"])
    """

    def __init__(self, base_url: Optional[str] = None, timeout: float = 120.0):
        """Initialize the client.

        Args:
            base_url: Override the proxy URL. If None, auto-starts a local server.
            timeout: Request timeout in seconds.
        """
        self._base_url = base_url
        self._timeout = timeout
        self._http = None

    def _get_base_url(self) -> str:
        if self._base_url is None:
            self._base_url = ensure_server()
        return self._base_url

    def _get_http(self) -> httpx.Client:
        if self._http is None:
            self._http = httpx.Client(timeout=self._timeout)
        return self._http

    def chat(
        self,
        model: str,
        messages: str | list[dict[str, Any]],
        *,
        max_tokens: Optional[int] = None,
        temperature: Optional[float] = None,
        reasoning_effort: Optional[str] = None,
        provider_config: Optional[dict[str, Any]] = None,
        fallback: Optional[list[str]] = None,
    ) -> dict[str, Any]:
        """Send a chat completion request.

        Args:
            model: Model ID (e.g., "anthropic/claude-sonnet-4-6" or "claude-sonnet-4-6")
            messages: A string (single user message) or list of message dicts
            max_tokens: Maximum output tokens
            temperature: Sampling temperature
            reasoning_effort: "low", "medium", or "high"
            provider_config: Raw provider-specific JSON
            fallback: Ordered list of fallback model IDs

        Returns:
            Response dict with keys: id, model, provider, message, usage, latency_ms
        """
        if isinstance(messages, str):
            msgs = [{"role": "user", "content": messages}]
        else:
            msgs = messages

        body: dict[str, Any] = {"model": model, "messages": msgs}

        config: dict[str, Any] = {}
        if max_tokens is not None:
            config["max_tokens"] = max_tokens
        if temperature is not None:
            config["temperature"] = temperature
        if reasoning_effort is not None:
            config["reasoning_effort"] = reasoning_effort
        if config:
            body["config"] = config

        if provider_config is not None:
            body["provider_config"] = provider_config
        if fallback is not None:
            body["fallback"] = fallback

        resp = self._get_http().post(
            f"{self._get_base_url()}/v1/chat",
            json=body,
        )
        resp.raise_for_status()
        return resp.json()

    def stream(
        self,
        model: str,
        messages: str | list[dict[str, Any]],
        *,
        max_tokens: Optional[int] = None,
        temperature: Optional[float] = None,
        reasoning_effort: Optional[str] = None,
        provider_config: Optional[dict[str, Any]] = None,
    ) -> Generator[dict[str, Any], None, None]:
        """Stream a chat completion. Yields typed event dicts.

        Event types: content, reasoning, tool_call, usage, done, error

        Usage:
            for event in client.stream("claude-sonnet-4-6", "Write a poem"):
                if event["type"] == "content":
                    print(event["text"], end="")
        """
        if isinstance(messages, str):
            msgs = [{"role": "user", "content": messages}]
        else:
            msgs = messages

        body: dict[str, Any] = {"model": model, "messages": msgs}

        config: dict[str, Any] = {}
        if max_tokens is not None:
            config["max_tokens"] = max_tokens
        if temperature is not None:
            config["temperature"] = temperature
        if reasoning_effort is not None:
            config["reasoning_effort"] = reasoning_effort
        if config:
            body["config"] = config

        if provider_config is not None:
            body["provider_config"] = provider_config

        with httpx.stream(
            "POST",
            f"{self._get_base_url()}/v1/chat/stream",
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
        resp = self._get_http().get(f"{self._get_base_url()}/v1/models")
        resp.raise_for_status()
        return resp.json()["models"]

    def health(self) -> dict[str, Any]:
        """Health check."""
        resp = self._get_http().get(f"{self._get_base_url()}/health")
        resp.raise_for_status()
        return resp.json()

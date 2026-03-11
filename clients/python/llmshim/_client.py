"""
llmshim module-level API.

import llmshim
resp = llmshim.chat("claude-sonnet-4-6", "Hello!")
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Generator, Optional

import httpx

from llmshim._server import ensure_server

_base_url: Optional[str] = None
_http: Optional[httpx.Client] = None
_timeout: float = 120.0


def _get_base_url() -> str:
    global _base_url
    if _base_url is None:
        _base_url = ensure_server()
    return _base_url


def _get_http() -> httpx.Client:
    global _http
    if _http is None:
        _http = httpx.Client(timeout=_timeout)
    return _http


def configure(
    *,
    openai: Optional[str] = None,
    anthropic: Optional[str] = None,
    gemini: Optional[str] = None,
    xai: Optional[str] = None,
) -> None:
    """Configure API keys. Writes to ~/.llmshim/config.toml.

    Keys are persistent — configure once, use everywhere.
    Only provided keys are updated; others are left unchanged.

    Usage:
        import llmshim
        llmshim.configure(anthropic="sk-ant-...", openai="sk-...")
    """
    config_dir = Path.home() / ".llmshim"
    config_path = config_dir / "config.toml"

    # Read existing config
    existing: dict[str, Any] = {}
    if config_path.exists():
        try:
            import tomllib  # Python 3.11+
        except ImportError:
            tomllib = None  # type: ignore
        if tomllib:
            try:
                with open(config_path, "rb") as f:
                    existing = tomllib.load(f)
            except Exception:
                pass

    keys = existing.get("keys", {})
    if openai is not None:
        keys["openai"] = openai
    if anthropic is not None:
        keys["anthropic"] = anthropic
    if gemini is not None:
        keys["gemini"] = gemini
    if xai is not None:
        keys["xai"] = xai

    # Write back
    config_dir.mkdir(parents=True, exist_ok=True)
    lines = ["[keys]"]
    for key, val in keys.items():
        if val:
            lines.append(f'{key} = "{val}"')
    if "proxy" in existing:
        lines.append("")
        lines.append("[proxy]")
        proxy = existing["proxy"]
        if "host" in proxy:
            lines.append(f'host = "{proxy["host"]}"')
        if "port" in proxy:
            lines.append(f"port = {proxy['port']}")

    config_path.write_text("\n".join(lines) + "\n")

    # Also set as env vars for the current process (so server picks them up)
    if openai:
        os.environ["OPENAI_API_KEY"] = openai
    if anthropic:
        os.environ["ANTHROPIC_API_KEY"] = anthropic
    if gemini:
        os.environ["GEMINI_API_KEY"] = gemini
    if xai:
        os.environ["XAI_API_KEY"] = xai

    # If server is already running, it won't pick up new keys until restart.
    # Force restart on next call.
    global _base_url
    from llmshim._server import _stop_server

    _stop_server()
    _base_url = None


def chat(
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

    Usage:
        resp = llmshim.chat("claude-sonnet-4-6", "What is Rust?")
        print(resp["message"]["content"])
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

    resp = _get_http().post(f"{_get_base_url()}/v1/chat", json=body)
    resp.raise_for_status()
    return resp.json()


def stream(
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
        for event in llmshim.stream("claude-sonnet-4-6", "Write a poem"):
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
        f"{_get_base_url()}/v1/chat/stream",
        json=body,
        timeout=_timeout,
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


def models() -> list[dict[str, str]]:
    """List available models.

    Returns:
        List of dicts with keys: id, provider, name
    """
    resp = _get_http().get(f"{_get_base_url()}/v1/models")
    resp.raise_for_status()
    return resp.json()["models"]


def health() -> dict[str, Any]:
    """Health check.

    Returns:
        Dict with keys: status, providers
    """
    resp = _get_http().get(f"{_get_base_url()}/health")
    resp.raise_for_status()
    return resp.json()

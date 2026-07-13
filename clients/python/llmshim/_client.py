"""
llmshim module-level API.

import llmshim
resp = llmshim.chat("claude-sonnet-4-6", "Hello!")
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Generator, List, Optional, Union

import httpx

from llmshim._server import ensure_server
from llmshim.types import (
    ChatResponse,
    HealthResponse,
    Message,
    ModelEntry,
    ReasoningEffort,
    ReasoningMode,
    StreamEvent,
)

_base_url: Optional[str] = None
_http: Optional[httpx.Client] = None
_timeout: float = 120.0


class LlmShimError(Exception):
    """Raised when the proxy returns an error response.

    Mirrors the spec's ``ErrorResponse`` shape. ``code`` and ``message`` come
    from the ``error`` object when the body is a structured error; otherwise
    they fall back to the raw response text.

    Attributes:
        code: Machine-readable error code (e.g., "bad_request").
        message: Human-readable error message.
        status_code: HTTP status code of the response.
    """

    def __init__(self, message: str, *, code: str = "error", status_code: int = 0):
        self.code = code
        self.message = message
        self.status_code = status_code
        super().__init__(
            f"[{status_code} {code}] {message}" if status_code else message
        )


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


def _raise_for_error(resp: httpx.Response) -> None:
    """Raise LlmShimError if the response is an error, parsing ErrorResponse."""
    if resp.status_code < 400:
        return
    code = "error"
    message = resp.text
    try:
        body = resp.json()
        err = body.get("error") if isinstance(body, dict) else None
        if isinstance(err, dict):
            code = err.get("code", code)
            message = err.get("message", message)
    except (ValueError, json.JSONDecodeError):
        pass
    raise LlmShimError(message, code=code, status_code=resp.status_code)


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


def _build_body(
    model: str,
    messages: Union[str, List[Message]],
    *,
    max_tokens: Optional[int],
    temperature: Optional[float],
    top_p: Optional[float],
    top_k: Optional[int],
    stop: Optional[List[str]],
    reasoning_effort: Optional[ReasoningEffort],
    reasoning_mode: Optional[ReasoningMode],
    tools: Optional[List[dict]],
    tool_choice: Optional[Any],
    provider_config: Optional[dict],
    fallback: Optional[List[str]],
    stream: bool = False,
) -> dict[str, Any]:
    """Build a spec-faithful ChatRequest body."""
    if isinstance(messages, str):
        msgs: List[Any] = [{"role": "user", "content": messages}]
    else:
        msgs = messages

    body: dict[str, Any] = {"model": model, "messages": msgs}
    if stream:
        body["stream"] = True

    # config — provider-agnostic settings
    config: dict[str, Any] = {}
    if max_tokens is not None:
        config["max_tokens"] = max_tokens
    if temperature is not None:
        config["temperature"] = temperature
    if top_p is not None:
        config["top_p"] = top_p
    if top_k is not None:
        config["top_k"] = top_k
    if stop is not None:
        config["stop"] = stop
    if reasoning_effort is not None:
        config["reasoning_effort"] = reasoning_effort
    if reasoning_mode is not None:
        config["reasoning_mode"] = reasoning_mode
    if config:
        body["config"] = config

    # Tools go in provider_config (passed through to the provider).
    pc = dict(provider_config) if provider_config else {}
    if tools is not None:
        pc["tools"] = tools
    if tool_choice is not None:
        pc["tool_choice"] = tool_choice
    if pc:
        body["provider_config"] = pc

    if fallback is not None:
        body["fallback"] = fallback

    return body


def chat(
    model: str,
    messages: Union[str, List[Message]],
    *,
    max_tokens: Optional[int] = None,
    temperature: Optional[float] = None,
    top_p: Optional[float] = None,
    top_k: Optional[int] = None,
    stop: Optional[List[str]] = None,
    reasoning_effort: Optional[ReasoningEffort] = None,
    reasoning_mode: Optional[ReasoningMode] = None,
    tools: Optional[List[dict]] = None,
    tool_choice: Optional[Any] = None,
    provider_config: Optional[dict] = None,
    fallback: Optional[List[str]] = None,
) -> ChatResponse:
    """Send a chat completion request.

    Args:
        model: Model ID (e.g., "anthropic/claude-sonnet-4-6" or "claude-sonnet-4-6")
        messages: A string (single user message) or list of message dicts
        max_tokens: Maximum output tokens
        temperature: Sampling temperature (0–2)
        top_p: Nucleus sampling probability
        top_k: Top-k sampling
        stop: Stop sequences
        reasoning_effort: "none", "low", "medium", "high", "xhigh", or "max"
            (mapped per provider/model with clamping; see docs/reasoning.md)
        reasoning_mode: "standard" (default) or "pro" — "pro" requests
            substantially more model work
        tools: Tool definitions (Chat Completions format — auto-translated per provider)
        tool_choice: Tool selection ("auto", "required", "none", or specific tool)
        provider_config: Raw provider-specific JSON
        fallback: Ordered list of fallback model IDs

    Returns:
        A ChatResponse dict with keys: id, model, provider, message, usage,
        latency_ms, and optionally reasoning.

    Raises:
        LlmShimError: If the proxy returns an error (e.g., 400, 502).

    Usage:
        resp = llmshim.chat("claude-sonnet-4-6", "What is Rust?")
        print(resp["message"]["content"])
    """
    body = _build_body(
        model,
        messages,
        max_tokens=max_tokens,
        temperature=temperature,
        top_p=top_p,
        top_k=top_k,
        stop=stop,
        reasoning_effort=reasoning_effort,
        reasoning_mode=reasoning_mode,
        tools=tools,
        tool_choice=tool_choice,
        provider_config=provider_config,
        fallback=fallback,
    )

    resp = _get_http().post(f"{_get_base_url()}/v1/chat", json=body)
    _raise_for_error(resp)
    return resp.json()


def stream(
    model: str,
    messages: Union[str, List[Message]],
    *,
    max_tokens: Optional[int] = None,
    temperature: Optional[float] = None,
    top_p: Optional[float] = None,
    top_k: Optional[int] = None,
    stop: Optional[List[str]] = None,
    reasoning_effort: Optional[ReasoningEffort] = None,
    reasoning_mode: Optional[ReasoningMode] = None,
    tools: Optional[List[dict]] = None,
    tool_choice: Optional[Any] = None,
    provider_config: Optional[dict] = None,
    fallback: Optional[List[str]] = None,
) -> Generator[StreamEvent, None, None]:
    """Stream a chat completion. Yields typed event dicts.

    Event types: content, reasoning, tool_call, usage, done, error.
    Each yielded dict has a ``type`` key matching the SSE ``event:`` field.

    Raises:
        LlmShimError: If the endpoint itself returns an HTTP error before the
            stream begins. Provider errors mid-stream arrive as an ``error``
            event rather than an exception.

    Usage:
        for event in llmshim.stream("claude-sonnet-4-6", "Write a poem"):
            if event["type"] == "content":
                print(event["text"], end="")
    """
    body = _build_body(
        model,
        messages,
        max_tokens=max_tokens,
        temperature=temperature,
        top_p=top_p,
        top_k=top_k,
        stop=stop,
        reasoning_effort=reasoning_effort,
        reasoning_mode=reasoning_mode,
        tools=tools,
        tool_choice=tool_choice,
        provider_config=provider_config,
        fallback=fallback,
    )

    with httpx.stream(
        "POST",
        f"{_get_base_url()}/v1/chat/stream",
        json=body,
        timeout=_timeout,
    ) as resp:
        if resp.status_code >= 400:
            resp.read()
            _raise_for_error(resp)
        current_event = ""
        for line in resp.iter_lines():
            if line.startswith("event: "):
                current_event = line[7:]
            elif line.startswith("data: "):
                data = json.loads(line[6:])
                data["type"] = current_event or data.get("type", "")
                yield data
            elif line == "":
                # Blank line terminates an SSE event.
                current_event = ""


def models() -> List[ModelEntry]:
    """List available models.

    Returns:
        List of ModelEntry dicts with keys: id, provider, name.
    """
    resp = _get_http().get(f"{_get_base_url()}/v1/models")
    _raise_for_error(resp)
    return resp.json()["models"]


def health() -> HealthResponse:
    """Health check.

    Returns:
        A HealthResponse dict with keys: status, providers.
    """
    resp = _get_http().get(f"{_get_base_url()}/health")
    _raise_for_error(resp)
    return resp.json()

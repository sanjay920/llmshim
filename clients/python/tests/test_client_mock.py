"""
Fully mocked unit tests for the llmshim Python client.

These spin up a local ``http.server`` that returns canned JSON and canned SSE.
They verify request building, response parsing, SSE event typing, and error
handling WITHOUT touching any real provider API or needing any API key. The
bundled-binary auto-spawn (``_server.ensure_server``) is bypassed by pointing
the client's ``_base_url`` at the local mock — the auto-spawn behavior itself
is left untouched in the package.

Run:  pytest tests/test_client_mock.py
Cost: $0 — no network calls leave localhost.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest

import llmshim
from llmshim import _client

# Captures the most recent request the mock server received, so tests can
# assert on exactly what the client serialized and sent.
CAPTURED: dict = {}


class _MockHandler(BaseHTTPRequestHandler):
    def log_message(self, *args):  # silence the default stderr logging
        pass

    def _read_json(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length else b""
        CAPTURED["path"] = self.path
        CAPTURED["body"] = json.loads(raw) if raw else None
        return CAPTURED["body"]

    def _send_json(self, status: int, payload: dict):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _send_sse(self, events):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for name, payload in events:
            block = f"event: {name}\ndata: {json.dumps(payload)}\n\n"
            self.wfile.write(block.encode())
        self.wfile.flush()

    def do_GET(self):
        if self.path == "/health":
            self._send_json(200, {"status": "ok", "providers": ["anthropic", "openai"]})
        elif self.path == "/v1/models":
            self._send_json(
                200,
                {
                    "models": [
                        {
                            "id": "anthropic/claude-sonnet-4-6",
                            "provider": "anthropic",
                            "name": "claude-sonnet-4-6",
                        },
                        {
                            "id": "openai/gpt-5.5",
                            "provider": "openai",
                            "name": "gpt-5.5",
                        },
                    ]
                },
            )
        else:
            self._send_json(404, {"error": {"code": "not_found", "message": self.path}})

    def do_POST(self):
        body = self._read_json()
        model = (body or {}).get("model", "")

        if self.path == "/v1/chat":
            if model.startswith("error"):
                self._send_json(
                    400,
                    {"error": {"code": "bad_request", "message": "unknown provider"}},
                )
                return
            self._send_json(
                200,
                {
                    "id": "resp_123",
                    "model": model or "claude-sonnet-4-6",
                    "provider": "anthropic",
                    "message": {"role": "assistant", "content": "pong"},
                    "reasoning": "let me think",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 5,
                        "reasoning_tokens": 3,
                        "total_tokens": 18,
                    },
                    "latency_ms": 42,
                },
            )
        elif self.path == "/v1/chat/stream":
            if model.startswith("error"):
                self._send_json(
                    400,
                    {"error": {"code": "bad_request", "message": "unknown provider"}},
                )
                return
            if model == "streamerr":
                self._send_sse([("error", {"message": "provider blew up"})])
                return
            self._send_sse(
                [
                    ("reasoning", {"text": "hmm"}),
                    ("content", {"text": "Hello"}),
                    ("content", {"text": " world"}),
                    (
                        "tool_call",
                        {"id": "call_1", "name": "get_weather", "arguments": '{"city":"NYC"}'},
                    ),
                    (
                        "usage",
                        {
                            "input_tokens": 3,
                            "output_tokens": 2,
                            "reasoning_tokens": 1,
                            "total_tokens": 6,
                        },
                    ),
                    ("done", {}),
                ]
            )
        else:
            self._send_json(404, {"error": {"code": "not_found", "message": self.path}})


@pytest.fixture(scope="module", autouse=True)
def mock_server():
    server = HTTPServer(("127.0.0.1", 0), _MockHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    host, port = server.server_address
    # Point the client at the mock, bypassing the Rust-binary auto-spawn.
    _client._base_url = f"http://{host}:{port}"
    _client._http = None  # reset any cached client
    try:
        yield
    finally:
        server.shutdown()
        _client._base_url = None
        _client._http = None


@pytest.fixture(autouse=True)
def _clear_capture():
    CAPTURED.clear()
    yield


# --------------------------------------------------------------------------- #
# health / models
# --------------------------------------------------------------------------- #


def test_health():
    h = llmshim.health()
    assert h["status"] == "ok"
    assert h["providers"] == ["anthropic", "openai"]


def test_models_returns_list():
    m = llmshim.models()
    assert isinstance(m, list)
    assert m[0]["id"] == "anthropic/claude-sonnet-4-6"
    assert {"id", "provider", "name"} <= set(m[0].keys())


# --------------------------------------------------------------------------- #
# chat: request building
# --------------------------------------------------------------------------- #


def test_chat_string_message_is_wrapped():
    llmshim.chat("claude-sonnet-4-6", "hi")
    assert CAPTURED["body"]["messages"] == [{"role": "user", "content": "hi"}]


def test_chat_builds_spec_faithful_body():
    llmshim.chat(
        "anthropic/claude-sonnet-4-6",
        [{"role": "user", "content": "hi"}],
        max_tokens=100,
        temperature=0.5,
        top_p=0.9,
        top_k=40,
        stop=["END"],
        reasoning_effort="high",
        tools=[{"type": "function", "function": {"name": "f"}}],
        tool_choice="auto",
        provider_config={"thinking": {"type": "adaptive"}},
        fallback=["openai/gpt-5.5"],
    )
    body = CAPTURED["body"]
    assert body["model"] == "anthropic/claude-sonnet-4-6"
    # config object holds all provider-agnostic settings
    assert body["config"] == {
        "max_tokens": 100,
        "temperature": 0.5,
        "top_p": 0.9,
        "top_k": 40,
        "stop": ["END"],
        "reasoning_effort": "high",
    }
    # tools/tool_choice merged into provider_config alongside passthrough keys
    pc = body["provider_config"]
    assert pc["thinking"] == {"type": "adaptive"}
    assert pc["tools"] == [{"type": "function", "function": {"name": "f"}}]
    assert pc["tool_choice"] == "auto"
    # fallback is top-level
    assert body["fallback"] == ["openai/gpt-5.5"]


def test_chat_omits_empty_config_and_provider_config():
    llmshim.chat("claude-sonnet-4-6", "hi")
    body = CAPTURED["body"]
    assert "config" not in body
    assert "provider_config" not in body
    assert "fallback" not in body


# --------------------------------------------------------------------------- #
# chat: response parsing
# --------------------------------------------------------------------------- #


def test_chat_parses_response():
    resp = llmshim.chat("claude-sonnet-4-6", "hi")
    assert resp["id"] == "resp_123"
    assert resp["provider"] == "anthropic"
    assert resp["message"]["content"] == "pong"
    assert resp["reasoning"] == "let me think"
    assert resp["usage"]["total_tokens"] == 18
    assert resp["latency_ms"] == 42


# --------------------------------------------------------------------------- #
# chat: error handling
# --------------------------------------------------------------------------- #


def test_chat_raises_llmshim_error():
    with pytest.raises(llmshim.LlmShimError) as exc:
        llmshim.chat("error/model", "hi")
    err = exc.value
    assert err.code == "bad_request"
    assert err.message == "unknown provider"
    assert err.status_code == 400


# --------------------------------------------------------------------------- #
# stream
# --------------------------------------------------------------------------- #


def test_stream_yields_typed_events():
    events = list(llmshim.stream("claude-sonnet-4-6", "hi"))
    types = [e["type"] for e in events]
    assert types == ["reasoning", "content", "content", "tool_call", "usage", "done"]

    reasoning = next(e for e in events if e["type"] == "reasoning")
    assert reasoning["text"] == "hmm"

    content = [e["text"] for e in events if e["type"] == "content"]
    assert "".join(content) == "Hello world"

    tc = next(e for e in events if e["type"] == "tool_call")
    assert tc["id"] == "call_1"
    assert tc["name"] == "get_weather"
    assert tc["arguments"] == '{"city":"NYC"}'

    usage = next(e for e in events if e["type"] == "usage")
    assert usage["total_tokens"] == 6


def test_stream_error_event_is_yielded_not_raised():
    events = list(llmshim.stream("streamerr", "hi"))
    assert len(events) == 1
    assert events[0]["type"] == "error"
    assert events[0]["message"] == "provider blew up"


def test_stream_http_error_raises():
    with pytest.raises(llmshim.LlmShimError) as exc:
        list(llmshim.stream("error/model", "hi"))
    assert exc.value.status_code == 400
    assert exc.value.code == "bad_request"


def test_stream_sends_stream_endpoint_body():
    list(llmshim.stream("claude-sonnet-4-6", "hi", max_tokens=50))
    assert CAPTURED["path"] == "/v1/chat/stream"
    assert CAPTURED["body"]["config"] == {"max_tokens": 50}


# --------------------------------------------------------------------------- #
# types module
# --------------------------------------------------------------------------- #


def test_types_are_exported():
    from llmshim import types

    for name in [
        "ChatRequest",
        "ChatResponse",
        "Config",
        "Message",
        "Usage",
        "StreamEvent",
        "ErrorResponse",
        "ModelsResponse",
        "HealthResponse",
    ]:
        assert hasattr(types, name)
    # Re-exported at top level too.
    assert llmshim.ChatResponse is types.ChatResponse

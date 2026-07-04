"""
Spec-faithful type definitions for the llmshim API.

These mirror the schemas in ``api/openapi.yaml`` and are provided so that
callers can type-check their request/response handling. They are plain
``TypedDict`` / ``Literal`` aliases — nothing is validated at runtime.

Optional keys are expressed with the base/total=False inheritance pattern so
the module stays compatible with Python 3.9 (no ``typing.NotRequired``).
"""

from __future__ import annotations

from typing import Any, List, Literal, TypedDict, Union

__all__ = [
    "Role",
    "ReasoningEffort",
    "ToolCallFunction",
    "ToolCall",
    "Message",
    "Config",
    "ChatRequest",
    "Usage",
    "ResponseMessage",
    "ChatResponse",
    "ModelEntry",
    "ModelsResponse",
    "HealthResponse",
    "ErrorDetail",
    "ErrorResponse",
    "ContentEvent",
    "ReasoningEvent",
    "ToolCallEvent",
    "UsageEvent",
    "DoneEvent",
    "ErrorEvent",
    "StreamEvent",
    "StreamEventType",
]

# --- primitives -------------------------------------------------------------

Role = Literal["system", "user", "assistant", "tool", "developer"]
ReasoningEffort = Literal["low", "medium", "high"]
StreamEventType = Literal["content", "reasoning", "tool_call", "usage", "done", "error"]


# --- tool calls -------------------------------------------------------------


class ToolCallFunction(TypedDict, total=False):
    """The ``function`` object inside a tool call."""

    name: str
    arguments: str  # JSON-encoded arguments


class ToolCall(TypedDict, total=False):
    id: str
    type: Literal["function"]
    function: ToolCallFunction


# --- request ----------------------------------------------------------------


class _MessageBase(TypedDict):
    role: Role


class Message(_MessageBase, total=False):
    """A conversation message (request side)."""

    # str, list of content blocks, or None
    content: Union[str, List[Any], None]
    tool_call_id: str
    tool_calls: List[ToolCall]


class Config(TypedDict, total=False):
    """Provider-agnostic configuration."""

    max_tokens: int
    temperature: float
    top_p: float
    top_k: int
    stop: List[str]
    reasoning_effort: ReasoningEffort


class _ChatRequestBase(TypedDict):
    model: str
    messages: List[Message]


class ChatRequest(_ChatRequestBase, total=False):
    stream: bool
    config: Config
    provider_config: dict
    fallback: List[str]


# --- response ---------------------------------------------------------------


class Usage(TypedDict, total=False):
    input_tokens: int
    output_tokens: int
    reasoning_tokens: int
    total_tokens: int


class _ResponseMessageBase(TypedDict):
    role: str
    content: Union[str, None]


class ResponseMessage(_ResponseMessageBase, total=False):
    tool_calls: List[ToolCall]


class _ChatResponseBase(TypedDict):
    id: str
    model: str
    provider: str
    message: ResponseMessage
    usage: Usage
    latency_ms: int


class ChatResponse(_ChatResponseBase, total=False):
    reasoning: Union[str, None]


# --- models / health / error -----------------------------------------------


class ModelEntry(TypedDict):
    id: str
    provider: str
    name: str


class ModelsResponse(TypedDict):
    models: List[ModelEntry]


class HealthResponse(TypedDict):
    status: str
    providers: List[str]


class ErrorDetail(TypedDict):
    code: str
    message: str


class ErrorResponse(TypedDict):
    error: ErrorDetail


# --- stream events ----------------------------------------------------------
#
# The proxy sends each SSE event with a typed ``event:`` field. The Python
# client also injects a ``type`` key mirroring that event name so callers can
# switch on ``event["type"]`` uniformly.


class ContentEvent(TypedDict):
    type: Literal["content"]
    text: str


class ReasoningEvent(TypedDict):
    type: Literal["reasoning"]
    text: str


class ToolCallEvent(TypedDict):
    type: Literal["tool_call"]
    id: str
    name: str
    arguments: str


class UsageEvent(TypedDict, total=False):
    type: Literal["usage"]
    input_tokens: int
    output_tokens: int
    reasoning_tokens: int
    total_tokens: int


class DoneEvent(TypedDict):
    type: Literal["done"]


class ErrorEvent(TypedDict):
    type: Literal["error"]
    message: str


StreamEvent = Union[
    ContentEvent,
    ReasoningEvent,
    ToolCallEvent,
    UsageEvent,
    DoneEvent,
    ErrorEvent,
]

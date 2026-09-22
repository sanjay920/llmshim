"""
Spec-faithful type definitions for the llmshim API.

These mirror the schemas in ``api/openapi.yaml`` and are provided so that
callers can type-check their request/response handling. They are plain
``TypedDict`` / ``Literal`` aliases — nothing is validated at runtime.

Optional keys are expressed with the base/total=False inheritance pattern so
the module stays compatible with Python 3.9 (no ``typing.NotRequired``).
"""

from __future__ import annotations

from typing import Any, List, Literal, Optional, TypedDict, Union

__all__ = [
    "Role",
    "ReasoningEffort",
    "ReasoningMode",
    "ReasoningOrigin",
    "ReasoningBlock",
    "ReasoningDelta",
    "ThoughtSignature",
    "WireToolId",
    "ToolCallFunction",
    "ToolCall",
    "Message",
    "Config",
    "CachePolicy",
    "ShimConfig",
    "CacheSegment",
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
# Unified reasoning depth: mapped per provider/model with clamping to the
# nearest supported tier (see docs/reasoning.md in the llmshim repo).
ReasoningEffort = Literal["none", "low", "medium", "high", "xhigh", "max"]
# Unified reasoning mode: "pro" requests substantially more model work
# (native on OpenAI gpt-5.6/-pro models, emulated elsewhere).
ReasoningMode = Literal["standard", "pro"]
StreamEventType = Literal["content", "reasoning", "tool_call", "usage", "done", "error"]


class ReasoningOrigin(TypedDict, total=False):
    provider: str
    model: str
    family: Union[str, None]
    wire: str
    received_at: str
    account: str


class ReasoningBlock(TypedDict, total=False):
    kind: Literal["text", "redacted", "encrypted"]
    text: str
    data: str
    signature: str
    item_id: str
    origin: ReasoningOrigin
    payload: Any
    source_field: str


class ReasoningDelta(ReasoningBlock, total=False):
    index: Union[int, str]
    replace: bool


class ThoughtSignature(TypedDict):
    data: str
    origin: ReasoningOrigin


class WireToolId(TypedDict, total=False):
    signature_field: str
    provider: str
    wire: str
    scope: str
    part_id: str
    id: Union[str, None]
    item_id: str


# --- tool calls -------------------------------------------------------------


class ToolCallFunction(TypedDict, total=False):
    """The ``function`` object inside a tool call."""

    name: str
    arguments: str  # JSON-encoded arguments


class ToolCall(TypedDict, total=False):
    wire_ids: List[WireToolId]
    id: str
    type: Literal["function"]
    function: ToolCallFunction
    thought_signature: ThoughtSignature


# --- request ----------------------------------------------------------------


class _MessageBase(TypedDict):
    role: Role


class Message(_MessageBase, total=False):
    """A conversation message (request side)."""

    # str, list of content blocks, or None
    content: Union[str, List[Any], None]
    tool_call_id: str
    tool_calls: List[ToolCall]
    reasoning: List[ReasoningBlock]


class Config(TypedDict, total=False):
    """Provider-agnostic configuration."""

    max_tokens: int
    temperature: float
    top_p: float
    top_k: int
    stop: List[str]
    reasoning_effort: ReasoningEffort
    reasoning_mode: ReasoningMode


class CacheSegment(TypedDict, total=False):
    upto_message: int
    label: str
    stability: Literal["static", "session", "turn"]


class CachePolicy(TypedDict, total=False):
    segments: List[CacheSegment]
    key: str


class ShimConfig(TypedDict, total=False):
    structured_output: Literal["auto", "native", "forced_tool", "prompt"]
    tool_calling: Literal["auto", "native", "prompt"]
    reasoning_capture: Literal["off", "forced_tool"]


_CacheRequest = TypedDict("_CacheRequest", {"x-cache": CachePolicy, "x-shim": ShimConfig}, total=False)


class _ChatRequestBase(TypedDict):
    model: str
    messages: List[Message]


class ChatRequest(_ChatRequestBase, _CacheRequest, total=False):
    response_format: dict
    stream: bool
    config: Config
    provider_config: dict
    fallback: List[str]


# --- response ---------------------------------------------------------------


#: Where a ``cost_usd`` figure came from. ``"provider"`` is the bill the
#: provider reported for the generation; ``"catalog"`` is computed from catalog
#: prices and is an upper bound.
CostSource = Literal["provider", "catalog"]


class Usage(TypedDict, total=False):
    input_tokens: int
    output_tokens: int
    reasoning_tokens: int
    total_tokens: int
    cache_read_tokens: int
    cache_write_tokens: int
    #: USD charged for this response. ``None`` means the server could not price
    #: the model — it never means free.
    cost_usd: Optional[float]
    #: Where ``cost_usd`` came from: ``"provider"`` when the provider reported
    #: what it charged for this generation (OpenRouter does), ``"catalog"``
    #: when it was computed from catalog prices, which is an upper bound.
    cost_source: CostSource


class _ResponseMessageBase(TypedDict):
    role: str
    content: Union[str, None]


class ResponseMessage(_ResponseMessageBase, total=False):
    refusal: str
    tool_calls: List[ToolCall]
    reasoning: List[ReasoningBlock]


class _ChatResponseBase(TypedDict):
    id: str
    model: str
    provider: str
    message: ResponseMessage
    usage: Usage
    latency_ms: int


_Observation = TypedDict("_Observation", {"x-llmshim-served-model": str}, total=False)


class ChatResponse(_ChatResponseBase, _Observation, total=False):
    finish_reason: str
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


class ReasoningEvent(TypedDict, total=False):
    blocks: List[ReasoningDelta]
    type: Literal["reasoning"]
    text: str


class _ToolCallEventOptional(TypedDict, total=False):
    wire_ids: List[WireToolId]
    thought_signature: ThoughtSignature


class ToolCallEvent(_ToolCallEventOptional):
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
    cache_read_tokens: int
    cache_write_tokens: int
    #: ``None`` when the server could not price the model, never ``0.0``.
    cost_usd: Optional[float]
    #: Whether ``cost_usd`` is the provider's reported bill or a catalog
    #: estimate.
    cost_source: CostSource


class _DoneBase(TypedDict):
    type: Literal["done"]


class DoneEvent(_DoneBase, _Observation, total=False):
    finish_reason: str


class _ErrorEventBase(TypedDict):
    type: Literal["error"]
    message: str


class ErrorEvent(_ErrorEventBase, total=False):
    error: dict


StreamEvent = Union[
    ContentEvent,
    ReasoningEvent,
    ToolCallEvent,
    UsageEvent,
    DoneEvent,
    ErrorEvent,
]

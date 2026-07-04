"""
llmshim — Multi-provider LLM gateway for Python.

Usage:
    import llmshim

    llmshim.configure(anthropic="sk-ant-...", openai="sk-...")
    resp = llmshim.chat("claude-sonnet-4-6", "Hello!")
    print(resp["message"]["content"])

The proxy server starts automatically on first use and stops on exit.

Spec-faithful TypedDicts live in ``llmshim.types`` (also re-exported here) for
static type-checking of requests, responses, and stream events.
"""

from llmshim import types
from llmshim._client import LlmShimError, chat, configure, health, models, stream
from llmshim.types import (
    ChatRequest,
    ChatResponse,
    Config,
    ErrorResponse,
    HealthResponse,
    Message,
    ModelEntry,
    ModelsResponse,
    ResponseMessage,
    StreamEvent,
    ToolCall,
    Usage,
)

__all__ = [
    # functions
    "chat",
    "stream",
    "models",
    "health",
    "configure",
    # errors
    "LlmShimError",
    # types module + common aliases
    "types",
    "ChatRequest",
    "ChatResponse",
    "Config",
    "ErrorResponse",
    "HealthResponse",
    "Message",
    "ModelEntry",
    "ModelsResponse",
    "ResponseMessage",
    "StreamEvent",
    "ToolCall",
    "Usage",
]
__version__ = "0.1.22"

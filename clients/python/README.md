# llmshim

One interface, every LLM provider. The proxy server starts automatically — no setup needed.

## Install

```bash
pip install llmshim
```

## Configure

```python
import llmshim

# Set API keys (writes to ~/.llmshim/config.toml — only needed once)
llmshim.configure(
    anthropic="sk-ant-...",
    openai="sk-...",
    gemini="AIza...",
    xai="xai-...",
)
```

Or from the CLI: `llmshim configure`

## Chat

```python
import llmshim

resp = llmshim.chat("claude-sonnet-4-6", "What is Rust?")
print(resp["message"]["content"])
```

With options (all map to the API's provider-agnostic `config`):

```python
resp = llmshim.chat(
    "openai/gpt-5.5",
    "Explain quicksort",
    max_tokens=500,
    temperature=0.7,
    top_p=0.9,
    top_k=40,
    stop=["\n\n"],
    reasoning_effort="high",
)
```

With message history:

```python
resp = llmshim.chat("claude-sonnet-4-6", [
    {"role": "system", "content": "You are a pirate."},
    {"role": "user", "content": "Hello!"},
], max_tokens=500)
```

## Streaming

```python
for event in llmshim.stream("claude-sonnet-4-6", "Write a poem"):
    if event["type"] == "content":
        print(event["text"], end="", flush=True)
    elif event["type"] == "reasoning":
        pass  # thinking tokens
    elif event["type"] == "usage":
        print(f"\n[↑{event['input_tokens']} ↓{event['output_tokens']}]")
```

## Multi-Model Conversations

Switch models mid-conversation. History carries over.

```python
messages = [{"role": "user", "content": "What is a closure?"}]

r1 = llmshim.chat("claude-sonnet-4-6", messages, max_tokens=500)
print(f"Claude: {r1['message']['content']}")

messages.append({"role": "assistant", "content": r1["message"]["content"]})
messages.append({"role": "user", "content": "Now explain differently."})

r2 = llmshim.chat("gpt-5.5", messages, max_tokens=500)
print(f"GPT: {r2['message']['content']}")
```

## Reasoning / Thinking

```python
resp = llmshim.chat(
    "claude-sonnet-4-6",
    "Solve: x^2 - 5x + 6 = 0",
    max_tokens=4000,
    reasoning_effort="high",
)
print(resp["reasoning"])        # thinking content
print(resp["message"]["content"])  # answer
```

## Tool Use / Function Calling

```python
tools = [{
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "Get current weather",
        "parameters": {
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        },
    },
}]

resp = llmshim.chat("claude-sonnet-4-6", "Weather in Tokyo?", max_tokens=500, tools=tools)
for tc in resp["message"].get("tool_calls", []):
    print(f"{tc['function']['name']}({tc['function']['arguments']})")
```

Tools are accepted in OpenAI Chat Completions format and auto-translated to each provider's native format.

## Fallback Chains

```python
resp = llmshim.chat(
    "anthropic/claude-sonnet-4-6",
    "Hello",
    max_tokens=100,
    fallback=["openai/gpt-5.5", "gemini/gemini-3-flash-preview"],
)
```

## Error Handling

Non-streaming errors (bad model, unknown provider, provider failures) raise
`LlmShimError`, which carries the API's structured `error` fields:

```python
try:
    llmshim.chat("unknown/model", "hi")
except llmshim.LlmShimError as e:
    print(e.status_code)  # 400
    print(e.code)         # "bad_request"
    print(e.message)      # human-readable message
```

Streaming errors that occur mid-stream instead arrive as an `error` event
(`event["type"] == "error"`); an HTTP error before the stream starts still
raises `LlmShimError`.

## Types

Spec-faithful `TypedDict` definitions are available in `llmshim.types` (and the
common ones are re-exported at the top level) for static type-checking:

```python
from llmshim.types import ChatResponse, StreamEvent, Message, Config

resp: ChatResponse = llmshim.chat("claude-sonnet-4-6", "hi")
```

Available: `ChatRequest`, `ChatResponse`, `Config`, `Message`, `ToolCall`,
`Usage`, `ResponseMessage`, `ModelEntry`, `ModelsResponse`, `HealthResponse`,
`ErrorResponse`, and the `StreamEvent` union (`ContentEvent`, `ReasoningEvent`,
`ToolCallEvent`, `UsageEvent`, `DoneEvent`, `ErrorEvent`).

## Other

```python
llmshim.models()   # list available models
llmshim.health()   # {"status": "ok", "providers": [...]}
```

## How It Works

On first call, the package:
1. Finds the `llmshim` binary (bundled, on PATH, or in repo)
2. Starts the proxy on a random localhost port
3. Routes your request through it
4. Server stops automatically when Python exits

No Docker, no background services, no manual server management.

## Development

The unit tests under `tests/` are fully mocked — they run a local HTTP server
returning canned JSON and SSE, so they need no API keys and make no real
provider calls:

```bash
pip install httpx pytest
pytest tests/
```

`test_e2e.py` is a separate LIVE suite that spawns the real binary and makes
billed provider calls; run it only when you deliberately want to hit real APIs.

## Supported Models

| Provider | Models |
|----------|--------|
| OpenAI | `gpt-5.5`, `gpt-5.5-pro`, `gpt-5.4`, `gpt-5.4-pro`, `gpt-5.4-mini`, `gpt-5.4-nano` |
| Anthropic | `claude-opus-5`, `claude-opus-4-8`, `claude-sonnet-5`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001` |
| Gemini | `gemini-3.5-flash`, `gemini-3.1-pro-preview`, `gemini-3-flash-preview` |
| xAI | `grok-4.5`, `grok-4.3`, `grok-4.20-multi-agent-beta-0309`, `grok-4.20-beta-0309-reasoning`, `grok-4.20-beta-0309-non-reasoning` |

Call `llmshim.models()` for the live list filtered to your configured providers.

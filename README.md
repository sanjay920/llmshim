# llmshim

A blazing fast LLM API translation layer in pure Rust. One interface, every provider.

## What it does

Send requests through llmshim → it translates to whichever provider you choose → translates the response back. Zero infrastructure, zero databases, ~5MB binary.

```python
import llmshim

resp = llmshim.chat("claude-sonnet-4-6", "What is Rust?")
print(resp["message"]["content"])
```

Switch providers by changing the model string. Everything else stays the same.

## Install

```bash
pip install llmshim
```

Or from source:

```bash
cargo install --path . --features proxy
```

## Configure

```python
import llmshim

# Set API keys once — persisted to ~/.llmshim/config.toml
llmshim.configure(
    anthropic="sk-ant-...",
    openai="sk-...",
    gemini="AIza...",
    xai="xai-...",
)
```

Or from the CLI: `llmshim configure`

## Supported models

| Provider | Models | Reasoning visible |
|----------|--------|-------------------|
| **OpenAI** | `gpt-5.4` | Yes (summaries) |
| **Anthropic** | `claude-opus-4-6`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001` | Yes (full thinking) |
| **Google Gemini** | `gemini-3.1-pro-preview`, `gemini-3-flash-preview`, `gemini-3.1-flash-lite-preview` | Yes (thought summaries) |
| **xAI** | `grok-4-1-fast-reasoning`, `grok-4-1-fast-non-reasoning` | No (hidden) |

## Chat

```python
import llmshim

# Simple
resp = llmshim.chat("claude-sonnet-4-6", "Hello!", max_tokens=500)
print(resp["message"]["content"])

# With message history
resp = llmshim.chat("gpt-5.4", [
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

## Multi-model conversations

Switch models mid-conversation. History carries over.

```python
messages = [{"role": "user", "content": "What is a closure?"}]

r1 = llmshim.chat("claude-sonnet-4-6", messages, max_tokens=500)
print(f"Claude: {r1['message']['content']}")

messages.append({"role": "assistant", "content": r1["message"]["content"]})
messages.append({"role": "user", "content": "Now explain differently."})

r2 = llmshim.chat("gpt-5.4", messages, max_tokens=500)
print(f"GPT: {r2['message']['content']}")
```

## Tool use

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

## Reasoning / thinking

```python
resp = llmshim.chat(
    "claude-sonnet-4-6",
    "Solve: x^2 - 5x + 6 = 0",
    max_tokens=4000,
    reasoning_effort="high",
)
print(resp["reasoning"])          # thinking content
print(resp["message"]["content"]) # answer
```

## Fallback chains

```python
resp = llmshim.chat(
    "anthropic/claude-sonnet-4-6",
    "Hello",
    max_tokens=100,
    fallback=["openai/gpt-5.4", "gemini/gemini-3-flash-preview"],
)
```

## Proxy server

llmshim runs as an HTTP proxy with its own API spec. Any language can talk to it.

```bash
llmshim proxy
# Listening on http://localhost:3000
```

```bash
curl http://localhost:3000/v1/chat \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"Hi"}],"config":{"max_tokens":100}}'
```

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/chat` | Chat completion (or streaming with `stream: true`) |
| `POST` | `/v1/chat/stream` | Always-streaming SSE with typed events |
| `GET` | `/v1/models` | List available models |
| `GET` | `/health` | Health check |

Full API spec: [`api/openapi.yaml`](api/openapi.yaml)

## Docker

```bash
llmshim docker build
llmshim docker start
llmshim docker status
llmshim docker logs
llmshim docker stop
```

## CLI

```bash
llmshim                     # show help
llmshim chat                # interactive multi-model chat
llmshim configure           # set API keys
llmshim set <key> <value>   # set a config value
llmshim list                # show configured keys
llmshim models              # list available models
llmshim proxy               # start HTTP proxy
```

## How it works

No canonical struct. Requests flow as `serde_json::Value` — each provider maps only what it understands. Adding a provider = implementing one trait with three methods.

```
llmshim::completion(router, request)
  → router.resolve("anthropic/claude-sonnet-4-6")
  → provider.transform_request(model, &value)
  → HTTP
  → provider.transform_response(model, body)
```

## Key features

- **Multi-model conversations** — switch providers mid-chat, history carries over
- **Reasoning/thinking** — visible chain-of-thought from OpenAI, Anthropic, and Gemini
- **Streaming** — token-by-token with thinking in dim grey
- **Tool use** — Chat Completions format auto-translated to each provider
- **Vision/images** — send images in any format, auto-translated between providers
- **Fallback chains** — automatic failover across providers with exponential backoff
- **Cross-provider translation** — system messages, tool calls, and provider-specific fields all handled

## Build & test

```bash
cargo build                                    # dev build
cargo build --release --features proxy         # release build
cargo test --features proxy --tests            # unit tests (~380)
cargo test --features proxy -- --ignored       # integration tests (needs API keys)
```

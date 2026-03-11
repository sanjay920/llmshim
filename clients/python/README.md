# llmshim Python Client

One interface, every LLM provider. The proxy server starts automatically — no separate process to manage.

## Install

```bash
pip install llmshim
```

For development (from the repo):

```bash
# Build the binary
cargo build --release --features proxy

# Copy into the package
cp target/release/llmshim clients/python/llmshim/bin/

# Install in dev mode
pip install -e clients/python/
```

## Quick Start

```python
from llmshim import LlmShim

client = LlmShim()  # server starts automatically

resp = client.chat("claude-sonnet-4-6", "What is Rust?")
print(resp["message"]["content"])
```

That's it. No server to start, no config to write (if you've run `llmshim configure`).

## Chat

```python
# Simple string message
resp = client.chat("claude-sonnet-4-6", "Hello!", max_tokens=500)
print(resp["message"]["content"])
print(f"Provider: {resp['provider']}, Latency: {resp['latency_ms']}ms")

# Message array with system prompt
resp = client.chat("gpt-5.4", [
    {"role": "system", "content": "You are a pirate."},
    {"role": "user", "content": "Hello!"},
], max_tokens=500)
```

## Streaming

```python
for event in client.stream("claude-sonnet-4-6", "Write a poem"):
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

# Ask Claude
r1 = client.chat("claude-sonnet-4-6", messages, max_tokens=500)
print(f"Claude: {r1['message']['content']}")

# Continue with GPT
messages.append({"role": "assistant", "content": r1["message"]["content"]})
messages.append({"role": "user", "content": "Now explain it differently."})
r2 = client.chat("gpt-5.4", messages, max_tokens=500)
print(f"GPT: {r2['message']['content']}")
```

## Reasoning / Thinking

```python
# Via config (all providers)
resp = client.chat("claude-sonnet-4-6", "Solve: x^2 - 5x + 6 = 0",
                    reasoning_effort="high", max_tokens=4000)
print(resp["reasoning"])  # thinking content
print(resp["message"]["content"])  # answer

# Via provider-specific config
resp = client.chat("claude-sonnet-4-6", "Complex problem...",
                    max_tokens=4000,
                    provider_config={"thinking": {"type": "enabled", "budget_tokens": 5000}})
```

## Fallback Chains

```python
resp = client.chat(
    "anthropic/claude-sonnet-4-6",
    "Hello",
    max_tokens=100,
    fallback=["openai/gpt-5.4", "gemini/gemini-3-flash-preview"],
)
```

## Other Methods

```python
# List available models
for m in client.models():
    print(f"{m['id']} ({m['provider']})")

# Health check
print(client.health())
```

## How It Works

The first time you call any method, the package:
1. Finds the `llmshim` binary (bundled in package, on PATH, or in repo)
2. Starts the proxy server on a random localhost port
3. Sends your request through the proxy
4. Server auto-stops when your Python process exits

No Docker, no background services, no config files needed.

## Supported Models

| Provider | Models |
|----------|--------|
| OpenAI | `gpt-5.4` |
| Anthropic | `claude-opus-4-6`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001` |
| Gemini | `gemini-3.1-pro-preview`, `gemini-3-flash-preview`, `gemini-3.1-flash-lite-preview` |
| xAI | `grok-4-1-fast-reasoning`, `grok-4-1-fast-non-reasoning` |

Configure API keys with `llmshim configure` or set environment variables.

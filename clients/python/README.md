# llmshim Python Client

Generated from the llmshim OpenAPI spec. Talk to any LLM through one API.

## Install

```bash
pip install httpx attrs
```

## Quick Start

```python
from llmshim import LlmShim

client = LlmShim()  # defaults to http://localhost:3000

# Simple chat
resp = client.chat("claude-sonnet-4-6", "What is Rust?")
print(resp.message.content)

# With config
resp = client.chat(
    "openai/gpt-5.4",
    "Explain quicksort",
    max_tokens=500,
    temperature=0.7,
)
```

## Streaming

```python
for event in client.stream("claude-sonnet-4-6", "Write a poem about Rust"):
    if event["type"] == "content":
        print(event["text"], end="", flush=True)
    elif event["type"] == "reasoning":
        pass  # thinking tokens
    elif event["type"] == "usage":
        print(f"\n[{event['input_tokens']}in / {event['output_tokens']}out]")
```

## Multi-Model Conversations

Switch models mid-conversation. History carries over.

```python
messages = [
    {"role": "system", "content": "You are a helpful tutor."},
    {"role": "user", "content": "What is a closure?"},
]

# Ask Claude
r1 = client.chat("claude-sonnet-4-6", messages, max_tokens=500)
print(f"Claude: {r1.message.content}")

# Continue with GPT
messages.append({"role": "assistant", "content": r1.message.content})
messages.append({"role": "user", "content": "Now explain it differently."})
r2 = client.chat("gpt-5.4", messages, max_tokens=500)
print(f"GPT: {r2.message.content}")
```

## Reasoning / Thinking

```python
# Via config (works across all providers)
resp = client.chat("claude-sonnet-4-6", "What is 15*37?", reasoning_effort="high")
print(resp.reasoning)  # thinking content
print(resp.message.content)  # answer

# Via provider-specific config
resp = client.chat(
    "claude-sonnet-4-6",
    "Solve this step by step: x^2 - 5x + 6 = 0",
    max_tokens=4000,
    provider_config={"thinking": {"type": "enabled", "budget_tokens": 2000}},
)
```

## Other Methods

```python
# List available models
for m in client.models():
    print(f"{m['id']} ({m['provider']})")

# Health check
print(client.health())  # {"status": "ok", "providers": ["openai", "anthropic", ...]}
```

## Running the Server

```bash
# From the llmshim repo root
cargo run --features proxy --bin llmshim-proxy
```

Configure API keys with `llmshim configure` or set environment variables: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `XAI_API_KEY`.

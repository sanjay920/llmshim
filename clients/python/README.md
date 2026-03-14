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

With options:

```python
resp = llmshim.chat(
    "openai/gpt-5.4",
    "Explain quicksort",
    max_tokens=500,
    temperature=0.7,
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

r2 = llmshim.chat("gpt-5.4", messages, max_tokens=500)
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
    fallback=["openai/gpt-5.4", "gemini/gemini-3-flash-preview"],
)
```

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

## Supported Models

| Provider | Models |
|----------|--------|
| OpenAI | `gpt-5.4` |
| Anthropic | `claude-opus-4-6`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001` |
| Gemini | `gemini-3.1-pro-preview`, `gemini-3-flash-preview`, `gemini-3.1-flash-lite-preview` |
| xAI | `grok-4.20-multi-agent-beta-0309`, `grok-4.20-beta-0309-reasoning`, `grok-4.20-beta-0309-non-reasoning`, `grok-4-1-fast-reasoning`, `grok-4-1-fast-non-reasoning` |

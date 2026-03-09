# llmshim

A blazing fast LLM API translation layer in pure Rust. One interface, every provider.

## What it does

Send requests through llmshim's API → it translates to whichever provider you choose → translates the response back. Zero infrastructure, zero databases, ~5MB binary.

```python
from llmshim import LlmShim

client = LlmShim()
resp = client.chat("claude-sonnet-4-6", "What is Rust?")
print(resp.message.content)
```

Switch providers by changing the model string. Everything else stays the same.

## Supported providers & models

| Provider | Models | Reasoning visible |
|----------|--------|-------------------|
| **OpenAI** | `gpt-5.4` | Yes (summaries) |
| **Anthropic** | `claude-opus-4-6`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001` | Yes (full thinking) |
| **Google Gemini** | `gemini-3.1-pro-preview`, `gemini-3-flash-preview`, `gemini-3.1-flash-lite-preview` | Yes (thought summaries) |
| **xAI** | `grok-4-1-fast-reasoning`, `grok-4-1-fast-non-reasoning` | No (hidden) |

## Quick start

```bash
# Install
cargo install --path . --features proxy

# Configure API keys (interactive, like aws configure)
llmshim configure

# Or set keys individually
llmshim set openai sk-...
llmshim set anthropic sk-ant-...

# Start the interactive chat
llmshim

# Start the proxy server
llmshim proxy
```

Keys are stored in `~/.llmshim/config.toml`. You can also use `.env` files or environment variables — precedence is: env vars > `.env` > config file.

### Docker

```bash
# Build the image
llmshim docker build

# Start the proxy (uses keys from ~/.llmshim/config.toml or env vars)
llmshim docker start

# Check status, view logs, stop
llmshim docker status
llmshim docker logs
llmshim docker stop

# Or run directly with docker
docker run -p 3000:3000 -e OPENAI_API_KEY=sk-... llmshim
```

## Proxy Server

llmshim runs as an HTTP proxy with its own API spec (not OpenAI-compatible). Any language can talk to it.

```bash
cargo run --features proxy --bin llmshim-proxy
# Listening on http://localhost:3000
```

### Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/chat` | Chat completion (or streaming with `stream: true`) |
| `POST` | `/v1/chat/stream` | Always-streaming SSE with typed events |
| `GET` | `/v1/models` | List available models |
| `GET` | `/health` | Health check |

### Request format

```json
{
  "model": "anthropic/claude-sonnet-4-6",
  "messages": [{"role": "user", "content": "Hello!"}],
  "config": {
    "max_tokens": 1000,
    "reasoning_effort": "high"
  },
  "provider_config": {
    "thinking": {"type": "adaptive"}
  }
}
```

`config` holds provider-agnostic settings. `provider_config` passes raw JSON to the underlying provider for features like Anthropic thinking or Gemini safety settings.

### Fallback chains

Add `fallback` to automatically try other models when the primary fails (429, 500, 502, 503):

```json
{
  "model": "anthropic/claude-sonnet-4-6",
  "messages": [{"role": "user", "content": "Hello"}],
  "fallback": ["openai/gpt-5.4", "gemini/gemini-3-flash-preview"]
}
```

Each model is retried up to 2 times with exponential backoff before moving to the next. Also available as a Rust API:

```rust
let config = FallbackConfig::new(vec![
    "anthropic/claude-sonnet-4-6".into(),
    "openai/gpt-5.4".into(),
]);
let resp = llmshim::completion_with_fallback(&router, &request, &config, None).await?;
```

### Streaming events

The `/v1/chat/stream` endpoint emits typed SSE events:

```
event: reasoning
data: {"type":"reasoning","text":"Let me think..."}

event: content
data: {"type":"content","text":"The answer is 42."}

event: usage
data: {"type":"usage","input_tokens":30,"output_tokens":50}

event: done
data: {"type":"done"}
```

## Client Libraries

Generated from the [OpenAPI spec](api/openapi.yaml). Install and go.

### Python

```python
from llmshim import LlmShim

client = LlmShim()

# Chat
resp = client.chat("claude-sonnet-4-6", "Hello!", max_tokens=500)
print(resp.message.content)

# Stream
for event in client.stream("gpt-5.4", "Write a poem"):
    if event["type"] == "content":
        print(event["text"], end="")

# Multi-model conversation
messages = [{"role": "user", "content": "What is Rust?"}]
r1 = client.chat("claude-sonnet-4-6", messages, max_tokens=500)
messages.append({"role": "assistant", "content": r1.message.content})
messages.append({"role": "user", "content": "Now explain it differently."})
r2 = client.chat("gpt-5.4", messages, max_tokens=500)
```

See [`clients/python/`](clients/python/) for setup.

### TypeScript

```typescript
import { LlmShim } from "./src/index.ts";

const client = new LlmShim();

// Chat
const resp = await client.chat("claude-sonnet-4-6", "Hello!", { max_tokens: 500 });
console.log(resp.message.content);

// Stream
for await (const chunk of client.stream("gpt-5.4", "Write a poem")) {
  if (chunk.type === "content") process.stdout.write(chunk.text!);
}
```

See [`clients/typescript/`](clients/typescript/) for setup.

### curl

```bash
curl http://localhost:3000/v1/chat \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"Hi"}],"config":{"max_tokens":100}}'
```

## Interactive CLI

```
$ llmshim

  llmshim — multi-provider LLM chat

  1. GPT-5.4
  2. Claude Opus 4.6
  3. Claude Sonnet 4.6
  ...

  Select model [1-9]: 3

you: What is Rust?
Claude Sonnet 4.6: [thinking in dim grey...]
Rust is a systems programming language...
  [2.1s · ↑ 30 · ↓ 150 tokens]

you: /model gpt
  Switched to: GPT-5.4

you: Tell me more
GPT-5.4: [continues the conversation...]
```

Commands: `/model` (switch by name, number, or fuzzy match), `/clear`, `/history`, `/quit`

## Key features

- **Multi-model conversations** — switch providers mid-chat, history carries over
- **Reasoning/thinking** — visible chain-of-thought from OpenAI, Anthropic, and Gemini
- **Streaming** — token-by-token with thinking in dim grey, answers in default color
- **Vision/images** — send images in any format, auto-translated between providers
- **Retry + fallback chains** — automatic failover across providers with exponential backoff
- **Cross-provider translation** — tool calls, system messages, and provider-specific fields all handled automatically
- **JSONL logging** — `llmshim chat --log llmshim.log`
- **OpenAPI spec** — generate clients for any language from `api/openapi.yaml`

## Architecture

No canonical struct. Requests flow as `serde_json::Value` — each provider maps only what it understands. Adding a provider = implementing one trait with three methods.

```
llmshim::completion(router, request)
  → router.resolve("anthropic/claude-sonnet-4-6")
  → provider.transform_request(model, &value)
  → HTTP
  → provider.transform_response(model, body)
```

## Build & Test

```bash
cargo build                                          # dev build
cargo build --release --features proxy               # release build with proxy
cargo test --tests                                   # unit tests (~288)
cargo test -- --ignored                              # integration tests (needs API keys)
cargo test --features proxy --tests                  # includes proxy tests
llmshim                                              # interactive chat
llmshim configure                                    # setup API keys
llmshim proxy                                        # proxy server (needs --features proxy)
llmshim models                                       # list available models
```

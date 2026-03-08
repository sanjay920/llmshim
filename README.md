# llmshim

A blazing fast LLM API translation layer in pure Rust. One interface, every provider.

## What it does

Send OpenAI-format requests → llmshim translates them to the native API of whichever provider you choose → translates the response back. Zero infrastructure, zero databases, 3.7MB binary.

```rust
let router = Router::from_env();
let request = json!({
    "model": "anthropic/claude-sonnet-4-6",
    "messages": [{"role": "user", "content": "Hello!"}],
    "max_tokens": 1000,
});
let response = llmshim::completion(&router, &request).await?;
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
# Set API keys
cp .env.example .env
# Edit .env with your keys

# Run the interactive chat
cargo run

# Or use as a library
cargo add llmshim
```

## Interactive CLI

```
$ cargo run

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

Commands: `/model` (switch), `/clear` (reset), `/history`, `/quit`

## Key features

- **Multi-model conversations** — switch providers mid-chat, history carries over
- **Reasoning/thinking** — visible chain-of-thought from OpenAI, Anthropic, and Gemini
- **Streaming** — token-by-token output with thinking in dim grey
- **Cross-provider translation** — tool calls, system messages, and provider-specific fields all handled
- **JSONL logging** — `cargo run -- --log llmshim.log`

## Architecture

No canonical struct. Requests flow as `serde_json::Value` — each provider maps only what it understands. Adding a provider = implementing one trait with three methods.

```
completion(router, request)
  → router.resolve("anthropic/claude-sonnet-4-6")
  → provider.transform_request(model, &value)
  → HTTP
  → provider.transform_response(model, body)
```

## Testing

```bash
cargo test --tests            # 168 unit tests
cargo test -- --ignored       # 52 integration tests (needs API keys)
```

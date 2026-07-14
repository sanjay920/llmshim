# Two contracts, one engine

llmshim exposes one set of capabilities through two data contracts. The Rust
crate uses flexible OpenAI-shaped JSON. The HTTP proxy uses a smaller request
and response envelope designed for llmshim's clients.

The contracts are related, but they are not wire-compatible.

## Rust: OpenAI-shaped `Value`

`llmshim::completion` and `llmshim::stream` accept a
`serde_json::Value`. Conversation fields and controls live at the top level:

```rust
let request = serde_json::json!({
    "model": "anthropic/claude-sonnet-4-6",
    "messages": [{ "role": "user", "content": "Hello" }],
    "max_tokens": 200,
    "reasoning_effort": "high"
});
```

A non-streaming result uses an OpenAI Chat Completions-style shape. The answer
is normally at `choices[0].message.content`; translated tool calls live beside
it, and provider-returned reasoning uses `reasoning_content` when available.

Rust streaming yields JSON strings in the normalized Chat Completions chunk
shape. It does not yield the proxy's typed event objects.

## Proxy: a compact llmshim envelope

The proxy accepts `model` and `messages`, then groups portable controls under
`config`:

```json
{
  "model": "anthropic/claude-sonnet-4-6",
  "messages": [{ "role": "user", "content": "Hello" }],
  "config": {
    "max_tokens": 200,
    "reasoning_effort": "high"
  }
}
```

A non-streaming proxy response is a compact `ChatResponse`:

```json
{
  "id": "...",
  "model": "claude-sonnet-4-6",
  "provider": "anthropic",
  "message": { "role": "assistant", "content": "..." },
  "usage": {
    "input_tokens": 8,
    "output_tokens": 12,
    "total_tokens": 20
  },
  "latency_ms": 640
}
```

When the provider returns reasoning content, the response also includes the
optional `reasoning` field.

The streaming endpoints emit typed SSE events rather than Chat Completions
chunks. Language clients decode the same HTTP contract into their native types.

## Where this goes

Use this placement convention throughout the documentation:

| Meaning | Rust crate | CLI chat | HTTP proxy | Language clients |
|---|---|---|---|---|
| Conversation | top-level `messages` | interactive history | top-level `messages` | method argument or request field |
| Portable controls | top-level fields | selected by the CLI | fields under `config` | convenience arguments or `config` |
| Tools | top-level `tools` | not exposed as a tool loop | `provider_config.tools` | Python/Ruby convenience; otherwise `provider_config` |
| Native controls | top-level `x-*` key | not exposed | `provider_config["x-*"]` | `provider_config` |
| Non-stream result | Chat Completions-style `Value` | rendered text and usage | compact `ChatResponse` | native wrapper around `ChatResponse` |
| Stream result | normalized JSON chunks | rendered live output | typed SSE events | iterator, generator, channel, or block |

The CLI is a workflow, not a fourth JSON contract. It builds requests, keeps
history for the current process, and renders results for a person.

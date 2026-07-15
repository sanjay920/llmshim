# Surface capability matrix

The Rust crate is the engine. The CLI calls it in-process, the proxy places an
HTTP boundary around it, and the language clients speak that proxy contract.

> **Availability:** Crate: in-process Rust · CLI: human workflow · Proxy: language-neutral server · Clients: proxy SDKs

## Choose the boundary

| | Rust crate | CLI | HTTP proxy | Language clients |
|---|---|---|---|---|
| Primary entrypoint | `llmshim::completion` / `stream` | `llmshim chat` | `POST /v1/chat` | `chat` / `stream` method |
| Transport | In-process calls; engine uses provider HTTPS | Terminal around the engine | JSON over HTTP; SSE for streams | HTTP/SSE to the proxy |
| Conversation state | Your application | CLI process | Caller | Caller |
| Provider keys | Rust process | CLI process | Proxy process | Proxy process, never client requests |
| Request contract | OpenAI-shaped `serde_json::Value` | CLI-owned | Compact `ChatRequest` | Native wrapper around `ChatRequest` |
| Non-stream result | OpenAI Chat Completions-shaped `Value` | Rendered text/usage | Compact `ChatResponse` | Native wrapper around `ChatResponse` |
| Streaming | Normalized OpenAI-delta JSON strings | Rendered live | Typed SSE | Iterator, async iterator, channel, or block over typed events |
| Process behavior | Embedded | One CLI process | Long-running server | Python/TS auto-start a bundled proxy; Go/Ruby connect to one |

No surface stores server-side conversation memory. The caller resends history;
the CLI merely does that for its current interactive session.

## Where request features go

| Meaning | Rust crate | CLI chat | HTTP proxy | Language clients |
|---|---|---|---|---|
| Model | top-level `model` | picker and `/model` | top-level `model` | request/model argument |
| Messages | top-level `messages` | current session history | top-level `messages` | request/messages argument |
| Portable controls | top-level fields | CLI-selected defaults | fields under `config` | convenience arguments or `config` |
| Tools | top-level `tools` | no tool loop | `provider_config.tools` | Python/Ruby convenience; otherwise `provider_config` |
| Native controls | top-level `x-openai`, `x-anthropic`, or `x-gemini` | not exposed | same namespace inside `provider_config` | `provider_config` |
| Fallbacks | `completion_with_fallback` configuration | not exposed | top-level `fallback` | `fallback` request field |

The proxy's `provider_config` object is merged into the OpenAI-shaped engine
request. That is why tools and native namespaces move under it without changing
their inner shapes.

## Build features

| Capability | Cargo feature |
|---|---|
| Core translation, Rust API, and CLI chat/config commands | default build; no optional feature |
| `llmshim proxy` and the Rust `llmshim::proxy` module | `proxy` |
| Redis-coordinated proxy rate limits | `redis-coordination` (includes `proxy`) |

The Homebrew package and the binaries bundled by Python and TypeScript include
proxy support. Go and Ruby are pure HTTP clients and require a separately
running proxy.

One typed-wrapper limitation matters for Gemini tool loops: raw proxy JSON
round-trips `thought_signature`, but the Go and Ruby typed structs do not
currently surface it. Use raw JSON if that field is required.

For shape details, continue to the [request field map](request-fields.md) and
[HTTP API](../proxy/http-api.md).

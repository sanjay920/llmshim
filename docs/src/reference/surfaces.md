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

The caller resends conversation history; the CLI does that for its current
interactive session. Native HTTP facades persist private replay metadata for
issued reasoning and tool IDs; see [native endpoints](../proxy/native-apis.md).

## Where request features go

| Meaning | Rust crate | CLI chat | HTTP proxy | Language clients |
|---|---|---|---|---|
| Model | top-level `model` | picker and `/model` | top-level `model` | request/model argument |
| Messages | top-level `messages` | current session history | top-level `messages` | request/messages argument |
| Portable controls | top-level fields | CLI-selected defaults | fields under `config` | convenience arguments or `config` |
| Tools | top-level `tools` | no tool loop | `provider_config.tools` | Python/Ruby convenience; otherwise `provider_config` |
| Native controls | top-level `x-openai`, `x-anthropic`, or `x-gemini` | not exposed | same namespace inside `provider_config` | `provider_config` |
| Structured output and shims | `response_format`, `x-shim` | not exposed | same top-level fields | typed fields or convenience arguments |
| Fallbacks | `completion_with_fallback` configuration | not exposed | top-level `fallback` | `fallback` request field |

The proxy's `provider_config` object is merged into the OpenAI-shaped engine
request. That is why tools and native namespaces move under it without changing
their inner shapes. Root `model` and `messages` are reserved. After route and
alias resolution, the selected provider's namespace also cannot replace its
native model or main history/input container. Native system and instruction
controls remain supported. Explicit fallback targets receive the same checks,
so a namespace is inert only when no selected fallback or route can activate
it.

## Build features

| Capability | Cargo feature |
|---|---|
| Core translation, Rust API, and CLI chat/config commands | default build |
| Best-effort signature observations | `signature-introspection` (default on) |
| `llmshim proxy` and the Rust `llmshim::proxy` module | `proxy` |
| Redis-coordinated proxy rate limits | `redis-coordination` (includes `proxy`) |

The Homebrew package and the binaries bundled by Python and TypeScript include
proxy support. Go and Ruby are pure HTTP clients and require a separately
running proxy.

All language clients expose reasoning blocks and the tool-call signature object
(`data` plus `origin`). Preserve those objects for replay.

## Upstream response limits

The engine limits ordinary JSON completion bodies to 32 MiB and provider error
bodies to 64 KiB, measured after HTTP decompression. It checks decoded chunks
before adding them to its body buffer. Oversized final bodies return a fixed
502 error without including a truncated provider response. Retry bodies use the
same error-body buffer limit; oversized bodies are dropped before another attempt.

These limits apply through the Rust completion API, CLI, proxy, and gateway.
They bound decoded body bytes; JSON allocations and process memory have
additional overhead. Successful raw responses returned by the low-level
`ShimClient::send` API use the configured connect and response-header
deadlines. After headers are returned, body size, decoding, idle time, total
lifetime, and cancellation remain the caller's responsibility; llmshim does not
attach a hidden body task or timer to that raw response.

Normal provider streams, ChatGPT's collected SSE replies, and native HTTP
facades use one bounded SSE data decoder. It checks input before growing its
line and frame buffers:

| Per-stream limit | Maximum |
|---|---:|
| Decoded transport bytes | 32 MiB |
| Line content | 8 MiB |
| Frame bytes, with CRLF treated as one line ending | 8 MiB |
| Blank-line-delimited frames, including empty frames | 100,000 |
| Input chunks, including empty chunks | 1,048,576 |
| Retained normalized tool/reasoning/choice state | estimated 16 MiB and 4,096 entries |
| Retained native usage/finality state | estimated 4 MiB and 4,096 entries |

The decoder handles UTF-8 fragments, an initial byte-order mark, multiline
`data` fields, comments, and LF/CRLF/CR line endings. Provider transforms consume
the data fields; unused SSE metadata is discarded. Limits terminate the stream
with a fixed error and release its input source. EOF does not manufacture a
completed event from an unterminated frame. These are framing limits, not an
RSS ceiling or a transport timeout.

The retained-state limits count only data held across events, including map and
JSON-container overhead. Forwarded text is not charged after its chunk is
released. Exceeding either retained-state limit ends the response with
`upstream stream retained state exceeds limit`; no partial tool call or
reasoning block is emitted. Rust embedders can inject smaller finite limits with
`StreamRetentionLimits` for testing or a stricter deployment policy.

Collected text and refusals, reasoning text and opaque fragments, and native
facade text append to their existing buffers. Completed reasoning snapshots
still replace their matching block while preserving its original provenance.

For shape details, continue to the [request field map](request-fields.md) and
[HTTP API](../proxy/http-api.md).

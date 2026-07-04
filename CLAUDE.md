# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is llmshim

A pure Rust LLM API translation layer. Takes OpenAI-format JSON requests, translates them to provider-native formats (and back), with zero infrastructure requirements. Supports OpenAI (Responses API), Anthropic, Google Gemini, and xAI. Includes an interactive CLI chat with streaming, reasoning, and mid-conversation model switching.

**Published on crates.io as `llmshim`** — https://crates.io/crates/llmshim

This is a public crate. Do NOT make breaking changes to `pub` items in `src/lib.rs`, `src/router.rs`, `src/provider.rs`, `src/error.rs`, `src/fallback.rs`, `src/log.rs`, `src/config.rs`, `src/models.rs`, or `src/vision.rs` without a semver bump. The `ragents` crate (https://github.com/sanjay920/ragents) depends on this.

## Supported models

- **OpenAI:** `gpt-5.5`, `gpt-5.5-pro`, `gpt-5.4`, `gpt-5.4-pro`, `gpt-5.4-mini`, `gpt-5.4-nano`
- **Anthropic:** `claude-opus-4-8`, `claude-sonnet-5`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001`
- **Gemini:** `gemini-3.5-flash`, `gemini-3.1-pro-preview`, `gemini-3.1-flash-lite-preview`, `gemini-3-flash-preview`
- **xAI:** `grok-4.3`, `grok-4.20-multi-agent-beta-0309`, `grok-4.20-beta-0309-reasoning`, `grok-4.20-beta-0309-non-reasoning`, `grok-4-1-fast-reasoning`, `grok-4-1-fast-non-reasoning`

## Build & Test

```bash
cargo build                                          # dev build
cargo build --release                                # release build (~6MB binary)
cargo test --tests                                   # unit tests (~326)
cargo test -- --ignored                              # integration tests (needs API keys)
cargo test --features proxy --tests                  # unit tests including proxy
cargo test --features proxy -- --ignored             # all integration tests including proxy
cargo run                                            # interactive CLI chat
cargo run --features proxy -- proxy                  # proxy server on :3000
```

API keys: `~/.llmshim/config.toml` (via `llmshim configure`) or env vars `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `XAI_API_KEY`. Precedence: env vars > config file.

## Architecture

### Value-based transforms, no canonical struct

Requests flow as `serde_json::Value`. Each provider's transform takes raw JSON and maps only what it understands. Provider-specific features use `x-anthropic`, `x-gemini` namespaces.

### Request flow

```
llmshim::completion(router, request)
  → router.resolve("anthropic/claude-sonnet-4-6")   // parse "provider/model"
  → provider.transform_request(model, &value)        // OpenAI JSON → provider-native
  → client.send(provider_request)                    // HTTP
  → provider.transform_response(model, body)         // provider-native → OpenAI JSON
```

### Provider trait (`src/provider.rs`)

Every provider implements: `transform_request`, `transform_response`, `transform_stream_chunk`.

### Router (`src/router.rs`)

Parses `"provider/model"` strings. Auto-infers provider from prefix (`gpt*`/`o*` → openai, `claude*` → anthropic, `gemini*` → gemini, `grok*` → xai). Supports aliases. `Router::from_env()` reads API key env vars.

### HTTP Client (`src/client.rs`)

`ShimClient` with shared connection pool (`LazyLock`), HTTP/2, gzip/brotli/zstd compression, TCP keepalive + nodelay. Automatic retry (3 attempts by default) on transport errors and 429/500/502/503/504/529 status codes. This is the **reactive** layer: on a retryable *response* it honors the server's `Retry-After` header (integer seconds or HTTP-date) and provider reset hints (OpenAI `x-ratelimit-reset-*`, Anthropic `anthropic-ratelimit-*-reset`), clamped to a cap and nudged with a little jitter; when there's no server hint (or a transport error) it falls back to full-jitter exponential backoff (uniform in `[0, min(cap, base·2^attempt)]`) to avoid a thundering herd. Tunable via `LLMSHIM_MAX_RETRIES` and `LLMSHIM_MAX_BACKOFF_SECS`. `warmup()` pre-establishes TCP+TLS connections. `SseStream` buffers bytes, extracts `data:` lines, routes through provider's `transform_stream_chunk`.

### Fallback chains (`src/fallback.rs`)

`FallbackConfig` defines an ordered list of models to try. On retryable errors (429, 500, 502, 503, 529), retries with exponential backoff then falls through to the next model. `completion_with_fallback()` is the top-level API. The proxy supports this via `"fallback": ["model1", "model2"]` in the request body.

### Vision (`src/vision.rs`)

Image content blocks are translated between providers automatically. Users can send images in any format (OpenAI `image_url`, Anthropic `image`, Gemini `inline_data`) and the correct provider sees its native format. Base64 data URIs and plain URLs are both handled. Gemini falls back to a text placeholder for URL images (only supports `inline_data`).

### Multi-model conversations

Each provider sanitizes messages from other providers in `transform_request`. OpenAI's `annotations`/`refusal` stripped for Anthropic/Gemini. `reasoning_content` stripped for all. Tool calls normalized to OpenAI format in responses, translated back per-provider on input.

### Tool format translation

llmshim accepts tools in OpenAI Chat Completions format (nested `function` object) and translates them to each provider's native format:

- **OpenAI (Responses API):** Tool definitions flattened from `{"type": "function", "function": {"name": ..., "parameters": ...}}` to `{"type": "function", "name": ..., "parameters": ...}`. Assistant messages with `tool_calls` → `function_call` items. `role: "tool"` messages → `function_call_output` items. Streaming function call events (`response.output_item.added`, `response.function_call_arguments.delta`) translated to Chat Completions chunk format.
- **Anthropic:** Tools translated to `{"name": ..., "description": ..., "input_schema": ...}` format. Tool results translated to Anthropic's `tool_result` content blocks.
- **xAI:** Same flat format as OpenAI Responses API — `translate_tools()` flattens nested format.
- **Gemini:** Tools wrapped in `functionDeclarations`. Tool results translated to `functionResponse` format.

### CLI (`src/main.rs`)

Single binary with subcommands: `llmshim chat` (default), `llmshim proxy`, `llmshim configure`, `llmshim set/get/list`, `llmshim models`. Interactive chat with streaming, `/model` to switch, `/clear` to reset. Reasoning on by default (`reasoning_effort: "high"`). Thinking tokens shown in dim grey, answers in default color. Final summary shows timing and token counts (`↑` input, `↓` output). Optional JSONL file logging via `--log <path>` or `LLMSHIM_LOG` env var.

### Logging (`src/log.rs`)

JSONL structured logging. Each entry: timestamp, model, provider, latency_ms, input/output/reasoning token counts, status, request_id. Logged from API-reported usage (not local counting). CLI shows summary after each response; file logging is opt-in.

### Proxy server (`src/proxy/`, feature-gated behind `proxy`)

HTTP proxy with our own API spec (not OpenAI-compatible). Built on axum.

Endpoints:
- `POST /v1/chat` — non-streaming (or streaming if `stream: true`)
- `POST /v1/chat/stream` — always SSE streaming with typed events (`content`, `reasoning`, `tool_call`, `usage`, `done`, `error`)
- `GET /v1/models` — list available models (filtered to configured providers)
- `GET /health` — health check with provider list

Request format uses `config` for provider-agnostic settings and `provider_config` for raw passthrough. OpenAPI 3.1 spec at `api/openapi.yaml`.

Run: `llmshim proxy` (requires `--features proxy` at build time)
Config: `LLMSHIM_HOST` (default `0.0.0.0`), `LLMSHIM_PORT` (default `3000`)

#### Horizontal scaling / rate limiting (`src/proxy/ratelimit.rs`)

A **proactive** load-shedding layer sits in front of the reactive retry in `client.rs` so a fleet of proxy replicas (Cloud Run / ECS, ~10k concurrent requests across N instances) doesn't collectively blow provider TPM/RPM limits. Three pieces:

1. **`trait RateLimiter`** (`acquire` / `penalize`) — a pluggable token-bucket coordinator held as `Arc<dyn RateLimiter>` in `AppState`. `acquire` returns `Err(RetryAfter(Duration))` on exhaustion (never blocks); `penalize` backs a bucket off after an upstream 429. The pure token-bucket math (`TokenBucket`) is unit-tested with `tokio::time` paused.
2. **`InMemoryRateLimiter`** (default, zero infra) — per-provider RPM + optional TPM token buckets, refilling continuously. Governs a single instance.
3. **`RedisRateLimiter`** (opt-in, feature `redis-coordination`) — distributed token bucket via an atomic Redis Lua script (refill-by-timestamp + check-and-decrement), keyed per provider and shared across replicas for a true global limit. Enabled when `LLMSHIM_REDIS_URL` is set *and* the binary was built with `--features redis-coordination`; it connects lazily and **fails open** (admits + logs) if Redis is unreachable. If `LLMSHIM_REDIS_URL` is set but the feature is missing, it logs a warning and falls back to in-memory. The `redis` crate is gated so the default proxy binary stays lean.

Plus a per-instance **concurrency cap + bounded queue** (`Backpressure`, a `tokio::sync::Semaphore`): waiting for a permit is bounded, and on timeout the handler returns **503 + `Retry-After`** instead of growing memory unboundedly. A proactive rate-limit rejection returns **429 + `Retry-After`**. Both are mapped in `src/proxy/error.rs` (existing proxy responses are unchanged — this only adds the headers + new 429/503 backpressure responses).

Env config (all optional, safe defaults; when no RPM/TPM limits are set the limiter is a no-op but the concurrency cap still applies):

| Var | Default | Meaning |
| --- | --- | --- |
| `LLMSHIM_MAX_CONCURRENCY` | `256` | Max in-flight upstream requests per instance. |
| `LLMSHIM_QUEUE_TIMEOUT_MS` | `5000` | Max wait for a concurrency permit before 503. |
| `LLMSHIM_RATE_LIMIT_RPM` | unset | Global requests-per-minute limit (per provider). |
| `LLMSHIM_RATE_LIMIT_TPM` | unset | Global tokens-per-minute limit (estimated from content + `max_tokens`). |
| `LLMSHIM_<PROVIDER>_RPM` / `_TPM` | unset | Per-provider overrides, e.g. `LLMSHIM_OPENAI_RPM`, `LLMSHIM_ANTHROPIC_TPM`. Inherit the global for any unset field. |
| `LLMSHIM_REDIS_URL` | unset | Enable distributed coordination (needs `--features redis-coordination`). |
| `LLMSHIM_PENALTY_SECS` | `5` | Bucket backoff applied after an upstream 429. |

Topologies: **sidecar / zero-infra** (default in-memory limiter — set limits to `global / N` when running N replicas) vs. **Redis-coordinated fleet** (one shared global limit across all replicas).

## Detailed reference

Scoped rules in `.claude/rules/` load automatically when working in relevant files.

## Maintainer skills

Common maintenance workflows are packaged as [skills](https://code.claude.com/docs/en/skills) in `.claude/skills/` (see `.claude/skills/README.md`):

- `/add-model provider/id "Label"` — register a new model on an existing provider.
- `/add-provider key Name` — wire up a brand-new upstream provider.
- `/preflight` — run the fmt + clippy + test trio CI enforces.
- `/release 0.1.22` — bump version and tag so CI publishes.

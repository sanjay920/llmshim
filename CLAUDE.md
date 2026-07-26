# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is llmshim

A pure Rust LLM API translation layer. Takes OpenAI-format JSON requests, translates them to provider-native formats (and back), with zero infrastructure requirements. Supports OpenAI (Responses API), Anthropic, Google Gemini, xAI, OpenRouter (an OpenAI Chat Completions-compatible aggregator), and self-hosted **vLLM** / **SGLang** servers (OpenAI Chat Completions-compatible, local or remote). Includes an interactive CLI chat with streaming, reasoning, and mid-conversation model switching.

**Published on crates.io as `llmshim`** — https://crates.io/crates/llmshim

This is a public crate on crates.io. Do NOT make breaking changes to `pub` items in `src/lib.rs`, `src/router.rs`, `src/provider.rs`, `src/error.rs`, `src/fallback.rs`, `src/log.rs`, `src/config.rs`, `src/models.rs`, or `src/vision.rs` without a semver bump.

## Supported models

- **OpenAI:** `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.5`, `gpt-5.5-pro`, `gpt-5.4`, `gpt-5.4-pro`, `gpt-5.4-mini`, `gpt-5.4-nano`
- **Anthropic:** `claude-opus-5`, `claude-opus-4-8`, `claude-sonnet-5`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001`
- **Gemini:** `gemini-3.5-flash`, `gemini-3.1-pro-preview`, `gemini-3-flash-preview`
- **xAI:** `grok-4.5`, `grok-4.3`, `grok-4.20-multi-agent-beta-0309`, `grok-4.20-beta-0309-reasoning`, `grok-4.20-beta-0309-non-reasoning`
- **OpenRouter:** not enumerated (huge/dynamic catalog) — any `openrouter/<vendor>/<model>` slug routes through, e.g. `openrouter/anthropic/claude-sonnet-4.5`.
- **vLLM / SGLang:** not enumerated (self-hosted) — any `vllm/<served-model>` or `sglang/<served-model>` routes through to the configured server, e.g. `sglang/Qwen/Qwen3.6-35B-A3B-FP8`.

## Build & Test

```bash
cargo build                                          # dev build
cargo build --release                                # release build (~6MB binary)
cargo test --tests                                   # unit tests
cargo test -- --ignored                              # integration tests (needs API keys)
cargo test --features proxy --tests                  # unit tests incl. proxy (~420; what CI runs)
cargo test --features proxy -- --ignored             # all integration tests including proxy
cargo run                                            # interactive CLI chat
cargo run --features proxy -- proxy                  # proxy server on :3000
```

API keys: `~/.llmshim/config.toml` (via `llmshim configure`) or env vars `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `XAI_API_KEY`, `OPENROUTER_API_KEY`. Precedence: env vars > config file. Self-hosted servers are configured by **base URL** instead of a key: `VLLM_BASE_URL` / `SGLANG_BASE_URL` (each with an optional `VLLM_API_KEY` / `SGLANG_API_KEY`); the provider registers only when its base URL is set. Local vs remote is just the URL value.

## Architecture

### Value-based transforms, no canonical struct

Requests flow as `serde_json::Value`. Each provider's transform takes raw JSON and maps only what it understands. Provider-specific features use `x-anthropic`, `x-gemini`, `x-openrouter`, `x-vllm`, `x-sglang` namespaces.

**Self-hosted passthrough providers (vLLM / SGLang).** `src/providers/openai_compat.rs` is one generic OpenAI Chat Completions passthrough backing both `vllm` and `sglang` (`OpenAiCompatible::new(name, base_url, api_key: Option)`, registered per env base URL). Two things differ from the hosted providers: the **base URL is configuration** (local `http://localhost:8000/v1` vs remote `https://host/v1`), and **auth is optional** (self-hosted servers are unauthenticated unless launched with `--api-key`, so the `Authorization` header is sent only when a key is set). Passthrough transforms; `reasoning`/`reasoning_content` normalized to `reasoning_content` (vLLM is migrating the field name); `reasoning_effort` forwarded as-is (honored per-model, not clamped); server-specific params go under `x-<name>` (`chat_template_kwargs`, `separate_reasoning`, `guided_json`, `top_k`, …). Note: reasoning/tool parsing are **launch-time server flags** (`--reasoning-parser`, `--tool-call-parser`), so a request only gets that behavior if the server was started for it — llmshim can't enable it per request.

**OpenRouter is the one passthrough provider.** Every other provider translates the OpenAI-format input *away* to a native dialect; OpenRouter (`src/providers/openrouter.rs`) *is* OpenAI Chat Completions, so its transforms are near-identity — messages, tools, vision (`image_url`), and `response_format` are forwarded unchanged; `reasoning_effort` maps 1:1 to OpenRouter's `reasoning:{effort}` (its effort vocabulary is a superset, so no clamping); `message.reasoning` is normalized to `reasoning_content` on responses. OpenRouter models are **not enumerated** in `src/models.rs` (the catalog is huge and dynamic) — any `openrouter/<vendor>/<model>` slug routes through. `x-openrouter` carries OpenRouter-only controls (`provider`, `models`, `transforms`, `route`, native `reasoning`; plus `http_referer`/`x_title` which become headers). The `middle-out` transform is disabled by default for faithful passthrough. Uses `image_url` (Chat Completions) vision via `vision::to_openai_chat`.

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

Parses `"provider/model"` strings by splitting on the **first** `/` only, so an OpenRouter slug's internal slash survives (`openrouter/anthropic/claude-sonnet-4.5` → provider `openrouter`, model `anthropic/claude-sonnet-4.5`). Auto-infers provider from prefix (`gpt*`/`o*` → openai, `claude*` → anthropic, `gemini*` → gemini, `grok*` → xai); **OpenRouter, vLLM, and SGLang have no prefix inference** — their slugs collide with everyone's, so address them explicitly (`openrouter/…`, `vllm/…`, `sglang/…`); the first-slash split also preserves HF-style served-model slugs (`vllm/meta-llama/Llama-3.1-8B-Instruct`). Supports aliases. `Router::from_env()` reads API-key env vars, plus `VLLM_BASE_URL` / `SGLANG_BASE_URL` (+ optional `*_API_KEY`) for the self-hosted providers.

### HTTP Client (`src/client.rs`)

`ShimClient` with shared connection pool (`LazyLock`), HTTP/2, gzip/brotli/zstd compression, TCP keepalive + nodelay. Automatic retry (3 attempts by default) on transport errors and 429/500/502/503/504/529 status codes. This is the **reactive** layer: on a retryable *response* it honors the server's `Retry-After` header (integer seconds or HTTP-date) and provider reset hints (OpenAI `x-ratelimit-reset-*`, Anthropic `anthropic-ratelimit-*-reset`), clamped to a cap and nudged with a little jitter; when there's no server hint (or a transport error) it falls back to full-jitter exponential backoff (uniform in `[0, min(cap, base·2^attempt)]`) to avoid a thundering herd. Tunable via `LLMSHIM_MAX_RETRIES` and `LLMSHIM_MAX_BACKOFF_SECS`. `warmup()` pre-establishes TCP+TLS connections. `SseStream` buffers bytes, extracts `data:` lines, routes through provider's `transform_stream_chunk`.

### Fallback chains (`src/fallback.rs`)

`FallbackConfig` defines an ordered list of models to try. On retryable errors (429, 500, 502, 503, 529), retries with exponential backoff then falls through to the next model. `completion_with_fallback()` is the top-level API. The proxy supports this via `"fallback": ["model1", "model2"]` in the request body.

### Vision (`src/vision.rs`)

Image content blocks are translated between providers automatically. Users can send images in any format (OpenAI `image_url`, Anthropic `image`, Gemini `inline_data`) and the correct provider sees its native format. Base64 data URIs and plain URLs are both handled. Gemini falls back to a text placeholder for URL images (only supports `inline_data`).

### Multi-model conversations

Each provider sanitizes messages from other providers in `transform_request`. OpenAI's `annotations`/`refusal` stripped for Anthropic/Gemini. `reasoning_content` is stripped by other providers, but **Anthropic reconstructs a native `thinking` block** from `reasoning_content` + `reasoning_signature` (and `redacted_thinking` from `redacted_reasoning_content`) as the first block of the assistant turn, so extended-thinking + tool-use round-trips losslessly (surfaced on responses incl. streaming; opaque signatures are stripped by other providers so they never leak cross-provider; no signature → still stripped). Symmetric to the tool-call `thought_signature` round-trip. Tool calls normalized to OpenAI format in responses, translated back per-provider on input.

### Provider extension namespaces (`x-anthropic`, `x-gemini`)

Callers pass provider-specific controls under these keys. Each provider copies what it understands into the native request but **excludes control-only keys from the upstream body**. Anthropic supports:

- `x-anthropic.disable_1m_context` (bool) — opt out of the 1M-context beta header (on by default for supported models).
- `x-anthropic.extra_betas` (string array) — extra `anthropic-beta` tokens appended to the auto-managed set (1M-context / fast-mode / cache-TTL), de-duplicated. It's a header control, not a body param (e.g. lets a caller forward Claude Code's `--betas`). Logic + tests: `src/providers/anthropic.rs`, `tests/unit_anthropic.rs`.

### Unified reasoning controls

Two knobs work across every provider: `reasoning_effort` (`none|low|medium|high|xhigh|max`) and `reasoning_mode` (`standard|pro`). A third, `reasoning_summary` (`auto|none`), controls reasoning-text visibility → Anthropic `thinking.display` (`auto`→`summarized`, the default when `reasoning_effort` is present so newer models like Sonnet 5 / Opus 4.7-4.8 return reasoning text instead of the API-default `omitted`; `none`→`omitted` for lower latency). Applies to both the adaptive and pre-4.6 enabled thinking builders; a caller-supplied `thinking` block bypasses it. Each provider transform maps them to its native dialect, **clamping to the nearest tier the target model accepts** (all boundaries verified live — e.g. `max` is native only on OpenAI gpt-5.6; Anthropic 4.6 rejects `xhigh` but has `max`; Gemini's enum tops out at `high`; xAI grok-4.20 models reject any reasoning param). `mode: "pro"` is native on OpenAI gpt-5.6/-pro models (`reasoning.mode`), emulated as a one-tier effort bump elsewhere; explicit `none` always wins. Native passthrough (`x-openai.reasoning`, `x-anthropic.thinking`, `x-gemini.thinkingConfig`) bypasses the mapping entirely and always takes precedence. **Full per-provider mapping tables: `docs/src/guides/reasoning.md`** — update it and the pinning tests in `tests/unit_*.rs` together whenever a mapping changes.

### Tool format translation

llmshim accepts tools in OpenAI Chat Completions format (nested `function` object) and translates them to each provider's native format:

- **OpenAI (Responses API):** Tool definitions flattened from `{"type": "function", "function": {"name": ..., "parameters": ...}}` to `{"type": "function", "name": ..., "parameters": ...}`. Assistant messages with `tool_calls` → `function_call` items. `role: "tool"` messages → `function_call_output` items. Streaming function call events (`response.output_item.added`, `response.function_call_arguments.delta`) translated to Chat Completions chunk format.
- **Anthropic:** Tools translated to `{"name": ..., "description": ..., "input_schema": ...}` format. Tool results translated to Anthropic's `tool_result` content blocks.
- **xAI:** Same flat format as OpenAI Responses API — `translate_tools()` flattens nested format.
- **OpenRouter:** No translation — it accepts the Chat Completions nested `{"type":"function","function":{…}}` format directly, so `tools`/`tool_choice`/`tool_calls` pass through unchanged.
- **vLLM / SGLang:** Same as OpenRouter — Chat Completions nested tool format passes through unchanged (the server must be launched with `--tool-call-parser` / `--enable-auto-tool-choice` for tool calls to be parsed).
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

## Client libraries (`clients/`)

Thin clients that speak the proxy's HTTP API. They are faithful to the OpenAPI contract in `api/openapi.yaml` — when you change the proxy's request/response shapes, update that spec and keep the clients in sync. All publish in lockstep with the crate version on every release.

- **Python** (`clients/python`, PyPI `llmshim`): built with maturin (`bindings = "bin"`, `manifest-path = "../../Cargo.toml"`), so wheels **bundle the Rust binary** and `_server.py` auto-spawns `llmshim proxy` on first call. Version derives from `Cargo.toml` — no separate bump.
- **TypeScript/JS** (`clients/typescript`, npm `llmshim`): dependency-free, and **also bundles the binary + auto-spawns** it (unless you pass an explicit `baseUrl`). The binary ships via `optionalDependencies` on five per-platform packages in `clients/typescript/packages/` — npm installs only the one matching `os`/`cpu`. `src/server.ts` maps `process.platform`-`process.arch` → package name; the Windows package is **scoped under the maintainer's npm namespace** to sidestep npm's spam filter on unscoped `*-win32-*` names.
- **Go** (`clients/go`): stdlib-only, pure HTTP (no bundled binary). `go get` resolves the `clients/go/vX.Y.Z` tag that CI pushes each release (Go needs no registry).
- **Ruby** (`clients/ruby`, RubyGems `llmshim`): stdlib-only, pure HTTP. Tests use `webrick`, which is not a default gem on Ruby ≥ 3.0.

## Releasing (tag-driven, multi-registry)

Pushing a `vX.Y.Z` tag runs `.github/workflows/release.yml`: an fmt+clippy+test gate, then publishes to **crates.io, PyPI, npm (root + 5 platform packages), RubyGems, Homebrew, and a Go module tag**. All registry auth is **OIDC trusted publishing** (no stored tokens) except the crates.io token. The `/release` skill has the exact checklist. Rules learned the hard way — don't regress these:

- **Version lockstep**: bump `Cargo.toml` + `Cargo.lock`, `clients/typescript/package.json` (and its five `optionalDependencies`), every `clients/typescript/packages/*/package.json`, and `clients/ruby/lib/llmshim/version.rb` together. Python derives from `Cargo.toml`. The npm/rubygems jobs fail the release if a version file drifts from the tag.
- **Idempotent pipeline**: every publish step (crates/PyPI/npm/GitHub release/Go tag) skips if that version already exists, so the release is safe to re-run or re-tag when one registry hiccups.
- **First publish of a new npm package** is manual once (npm's Trusted Publisher UI requires the package to exist first); PyPI/RubyGems support pre-configured "pending" publishers. Each package needs a Trusted Publisher pointing at workflow `release.yml`, environment `release`.
- **CI environment quirks handled in the workflow**: OIDC npm publish needs npm ≥ 11.5.1 (upgrade npm in-job — the bundled one is too old); `mkdir -p bin` before copying binaries into platform packages (git doesn't track empty dirs); install `webrick` explicitly for the Ruby test; the root npm job uses `npm install --omit=optional` (not `npm ci`) because the platform packages publish in the same run.

## Detailed reference

Scoped rules in `.claude/rules/` load automatically when working in relevant files.

## Maintainer skills

Common maintenance workflows are packaged as [skills](https://code.claude.com/docs/en/skills) in `.claude/skills/` (see `.claude/skills/README.md`):

- `/add-model provider/id "Label"` — register a new model on an existing provider.
- `/add-provider key Name` — wire up a brand-new upstream provider.
- `/preflight` — run the fmt + clippy + test trio CI enforces.
- `/release 0.1.22` — bump version and tag so CI publishes.

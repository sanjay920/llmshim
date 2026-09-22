# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is llmshim

A pure Rust LLM API translation layer. Takes OpenAI-format JSON requests, translates them to provider-native formats (and back), with zero infrastructure requirements. Supports OpenAI (Responses API), Anthropic, Google Gemini, xAI, OpenRouter (an OpenAI Chat Completions-compatible aggregator), and self-hosted **vLLM** / **SGLang** servers (OpenAI Chat Completions-compatible, local or remote). Includes an interactive CLI chat with streaming, reasoning, and mid-conversation model switching.

**Published on crates.io as `llmshim`** — https://crates.io/crates/llmshim

This is a public crate on crates.io. Do NOT make breaking changes to `pub` items in `src/lib.rs`, `src/router.rs`, `src/provider.rs`, `src/error.rs`, `src/fallback.rs`, `src/log.rs`, `src/config.rs`, `src/models.rs`, or `src/vision.rs` without a semver bump.

## Advertised models

- **OpenAI:** `gpt-6-astra`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`
- **ChatGPT subscription (OAuth):** only `chatgpt/gpt-6-astra`, `chatgpt/gpt-5.6-sol`, `chatgpt/gpt-5.6-terra`, and `chatgpt/gpt-5.6-luna`. `CHATGPT_MODELS` in `src/models.rs` is shared by discovery, CLI selection, and validation; older/unlisted models fail before authentication or network calls.
- **Anthropic:** `claude-fable-5-1`, `claude-opus-5`, `claude-sonnet-5`, `claude-haiku-4-5-20251001`
- **Gemini:** `gemini-3.8-flash`, `gemini-3.5-flash-lite`
- **xAI:** `grok-4.7`
- **OpenRouter:** not enumerated (huge/dynamic catalog) — any `openrouter/<vendor>/<model>` slug routes through, e.g. `openrouter/anthropic/claude-sonnet-5`.
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

### Catalog and usage normalization (local 0.4 development)

`llmshim-catalog` is a standalone workspace member. Its `builtin` module owns
the curated/historical constants; `src/models.rs` reexports the legacy borrowed
API. Owned metadata and snapshots live in `llmshim::catalog`. Keep the curated
15-route discovery and four-entry ChatGPT allowlist distinct from catalog
coverage. Local policy > provider capabilities > verified builtin assertions >
models.dev, per field; unknowns never erase assertions. Provider APIs never
contribute pricing. See `crates/llmshim-catalog/README.md` for cache and override
paths, offline behavior, aliases, and publication order. Never await catalog
refresh in a completion; hold a snapshot for decisions that must agree.

`usage.cache_read_tokens` and `usage.cache_write_tokens` are always present on
normalized responses and usage chunks, logs, and proxy usage. Native token
fields remain readable. `src/usage.rs` owns extraction; the streaming client
merges Anthropic's start/delta usage before normalizing terminal counts.
`usage.uncached_input_tokens` joins them: the providers disagree on whether
their prompt total already includes the cache read (Anthropic's excludes it,
OpenAI/Chat Completions/Gemini include it), and only the transport boundary
still knows which convention the body used. The convention is read off the
cache-read field that actually matched, never a provider-name table.

### USD cost accounting

`src/cost.rs` is the only thing that multiplies a catalog `Cost` (USD per
million tokens) by those counters. Input is charged on `uncached_input_tokens`
so a cached prompt is never billed twice; cache reads and writes are charged at
their own rates; reasoning tokens are already inside `completion_tokens`.
**An absent price is `None`, never `0.0`** — a positive count in a bucket with
no rate poisons the whole total rather than producing a partial sum that reads
as a complete one. `client.rs` stamps `usage.cost_usd` at the transport
boundary (the last place that knows the dispatch target) for completions and
for whichever stream chunk carries usage; `log.rs`, `proxy::types::Usage`, both
native facades and the four bundled clients carry it through as a nullable
field. `null` means unknown, not free.

**A provider-reported bill outranks the catalog.** Where a response carries
`usage.cost` — OpenRouter returns it on every call, with no parameter to set —
`stamp` uses that number and records `usage.cost_source: "provider"`;
otherwise it prices from the catalog and records `"catalog"`. Measured
2026-09-22 on `deepseek/deepseek-v4.1-flash`: the catalog's rate for the
OpenRouter slug was half what OpenRouter billed, so the estimate this replaces
was under-reporting 2:1. A catalog cannot detect that from the inside. The
catalog product is an estimate of that bill and deliberately an upper bound, so
it must never overwrite one. `cost_source` rides beside `cost_usd` everywhere
it goes (`LogEntry`, `proxy::types::Usage`, OpenAPI, the typed clients).
`shim::add_usage` sums a reported `cost` across a repair's two attempts, because
two dispatches are two charges.

`ModelInfo.context_cost_tiers` adds context-dependent standard rates without
changing the public `Cost` struct. `cost_for_input_tokens` includes cached input
in tier selection; `src/cost.rs` applies the resulting rates to the whole
response. Local per-field prices outrank lower-source tier rates. Builtin
launch metadata lives in `crates/llmshim-catalog/data/verified.json`, separate
from the unmodified models.dev snapshot. Grok 4.7 live regression tests are in
`tests/integration_grok_4_7.rs`; they use an ephemeral proxy port, configured
keys, and billed API calls. `none` clamps to `low`; named tool choice must be
flat on the Responses wire. Preserve encrypted reasoning on both normal and
streaming tool round trips.

`src/gateway/quota.rs` adds a per-identity dollar cap beside the RPM/TPM
buckets: `budget_usd` + `budget_window_secs` on an `Identity`, checked before
dispatch and charged after (cost is only knowable once a response exists, so
one in-flight request can overshoot). Windows tumble rather than slide, because
the fleet-wide store is one counter per window. `SpendCap::with_store` takes the
Redis-backed `DistributedGateway` in distributed mode so `$100/day` means one
hundred dollars fleet-wide, not per replica. **A response the catalog cannot
price is not charged** — recording zero would let an unpriced model run forever
under a budget; `cost_usd: null` is the signal that a price is missing.
The native Chat Completions streams must use their own parser in the client;
passing them to the Responses parser silently drops all events.

Offline checks for this work:

```sh
LLMSHIM_CATALOG_OFFLINE=1 cargo test --workspace --features proxy --tests
cargo test -p llmshim-catalog
cargo clippy --workspace --features proxy -- -D warnings
cargo package -p llmshim-catalog --allow-dirty
```

The root package is 0.4.0 because log/proxy usage structs gain fields; no
release has been performed. Future release workflows must publish the catalog
dependency before llmshim. Public code, fixtures, artifacts, and docs must use
generic examples and contain no private consumer identities or context.

### Cache annotations and shared schema normalization

`src/cache.rs` translates caller `x-cache` segments into native Anthropic
breakpoints and an explicit Responses prompt_cache_key. It never infers
stability. Last eligible boundaries win within the four-slot budget; existing
explicit markers consume slots. Managed segments supersede automatic caching.
One-hour markers must precede five-minute markers. Marker scans inspect actual
cache locations, not arbitrary schema/default JSON. Without annotations, native
passthrough remains unchanged. `ProviderRequest::can_continue_from` checks
endpoint, headers, settings (including include/store/reasoning) and input prefix;
there is no stored/delta continuation engine. See the caching guide.

`src/schema/` owns the single schema walker and local resource resolver. Every
adapter normalizes native tool schemas after overrides. MCP inputSchema is
normalized on ingest and raw MCP tool definitions are accepted. Keep literal
values/property names separate from schema-node traversal, preserve meaningful
stripped constraints in descriptions, and never fetch an external reference.
Cycles, unresolved resources, incompatible residues and expansion-budget failures
fall back per tool. Only successful enforcement permits strict:true. Global
bypass flags: LLMSHIM_NO_SCHEMA_NORMALIZATION and LLMSHIM_NO_STRICT. OutputSchema
provides a reversible non-object wrapper; generated-instance validation and
repair remain a separate concern. See `docs/src/guides/schemas.md`.

### Curated discovery

`src/models.rs::MODELS` is the single advertised list, imported directly by
`src/main.rs` and used by `available_models()` for proxy discovery. Keep the
current model in each retained tier: four OpenAI, four Anthropic, two stable
Gemini, one xAI, and four ChatGPT routes. Do not add previews or bring back
superseded generations without an explicit catalog decision.

Historical metadata lives in private `LEGACY_MODELS` and remains queryable via
`spec()`. Pruning discovery does not delete legacy transforms, tests, or
explicit-ID routing. ChatGPT keeps its separately authorized four-ID request
allowlist. Keep reader-facing model tables and examples current; retain the
actual model IDs in historical benchmark results and regression fixtures.

### Value-based transforms, no canonical struct

Requests flow as `serde_json::Value`. Each provider's transform takes raw JSON and maps only what it understands. Provider-specific features use `x-anthropic`, `x-gemini`, `x-openrouter`, `x-vllm`, `x-sglang` namespaces.

**ChatGPT OAuth (`src/providers/chatgpt/`).** Run `llmshim login chatgpt` for
device-code authentication, `login chatgpt --status` for a local check, and
`logout chatgpt` to remove the selected cache. The independent default cache
is `~/.llmshim/chatgpt/auth.json`; `CHATGPT_TOKEN_DIR`/`CHATGPT_AUTH_FILE` can
override it. Never read or overwrite Codex credentials implicitly. The
object-safe `Provider::prepare_request` hook defaults to `transform_request`;
ChatGPT uses it to refresh asynchronously with cross-process file locking and
atomic owner-only token writes. Requests never initiate interactive login.

The subscription backend requires SSE, `stream: true`, and `store: false`.
ChatGPT reuses the Responses translator, enforces the backend field allowlist
after `x-chatgpt` overrides, and aggregates a validated terminal event for
non-streaming callers. EOF or `[DONE]` without a terminal event is an error.
Preserve `chatgpt/<model>` in normalized responses and chunks: a bare GPT name
is otherwise misattributed to API-key OpenAI by the proxy/gateway when both
providers are registered. Astra preserves reasoning effort `max`; `none` and
`minimal` clamp to `low`.
For ChatGPT streaming, use the same `StreamNormalizer`/`ToolStream` as other
transports. Do not reintroduce a separate completed-call emitter: it used to
lose or duplicate argument fragments at the proxy boundary. Text and reasoning
remain incremental; callable tools are complete and emitted once at termination.


Offline coverage lives in `tests/unit_chatgpt.rs`. The live server check starts
its own loopback CLI process and stops it on completion/failure:

```bash
cargo test --features proxy --test integration_chatgpt_proxy -- --ignored --nocapture
```

It checks all four models through `/v1/chat` and `/v1/chat/stream`, provider
identity with API-key OpenAI also registered, model discovery, old-model
rejection, Astra tool calls (normal and streaming), a tool-result round trip,
and image input. It uses the saved ChatGPT login and consumes
subscription usage; it is ignored during offline CI. Mount the whole token
directory writable for container use so refresh locks and atomic saves work.

**Self-hosted passthrough providers (vLLM / SGLang).** `src/providers/openai_compat.rs` is one generic OpenAI Chat Completions passthrough backing both `vllm` and `sglang` (`OpenAiCompatible::new(name, base_url, api_key: Option)`, registered per env base URL). Two things differ from the hosted providers: the **base URL is configuration** (local `http://localhost:8000/v1` vs remote `https://host/v1`), and **auth is optional** (self-hosted servers are unauthenticated unless launched with `--api-key`, so the `Authorization` header is sent only when a key is set). Passthrough transforms; reasoning normalized to typed `reasoning[]` with provenance (vLLM is migrating the field name); `reasoning_effort` forwarded as-is (honored per-model, not clamped); server-specific params go under `x-<name>` (`chat_template_kwargs`, `separate_reasoning`, `guided_json`, `top_k`, …). Note: reasoning/tool parsing are **launch-time server flags** (`--reasoning-parser`, `--tool-call-parser`), so a request only gets that behavior if the server was started for it — llmshim can't enable it per request.

**OpenRouter is the one passthrough provider.** Every other provider translates the OpenAI-format input *away* to a native dialect; OpenRouter (`src/providers/openrouter.rs`) *is* OpenAI Chat Completions, so its transforms are near-identity — messages, tools, vision (`image_url`), and `response_format` are forwarded unchanged; `reasoning_effort` maps 1:1 to OpenRouter's `reasoning:{effort}` (its effort vocabulary is a superset, so no clamping); reasoning is normalized to typed `reasoning[]` with provenance on responses. OpenRouter models are **not enumerated** in `src/models.rs` (the catalog is huge and dynamic) — any `openrouter/<vendor>/<model>` slug routes through. `x-openrouter` carries OpenRouter-only controls (`provider`, `models`, `transforms`, `route`, native `reasoning`; plus `http_referer`/`x_title` which become headers). The `middle-out` transform is disabled by default for faithful passthrough. Uses `image_url` (Chat Completions) vision via `vision::to_openai_chat`.

### Request flow

```
llmshim::completion(router, request)
  → router.resolve("anthropic/claude-sonnet-5")   // parse "provider/model"
  → provider.prepare_request(model, &value).await    // refresh OAuth if needed, then transform
  → client.send(provider_request)                    // HTTP
  → provider.transform_response(model, body)         // provider-native → OpenAI JSON
```

### Provider trait (`src/provider.rs`)

Every provider implements: `transform_request`, `transform_response`, `transform_stream_chunk`.

Non-streaming OpenAI/xAI Responses, Gemini and Anthropic transformations require
supported terminal metadata. Do not turn absent or unrecognized status into
`finish_reason: "stop"`. Rejected metadata returns a fixed 502 provider error
without copying provider-supplied status/content into the diagnostic. Tests
live in `tests/support/completion_status.rs`, included by `unit_openai`.
Streaming status handling is separate and is not changed by this policy.

### Router (`src/router.rs`)

Parses `"provider/model"` strings by splitting on the **first** `/` only, so an OpenRouter slug's internal slash survives (`openrouter/anthropic/claude-sonnet-5` → provider `openrouter`, model `anthropic/claude-sonnet-5`). Auto-infers provider from prefix (`gpt*`/`o*` → openai, `claude*` → anthropic, `gemini*` → gemini, `grok*` → xai); **OpenRouter, vLLM, and SGLang have no prefix inference** — their slugs collide with everyone's, so address them explicitly (`openrouter/…`, `vllm/…`, `sglang/…`); the first-slash split also preserves HF-style served-model slugs (`vllm/meta-llama/Llama-3.1-8B-Instruct`). Supports aliases. `Router::from_env()` reads API-key env vars, plus `VLLM_BASE_URL` / `SGLANG_BASE_URL` (+ optional `*_API_KEY`) for the self-hosted providers.

### HTTP Client (`src/client.rs`)

Automatic redirects are disabled on the shared client. Keep prompts and
provider-specific credential headers at the configured endpoint; 3xx responses
remain provider errors. Callers must configure the final URL directly.

`ShimClient` with shared connection pool (`LazyLock`), HTTP/2, gzip/brotli/zstd compression, TCP keepalive + nodelay. Automatic retry (3 attempts by default) on transport errors and 429/500/502/503/504/529 status codes. This is the **reactive** layer: on a retryable *response* it honors the server's `Retry-After` header (integer seconds or HTTP-date) and provider reset hints (OpenAI `x-ratelimit-reset-*`, Anthropic `anthropic-ratelimit-*-reset`), clamped to a cap and nudged with a little jitter; when there's no server hint (or a transport error) it falls back to full-jitter exponential backoff (uniform in `[0, min(cap, base·2^attempt)]`) to avoid a thundering herd. Tunable via `LLMSHIM_MAX_RETRIES` and `LLMSHIM_MAX_BACKOFF_SECS`. `warmup()` pre-establishes TCP+TLS connections. `SseStream` decodes bytes with eventsource-stream and feeds one per-response `StreamNormalizer`; do not restore lossy UTF-8 line buffering.

### Fallback chains (`src/fallback.rs`)

`FallbackConfig` defines an ordered list of models to try. On retryable errors (429, 500, 502, 503, 529), retries with exponential backoff then falls through to the next model. `completion_with_fallback()` is the top-level API. The proxy supports this via `"fallback": ["model1", "model2"]` in the request body.

### Provider health (`src/breaker.rs`, `src/proxy/health.rs`)

**Health is not rate-limit backoff.** The token buckets already slow a provider
down after a 429 — a 429 means the provider is alive and asking for less. The
breaker counts what retrying cannot fix: 5xx (500/502/503/504/529) and
transport failures. Adapted from `rcode-provider`'s `ProviderBreaker`, which we
own: sliding failure window, open state, and a single half-open probe admitted
after the cooldown. Config: `LLMSHIM_BREAKER_WINDOW_SECS` (60),
`LLMSHIM_BREAKER_TRIP_THRESHOLD` (3; `0` disables), `LLMSHIM_BREAKER_COOLDOWN_SECS` (30).

The breaker hangs on the `Router` (`Router::breaker()` / `with_breaker`), but
the *counting* happens in `ShimClient`: `ShimClient::with_breaker` attaches one,
and `completion` / `stream` / `stream_owned` observe their final result exactly
once. The top-level entry points bind the router's breaker to the shared client
per call (`lib.rs::bound_client`), so `llmshim::completion`, `stream`,
`completion_with_fallback` and a caller that resolves its own provider and dials
`ShimClient` directly all feed the same breaker — the last one only if it opted
in with `ShimClient::new().with_breaker(router.breaker().clone())`; a bare
`ShimClient::new()` reports to nobody. Do not add a second `.observe` around a
client call: one call, one observation (`tests/unit_client_breaker.rs`). Only
`fallback.rs` *refuses*, and it checks before every attempt rather than once per
chain entry — the attempt that opens a circuit is usually the chain's own, so a
per-entry check would still retry into a target it just watched die. A single-target call is still dispatched:
with no alternative, refusing would only convert an upstream failure into a
local one. `proxy::health::build_breaker()` attaches a Redis-coordinated
`SharedHealth` (failure ZSET + open marker + `SET NX` probe) when
`LLMSHIM_REDIS_URL` is set and `redis-coordination` is compiled in, mirroring
`build_limiter`. Every shared operation **fails open**.

### Named routes (`src/config.rs`, `src/router.rs`)

A caller-defined name maps to a model plus request settings:

```toml
[routes.compaction]
model = "anthropic/claude-haiku-4-5-20251001"
reasoning_effort = "low"
max_tokens = 4096
```

Addressed as `"model": "route/compaction"`, so a route travels through the
existing `provider/model` grammar — an OpenAI SDK, the CLI and the proxy's
admission control all handle it without learning a new field. `resolve_key`
resolves the indirection, so rate limiting never sees an unrecognized string.

**llmshim must not learn harness vocabulary.** The name is opaque: a harness may
call a route `compaction`, `advisor` or `webSearch`, and llmshim only knows it
maps to a model. Route settings are defaults — a per-request key always wins —
and an unknown name is a 400, never a silent fall back to the default model.
Routes do not chain.

### Vision (`src/vision.rs`)

Image content blocks are translated between providers automatically. Users can send images in any format (OpenAI `image_url`, Anthropic `image`, Gemini `inline_data`) and the correct provider sees its native format. Base64 data URIs and plain URLs are both handled. Gemini falls back to a text placeholder for URL images (only supports `inline_data`).

### Multi-model conversations

`src/reasoning.rs` owns the single replay policy, typed blocks, signature origins,
issuer bindings, and drop counters. Every adapter filters before serialization
and captures from the original response to preserve ordered blocks and encrypted
Responses items. The shared HTTP client binds origins to the actual request
(including refreshed OAuth account headers). Unknown provenance/families fail
closed; matching family+wire permits replay, with same-account binding also
required for encrypted blocks. Never route by inspecting opaque bytes.

New outputs use `message.reasoning[]` and `thought_signature:{data,origin}`.
Legacy sibling readers exist for one migration release and drop untracked data.
Native overrides cannot bypass this filter or enable provider-side Responses
storage. The CLI and proxy retain the whole message; streaming consumers use
`ReasoningAccumulator` and must preserve signatures and completed item snapshots.
The shared SSE reader handles split UTF-8, CRLF, multiline events, late usage,
and terminal markers; do not restore the old per-byte lossy string buffer.
Tests: `tests/unit_reasoning.rs`, the client SSE tests, and provider regressions.
See `docs/src/guides/reasoning.md` for the full shape and migration contract.

### Provider extension namespaces (`x-anthropic`, `x-gemini`)

Callers pass provider-specific controls under these keys. Each provider copies what it understands into the native request but **excludes control-only keys from the upstream body**. Anthropic supports:

- `x-anthropic.disable_1m_context` (bool) — opt out of the 1M-context beta header (on by default for supported models).
- `x-anthropic.extra_betas` (string array) — extra `anthropic-beta` tokens appended to the auto-managed set (1M-context / fast-mode / cache-TTL), de-duplicated. It's a header control, not a body param (e.g. lets a caller forward Claude Code's `--betas`). Logic + tests: `src/providers/anthropic.rs`, `tests/unit_anthropic.rs`.

### Unified reasoning controls

**Fable compatibility (verified September 2026).** Advertise Fable 5.1;
retain Fable 5 behavior and metadata for explicit requests. Both use always-on adaptive thinking; unified `none` clamps to `low`,
and native disabled/manual thinking fails locally. Strip `temperature`,
`top_p`, and `top_k` for Fable and Opus 5 even without an explicit thinking
object. Fable 5.1 rejects forced tool selection (`required` / `any` / `tool`);
do not silently turn it into `auto`. Fable 5 still accepts forced tools.
Both versions reject assistant prefill. Refusal stop reasons on Fable/Opus 5
map to `content_filter` in normal and streaming engine responses.

Fable 5.1's thinking signatures are conversation-bound. Preserve appended
system turns instead of hoisting them into the initial prompt. Keep the
initial system/tools/message prefix stable during a signed-thinking round
trip; errors must not become success. Test with the
`thinking-binding-controls-2026-08-01` beta and
`thinking.block_binding.prefix_mismatch_behavior: "error"` so older API
accounts exercise enforcement too. The provider handles incompatible
thinking on a switch to older models; do not infer signatures from text.

`tests/unit_fable.rs` pins these rules. Live checks for Fable 5, Fable 5.1,
Opus 5, Gemini 3.8 Flash, and Grok 4.7:

```bash
cargo test --features proxy --test integration_current_models -- --ignored --nocapture
```

The live tests consume API usage and are ignored during offline preflight.
Gemini 3.8 Flash and Opus 5 already had catalog/adapter support; Grok 4.7 is
the current xAI model ID (verified live 2026-09-21); retain Grok 4.6 for explicit routing. Keep the ChatGPT four-model allowlist independent.

Two knobs work across every provider: `reasoning_effort` (`none|low|medium|high|xhigh|max`) and `reasoning_mode` (`standard|pro`). A third, `reasoning_summary` (`auto|none`), controls reasoning-text visibility → Anthropic `thinking.display` (`auto`→`summarized`, the default when `reasoning_effort` is present so newer models like Sonnet 5 / Opus 4.7-4.8 return reasoning text instead of the API-default `omitted`; `none`→`omitted` for lower latency). Applies to both the adaptive and pre-4.6 enabled thinking builders; a caller-supplied `thinking` block bypasses it. Each provider transform maps them to its native dialect, **clamping to the nearest tier the target model accepts** (all boundaries verified live — e.g. `max` is native on OpenAI gpt-5.6 and GPT-6 Astra; Anthropic 4.6 rejects `xhigh` but has `max`; Gemini's enum tops out at `high`; xAI grok-4.20 models reject any reasoning param). `mode: "pro"` is native on OpenAI gpt-5.6/-pro models (`reasoning.mode`), emulated as a one-tier effort bump elsewhere; explicit `none` always wins. Native passthrough (`x-openai.reasoning`, `x-anthropic.thinking`, `x-gemini.thinkingConfig`) bypasses the mapping entirely and always takes precedence. **Full per-provider mapping tables: `docs/src/guides/reasoning.md`** — update it and the pinning tests in `tests/unit_*.rs` together whenever a mapping changes.

### Owned tool identities and stream state

`src/toolcall.rs` owns `WireToolId`, the bidirectional map, request projection,
and central call/result validation. Canonical ids are minted as `call_ls_*` and
never use a provider id directly. `wire_ids` must remain on persisted calls;
Responses `item_id` is distinct from correlation `id`. Source wire ids (including
Gemini's missing id) are restored on both calls/results, preserving signed
prefixes. Legacy input ids remain readable, but an owned id without its mapping
is an error. Never drop invalid tool history to make a request succeed.

`src/toolcall/streaming.rs` parses native events into `ToolDelta` and assembles
JSON arguments once. `src/streaming.rs::StreamNormalizer` is the public stateful
entry point used by HTTP streaming and manual SSE readers. Stateless provider
chunk methods no longer expose partial callable records. Tool signatures can
arrive after names/arguments; preserve their exact data/origin and original wire
container. Parallel choices close independently. The single-message proxy
projects choice zero; Rust retains choice indices.

Google's current Generate Content contract requires a signature on the first
function call of each current Gemini 3 batch, not every parallel call. Validate
that rule after filtering, preserve additional signatures exactly where present,
and retain optional native function ids on both sides. `unit_toolcall` covers
paired replay, order, signatures, invalid history, and transport delta sequences.

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
- `GET /v1/models` — list available models (filtered to configured providers).
  Serves two audiences from one body: an OpenAI SDK reads the `object: "list"` /
  `data[]` envelope (so `client.models.list()` works unmodified), llmshim's own
  clients read `models[]`. Both issue the same request, so there is no path to
  split on — the union *is* the split. `data[].id` is the routing id, requestable
  back as `model`. Built once in `proxy::convert::models_response`, shared with
  the gateway.
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

### Experimental: priority-queue gateway (`src/gateway/`, feature `gateway`)

An experimental scheduler that inverts the proxy's admission model: instead of *rejecting* when a provider's token bucket is empty, it **enqueues** each request into a per-provider priority queue and dispatches when capacity frees — ordered by **priority tier** (paying customer > free) then **FIFO** within a tier. Built for a fleet doing thousands of req/s that can't fire every call the instant it arrives. `gateway = ["proxy"]` (reuses the `RateLimiter` token buckets, `Backpressure`, and the proxy's request/response converters — `convert`/`error` are `pub(crate)` for this).

- **`Scheduler`** (`src/gateway/mod.rs`) owns one lane (queue + dispatcher task) per provider, so a rate-limited OpenAI queue never blocks a ready Anthropic one. `submit()` (unary) / `submit_stream()` (SSE) enqueue + await; a client disconnect / `max_wait` timeout cancels the queued job so it never burns a token.
- **Dispatcher** is event/timer-driven: pops the highest-priority job, acquires a concurrency slot *then* a rate token (so a saturated semaphore never wastes a token), and on a rate-limit miss **requeues** (preserving priority) and sleeps for exactly the `RetryAfter` the limiter reports — waking early on new work. No busy-wait, no `RateLimiter` changes. `max_wait` bounds **queue residence only** (a `started` signal releases it at dispatch), never the upstream call.
- **Fairness/aging** (`InMemoryQueue`): per-tier FIFO deques; dequeue picks the front with the highest *effective* priority = `tier + min(max_boost, wait/aging_step)`, so a starved low tier eventually overtakes a high-tier flood. Defaults (`aging_step` 5s, `max_boost` 16) keep normal load strictly priority-then-FIFO. `O(#tiers)` dequeue.
- **Streaming** (`submit_stream`): a `Delivery::Stream` job hands back an `mpsc` receiver once the upstream opens; the dispatcher forwards chunks and holds the concurrency permit for the whole stream (freed when it ends / the client disconnects). HTTP `POST /v1/chat/stream` (and `stream:true`) bridge it to SSE.
- **`RequestQueue`** trait is the in-process pluggable backend (in-memory default). Cross-process is a *separate* seam, not this trait (a `Job` holds a `oneshot`).
- **Distributed** (`src/gateway/distributed.rs`, feature `gateway-redis` = `gateway` + `redis-coordination`): a fleet shares a Redis **ZSET priority queue** per provider and a Redis **pub/sub response bus** keyed by request-id, so any instance dispatches any job and routes the result back to the origin's HTTP connection. Reuses the shared `RedisRateLimiter` for fleet-wide rate coordination. `llmshim gateway` auto-selects distributed mode when `LLMSHIM_REDIS_URL` is set and the binary has `gateway-redis`. Supports **unary + streaming** (typed `BusMessage`s over the channel). Full-parity with the in-memory lane:
  - **Aging** via a *virtual-deadline* score `enqueue_ms − tier·aging_step` popped with `ZPOPMIN` — one static score gives priority, FIFO, and anti-starvation aging with no re-scoring.
  - **At-least-once** via an atomic **lease** (Lua: `ZPOPMIN` queue → `processing` ZSET with a visibility deadline + recorded score) + ack/release + a background **reaper** (`reap_once`, also public for external cron) that requeues expired leases; streams refresh their lease as they run. A redelivered job may run twice (idempotent upstream calls). Env: `LLMSHIM_GATEWAY_LEASE_TIMEOUT_MS`, `LLMSHIM_GATEWAY_REQUEST_TIMEOUT_MS`.
- **HTTP** (`src/gateway/http.rs`): serves the proxy's `POST /v1/chat` + `/v1/chat/stream` contract routed through the scheduler; tier from the **`x-llmshim-priority`** header (uint, default 0). `RealDispatch` calls `completion_with_logger`/`stream`, mapping an upstream 429 → bucket penalty. Run: `llmshim gateway` (needs `--features gateway`, or `gateway-redis` for a fleet). Env: `LLMSHIM_GATEWAY_MAX_WAIT_MS`, `LLMSHIM_GATEWAY_QUEUE_DEPTH`, `LLMSHIM_GATEWAY_MAX_CONCURRENCY`, `LLMSHIM_GATEWAY_AGING_STEP_MS`, `LLMSHIM_GATEWAY_MAX_BOOST`, `LLMSHIM_GATEWAY_REQUEST_TIMEOUT_MS`, `LLMSHIM_GATEWAY_LEASE_TIMEOUT_MS`, `LLMSHIM_GATEWAY_MAX_ATTEMPTS`, `LLMSHIM_GATEWAY_IDEMPOTENCY_TTL_SECS`, plus all the proxy rate-limit vars.

#### Production hardening (gateway)

- **Auth + tier from identity** (`src/gateway/auth.rs`): set `LLMSHIM_GATEWAY_KEYS_FILE` to a JSON map `{ "<api-key>": {"tenant","tier","rpm","tpm"} }`. Then `Authorization: Bearer <key>` is **required** and tier/tenant come from the key — the client `x-llmshim-priority` header is ignored (closes the queue-jump exploit); missing/invalid key → 401. Unset ⇒ open dev mode (header trusted, tenant `anonymous`).
- **Per-tenant quotas** (`src/gateway/quota.rs`): per-`(tenant,provider)` RPM/TPM token buckets from the caller's identity, enforced in the HTTP layer (proactive 429 + `Retry-After`) on top of the global provider limits — one tenant can't monopolize shared capacity.
- **Idempotency** (`src/gateway/idempotency.rs`): `Idempotency-Key` header → cache-after-completion so a client retry returns the first result (no second billed call); in-memory for local, Redis for the fleet. Distributed at-least-once also has a **done-marker** (a completed job that gets redelivered is skipped) and a **dead-letter queue** after `max_attempts` (poison-job guard).
- **Observability**: `GET /metrics` (Prometheus, `src/gateway/metrics.rs`, dependency-free) — requests/dispatched/rejected counters, in-flight + queue-depth gauges, queue-wait + upstream-latency histograms; `GET /ready` (503 if Redis is down in distributed mode); `GET /v1/gateway/stats` (queue depths, dead-letter counts); every response carries `x-request-id`.
- **Graceful shutdown**: `gateway`/`proxy` drain in-flight on SIGTERM/Ctrl-C.
- **Load test**: `cargo run --release --features gateway --example gateway_loadtest` (throughput + zero-loss + priority-under-load asserts). **Deploy**: `Dockerfile` builds `--features gateway` by default (`--build-arg FEATURES=gateway-redis` for a fleet); `docker run … llmshim gateway`.
- Intentionally **not** done: per-tier (as opposed to per-tenant) queue-depth caps — the global depth cap + per-tenant quotas cover it; single-flight idempotency for *concurrent* same-key requests (only cache-after-completion).

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

### Capability plans and instance validation

`src/shim.rs::Plan` owns catalog-driven structured output, prompt tool calling,
and optional brief-rationale capture. `ShimClient` applies the same plan on
completion, stream, and fallback paths; direct provider transforms only support
native response-format translation. Never emit hidden synthetic calls, native
deliberation about those calls, or invalid attempts to logs/streams. Managed
streams buffer with a 32 MiB bound; top-level streaming uses an Arc-owned provider
so HTTP headers/keepalives can proceed while output is validated.

Validate generated data against the ORIGINAL schema with the network/filesystem
retriever disabled (`schema::validate`). Schema compile budgets are 96 levels,
32,768 JSON values and 8 MiB strings/keys. Only complete invalid answers receive
one repair; refusals and incomplete responses do not. Preserve both attempts'
reported usage. Unknown catalog fields remain unknown; `forced_tool_choice` is
independent of tools, with the verified Fable 5.1 prohibition overriding auto.
Keep synthetic schemas/instructions deterministic so unchanged requests preserve
prefix caching. Tests: `unit_shim`, proxy conversion tests, client request mocks.

### Signature observations and reasoning profiles

`providers/anthropic_signature.rs` has the default-on `signature-introspection`
feature and Option-only stub. Never use decoded metadata to change provenance,
messages, replay, routing, fallback, or caches. Synthetic fixtures only. Capture
one metric observation per response/terminal stream; logs read the observation
without incrementing it. `x-llmshim-served-model` belongs at response/event level.

`providers/anthropic_reasoning.rs::Profile` consumes catalog effort/budget unions
once per provider transform. Builtin verified options win over community data;
local/provider overrides retain documented precedence. Keep historical fallback
entries explicit in `catalog::builtin::anthropic_reasoning_options`, rather than
guessing support for new model names. Preserve mandatory native model constraints
(e.g. Fable adaptive-only and forbidden forced choices) separately.

### Native inbound facades

`proxy::wire` translates `/v1/messages` and `/v1/chat/completions` through the
existing chat handlers on both proxy and gateway. Do not create another dispatch,
auth, quota, or queue path. Gateway authenticates before receipt lookup and again
in its ordinary admission path; x-api-key maps to Bearer only when Authorization
is absent. Native idempotency keys include credential and protocol scope.

The native facade persists issued replay metadata in private atomic local files,
configured by `LLMSHIM_REPLAY_RECEIPTS_DIR`. Clients must preserve the native
message/ID and server receipts across restarts. Never stamp unknown native thinking
with current-target provenance. Receipts restore original blocks, then the common
replay filter decides eligibility. Missing owned IDs and edited calls error.
Text remains incremental; complete reasoning/tool blocks follow once metadata is
ready. Status/Retry-After and gateway request IDs survive error translation.
`src/error/normalize.rs` owns error unwrapping for compact JSON/SSE, gateway,
and native endpoints. Keep display messages readable and source type/code/param
metadata separate; do not move this logic back into a wire-only formatter.
JSON responses carry native metadata in response extensions, and SSE errors carry
an optional structured error object so native rendering remains lossless.
`n > 1` is refused rather than emulated: the OpenAI backend is the Responses
API (no `n`), Anthropic Messages and Gemini have no `n` either, and the
single-message proxy projects choice zero. The refusal is a correctly shaped
`{"error":{type,message,param:"n",code:"unsupported_parameter"}}`, carried
through `normalize_error` so both facades render it natively.
Tests: `unit_wire`, gateway `http::native_tests`. Use a temporary receipt directory
in tests; never put real signatures, credentials, or conversations in fixtures.

### Provider and CLI regression gates

`tests/unit_provider_contracts.rs` discovers Provider implementations from the
Rust AST and requires a fixture for each. New adapters must preserve compatible
reasoning, drop foreign/untracked blocks and signatures, and reject invalid tool
history before serialization. Tool-call containers accept arrays or null only.
HTTP handlers validate canonical history before committing to an SSE response.

`src/cli.rs` validates arguments before side effects. Server options override env
and saved host/port; help must not start a service. Bind errors are returned to
main for a clean exit. Tests use ephemeral loopback ports and never touch an
existing server. Run unit_cli, unit_provider_contracts, unit_wire and gateway
HTTP tests when changing these boundaries.

### Schema memoization and benchmark receipts

`src/schema/memo.rs` caches only schema inputs/results and `Normalization` reports.
The process-local cache is bounded to 512 entries/16 MiB estimated owned bytes,
uses LRU eviction, verifies input/options after a hash hit, and performs expensive
normalization outside the lock. Key all effective Options, including budgets.
Resolve environment overrides before lookup. Preserve object order and signed
zero: semantically equal numbers can produce different spilled descriptions.
No-op hits can retain the input value only after exact input/output comparison.

`LLMSHIM_NO_SCHEMA_CACHE=1` bypasses memoization while keeping normalization active.
Tests cover collisions, eviction, concurrency, returned-value independence,
report/fallback fidelity, environment changes and fresh tool metadata.

`cargo run --release --example bench` loads the two configured native API keys and
makes 43 logical requests plus client retries. Missing keys fail before network
calls with setup instructions. Provider errors must never print credentials.
`--transforms-only` needs no keys or HTTP calls; use it for cache-on/off comparison.
Keep README measurements dated, name the actual models/host/build/workload, and
separate API latency, full-transform CPU time, and RSS. Do not mix fresh Rust
figures with old Python results or claim unmeasured network-only latency shares.

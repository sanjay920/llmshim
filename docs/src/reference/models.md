# Model discovery

Runtime discovery is the canonical answer to “what can this configured
installation offer?”

```bash
llmshim models
curl http://localhost:3000/v1/models
```

Both commands filter the built-in registry to providers with configured API
keys or a saved ChatGPT login. The proxy returns `id`, `provider`, and unprefixed `name`; the CLI prints
the ID and display label.

The standalone `llmshim-catalog` crate supplies broader metadata without
changing this curated discovery list:

```bash
llmshim models --all --json
llmshim models --refresh
LLMSHIM_CATALOG_OFFLINE=1 llmshim models --all
```

Its owned `ModelInfo` adds model family, pricing, reasoning effort/budget
options, modalities, dates, and field-level sources. `llmshim::catalog::resolve`
reads the resident catalog; `llmshim::models::spec` retains the borrowed,
compile-time API for curated and historical facts.

Startup reads the vendored floor, cached data, and local overrides without
waiting for network. Refresh uses ETags and a 24-hour TTL; errors keep the old
snapshot available. `Router::from_env()` schedules one such refresh;
`Router::from_env_without_catalog_refresh()` schedules none and leaves it to
`Router::refresh_catalog_in_background()`. Offline mode uses only vendored and
local layers. User
overrides live in `~/.config/llmshim/models.toml`, project overrides in
`.llmshim/models.toml`, and cached data in `~/.cache/llmshim/models.dev.json`.
Local overrides win; verified builtin assertions win over models.dev data.
Provider discovery is explicit, accepts capability facts, and never supplies
pricing. Catalog entries do not imply credentials or account entitlement.

## Advertised catalog

The CLI picker, `llmshim models`, and `/v1/models` share this curated set of
13 routes. It keeps the current model in each retained tier. Google entries
are stable releases only. Credentials determine which providers are listed.

### OpenAI

| ID | Display name |
|---|---|
| `openai/gpt-6-astra` | GPT-6 Astra |
| `openai/gpt-6-sol` | GPT-6 Sol |
| `openai/gpt-6-luna` | GPT-6 Luna |

### Anthropic

| ID | Display name |
|---|---|
| `anthropic/claude-fable-5-1` | Claude Fable 5.1 |
| `anthropic/claude-opus-5-5` | Claude Opus 5.5 |
| `anthropic/claude-sonnet-5` | Claude Sonnet 5 |
| `anthropic/claude-haiku-4-5-20251001` | Claude Haiku 4.5 |

### Google Gemini

| ID | Display name |
|---|---|
| `gemini/gemini-3.8-flash` | Gemini 3.8 Flash |
| `gemini/gemini-3.5-flash-lite` | Gemini 3.5 Flash Lite |

### xAI

| ID | Display name |
|---|---|
| `xai/grok-4.7` | Grok 4.7 |

Grok 4.7 accepts text and images with a 500,000-token context window.
Reasoning supports `low`, `medium`, `high` (the upstream default), and `xhigh`;
llmshim maps `none` to `low` and `max` to `xhigh`. Explicit `xai/grok-4.6`
requests remain supported, but discovery advertises 4.7.

Native xAI standard rates, USD per million tokens:

| Total input tokens, including cached input | Input | Cached input | Output |
|---|---:|---:|---:|
| Up to 200,000 | $2 | $0.50 | $6 |
| Over 200,000 | $4 | $1 | $12 |

The higher tier applies to the whole request. These are native xAI rates;
OpenRouter has its own prices. See the [release notes](https://docs.x.ai/developers/release-notes).
Grok 4.7 Fast is available in Cursor/Grok Build, not as a public xAI API model.

For OpenRouter, use `openrouter/x-ai/grok-4.7`. You can append `:nitro` and set
`x-openrouter.provider.zdr = true` together. [Nitro](https://openrouter.ai/docs/guides/routing/model-variants/nitro)
prioritizes throughput and allows priority endpoints, whose rates can be higher.
[ZDR](https://openrouter.ai/docs/guides/features/zdr) restricts eligible inference
endpoints to those with a zero-retention policy. On `/v1/chat`, put
`x-openrouter` inside `provider_config`:

```json
{
  "model": "openrouter/x-ai/grok-4.7:nitro",
  "messages": [{"role": "user", "content": "Reply with only pong."}],
  "config": {"max_tokens": 1024, "reasoning_effort": "low"},
  "provider_config": {"x-openrouter": {"provider": {"zdr": true}}}
}
```

These routing options do not enforce ZDR on an llmshim fallback to a different
provider. For throughput sorting without priority endpoints, omit `:nitro` and
set `x-openrouter.provider.sort` to `"throughput"` alongside `zdr`.

The Nitro route has replay metadata but no fixed catalog price. When OpenRouter
reports its bill, llmshim uses it for `usage.cost_usd` and sets
`usage.cost_source` to `"provider"`. Without a reported bill or locally configured
rates, `cost_usd` remains `null`.

A partial stream bill that is not confirmed as the terminal provider bill uses
`cost_source: "provider_floor"`; it is a known lower bound rather than an exact
invoice.

The [OpenAI model catalog](https://developers.openai.com/api/docs/models) lists
GPT-6 Sol and Luna at $2/$10 and $0.10/$0.50 per million input/output tokens
on the Standard tier. Prompts above 272,000 input tokens use higher rates
for the whole request; the built-in catalog records those tiers.

Anthropic positions Claude Opus 5.5 at Fable 5.1 level on most work, and
estimates around 40% less cost per typical task than Opus 5. Its $4/$20
input/output token prices are 20% below Opus 5. Cache reads cost $0.20 per
million tokens. Five-minute cache writes cost $5 per million;
one-hour writes cost $8. Because the catalog has one aggregate cache-write
rate, its $8 estimate is the conservative one-hour ceiling. See the
[Anthropic model pricing](https://platform.claude.com/docs/en/models/opus-5-5/overview).

### ChatGPT subscription

| ID | Display name |
|---|---|
| `chatgpt/gpt-6-astra` | GPT-6 Astra (ChatGPT) |
| `chatgpt/gpt-6-sol` | GPT-6 Sol (ChatGPT) |
| `chatgpt/gpt-6-luna` | GPT-6 Luna (ChatGPT) |

The ChatGPT provider accepts only these three model IDs. Older and unlisted
IDs are rejected locally before authentication or network calls, including
when selected through a Router alias. The list is not an account entitlement
check. The new models are rolling out, so availability also depends on
workspace settings and account entitlement; see the
[ChatGPT and Codex changelog](https://learn.chatgpt.com/docs/changelog).
Context and output limits remain unspecified because they depend on the
account/backend. Bare GPT names still route to API-key OpenAI.

## Spec metadata

Each registry entry can also carry **spec metadata** beyond its identity, so a
consumer can read a model's facts from one place instead of maintaining its own
parallel table:

| Field | Type | Meaning |
|---|---|---|
| `context_window_tokens` | `Option<u32>` | Total context window (input + output), if published |
| `max_output_tokens` | `Option<u32>` | Maximum output tokens per response, if published |
| `capabilities.tools` | `Support` | Function/tool calling |
| `capabilities.streaming` | `Support` | Streaming responses |
| `capabilities.images` | `Support` | Image input |
| `capabilities.prompt_cache` | `Support` | Provider-side prompt caching |
| `capabilities.structured_output` | `Support` | JSON-schema / structured responses |
| `capabilities.parallel_tool_calls` | `Support` | Multiple tool calls per turn |
| `capabilities.reasoning` | `Support` | Accepts a reasoning-effort control |

`Support` is tri-state: `Supported`, `Unsupported`, or `Unknown`. **`Unknown` is
honest, not a bug** — llmshim never guesses a spec to fill a cell. Context
window, output ceiling, and capabilities are versioned snapshots populated
from official provider docs (platform.claude.com, developers.openai.com,
ai.google.dev, docs.x.ai), and `reasoning` is cross-checked against the provider
clamp logic. What's deliberately left `Unknown`/`None`:

- `parallel_tool_calls` for most models (providers rarely document it per model);
- xAI `max_output_tokens` (not published) and per-model streaming.

Note also that Gemini publishes an input limit rather than a combined total, so
`context_window_tokens` is the input window with `max_output_tokens` separate.

`reasoning` is deliberately a single flag — "does this model accept a reasoning
control at all." The detailed per-tier mapping is not duplicated here; it lives
in the [reasoning guide](../guides/reasoning.md) and the provider transforms.

Look up one model's full spec from the Rust crate:

```rust
if let Some(m) = llmshim::models::spec("openai/gpt-6-sol") {
    println!("{}: reasoning = {:?}", m.label, m.capabilities.reasoning);
}
```

`spec()` accepts a full id (`"openai/gpt-6-sol"`) or a bare name
(`"gpt-6-sol"`). It also retains historical metadata for explicit lookups;
those models are absent from the advertised list. `None` means no metadata
is recorded. Specs remain a point-in-time snapshot pinned by the crate version.

## Routing beyond the catalog

Pruning the advertised list does not remove provider adapters or their
compatibility behavior. Older explicit OpenAI and Anthropic IDs still reach their provider, subject
to upstream availability. The ChatGPT subscription route accepts only the
three current GPT-6 models. Known historical IDs also remain selectable by
exact ID in the CLI, and `spec()` keeps their metadata.

The Router does not check explicit model names against this registry. If a
provider is registered, `provider/arbitrary-model-id` is routed to that
provider with `arbitrary-model-id` unchanged. A bare, unregistered model name
works only when its prefix identifies a provider (`gpt`, `o1`, `o3`, `o4`,
`claude`, `gemini`, or `grok`).

The ChatGPT provider is the exception: it validates requests against the four
supported subscription models above.

**OpenRouter** is intentionally not enumerated above — its catalog is large and
dynamic. Any `openrouter/<vendor>/<model>` slug routes through (e.g.
`openrouter/anthropic/claude-sonnet-5`, `openrouter/meta-llama/llama-3.1-70b-instruct:nitro`);
the slug's internal slash and `:variant` suffix are preserved. Because its
slugs collide with other providers' prefixes, OpenRouter has no bare-model
inference — always address it explicitly as `openrouter/…`.

**Self-hosted vLLM / SGLang** are likewise not enumerated. Set `VLLM_BASE_URL` or `SGLANG_BASE_URL` (with an optional `*_API_KEY`) and address the served model as `vllm/<served-model>` or `sglang/<served-model>` — e.g. `sglang/Qwen/Qwen3.6-35B-A3B-FP8`. Local vs remote is just the base-URL value.

Rust applications can also define one-level Router aliases with
`Router::alias`. Those aliases are not part of the static registry and are not
configured by the stock CLI or proxy. See [Models and the Router](../concepts/routing.md).

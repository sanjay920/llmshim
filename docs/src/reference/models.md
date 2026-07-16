# Model discovery

Runtime discovery is the canonical answer to “what can this configured
installation offer?”

```bash
llmshim models
curl http://localhost:3000/v1/models
```

Both commands filter the built-in registry to providers with configured API
keys. The proxy returns `id`, `provider`, and unprefixed `name`; the CLI prints
the ID and display label.

## Registered catalog

The current registry contains 23 entries, newest first within each provider.
This page mirrors `src/models.rs`; use runtime discovery rather than parsing
this table in applications.

### OpenAI

| ID | Display name |
|---|---|
| `openai/gpt-5.6-sol` | GPT-5.6 Sol |
| `openai/gpt-5.6-terra` | GPT-5.6 Terra |
| `openai/gpt-5.6-luna` | GPT-5.6 Luna |
| `openai/gpt-5.5` | GPT-5.5 |
| `openai/gpt-5.5-pro` | GPT-5.5 Pro |
| `openai/gpt-5.4` | GPT-5.4 |
| `openai/gpt-5.4-pro` | GPT-5.4 Pro |
| `openai/gpt-5.4-mini` | GPT-5.4 Mini |
| `openai/gpt-5.4-nano` | GPT-5.4 Nano |

### Anthropic

| ID | Display name |
|---|---|
| `anthropic/claude-opus-4-8` | Claude Opus 4.8 |
| `anthropic/claude-sonnet-5` | Claude Sonnet 5 |
| `anthropic/claude-opus-4-7` | Claude Opus 4.7 |
| `anthropic/claude-opus-4-6` | Claude Opus 4.6 |
| `anthropic/claude-sonnet-4-6` | Claude Sonnet 4.6 |
| `anthropic/claude-haiku-4-5-20251001` | Claude Haiku 4.5 |

### Google Gemini

| ID | Display name |
|---|---|
| `gemini/gemini-3.5-flash` | Gemini 3.5 Flash |
| `gemini/gemini-3.1-pro-preview` | Gemini 3.1 Pro |
| `gemini/gemini-3-flash-preview` | Gemini 3 Flash |

### xAI

| ID | Display name |
|---|---|
| `xai/grok-4.5` | Grok 4.5 |
| `xai/grok-4.3` | Grok 4.3 |
| `xai/grok-4.20-multi-agent-beta-0309` | Grok 4.20 Multi-Agent |
| `xai/grok-4.20-beta-0309-reasoning` | Grok 4.20 Reasoning |
| `xai/grok-4.20-beta-0309-non-reasoning` | Grok 4.20 |

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
honest, not a bug** — llmshim never guesses a spec to fill a cell. As of the
2026-07-16 snapshot, context window, output ceiling, and capabilities are
populated from official provider docs (platform.claude.com, developers.openai.com,
ai.google.dev, docs.x.ai), and `reasoning` is cross-checked against the provider
clamp logic. What's deliberately left `Unknown`/`None`:

- `parallel_tool_calls` for most models (providers rarely document it per model);
- xAI `max_output_tokens` (not published) and per-model streaming.

A few documented exceptions are recorded honestly too — e.g. `gpt-5.5-pro` has
`streaming: Unsupported` and `gpt-5.4-pro` has `structured_output: Unsupported`.
Note also that Gemini publishes an input limit rather than a combined total, so
`context_window_tokens` is the input window with `max_output_tokens` separate.

`reasoning` is deliberately a single flag — "does this model accept a reasoning
control at all." The detailed per-tier mapping is not duplicated here; it lives
in the [reasoning guide](../guides/reasoning.md) and the provider transforms.

Look up one model's full spec from the Rust crate:

```rust
if let Some(m) = llmshim::models::spec("openai/gpt-5.6-sol") {
    println!("{}: reasoning = {:?}", m.label, m.capabilities.reasoning);
}
```

`spec()` accepts a full id (`"openai/gpt-5.6-sol"`) or a bare name
(`"gpt-5.6-sol"`) and returns `None` for unregistered models. These specs are a
point-in-time snapshot pinned by the crate version, exactly like the list above.

## Catalog is not an allowlist

The Router does not check explicit model names against this registry. If a
provider is registered, `provider/arbitrary-model-id` is routed to that
provider with `arbitrary-model-id` unchanged. A bare, unregistered model name
works only when its prefix identifies a provider (`gpt`, `o1`, `o3`, `o4`,
`claude`, `gemini`, or `grok`).

Rust applications can also define one-level Router aliases with
`Router::alias`. Those aliases are not part of the static registry and are not
configured by the stock CLI or proxy. See [Models and the Router](../concepts/routing.md).

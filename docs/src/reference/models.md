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

The current registry contains 26 entries, newest first within each provider.
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
| `gemini/gemini-3.1-flash-lite-preview` | Gemini 3.1 Flash Lite |
| `gemini/gemini-3-flash-preview` | Gemini 3 Flash |

### xAI

| ID | Display name |
|---|---|
| `xai/grok-4.5` | Grok 4.5 |
| `xai/grok-4.3` | Grok 4.3 |
| `xai/grok-4.20-multi-agent-beta-0309` | Grok 4.20 Multi-Agent |
| `xai/grok-4.20-beta-0309-reasoning` | Grok 4.20 Reasoning |
| `xai/grok-4.20-beta-0309-non-reasoning` | Grok 4.20 |
| `xai/grok-4-1-fast-reasoning` | Grok 4.1 Fast Reasoning |
| `xai/grok-4-1-fast-non-reasoning` | Grok 4.1 Fast |

## Catalog is not an allowlist

The Router does not check explicit model names against this registry. If a
provider is registered, `provider/arbitrary-model-id` is routed to that
provider with `arbitrary-model-id` unchanged. A bare, unregistered model name
works only when its prefix identifies a provider (`gpt`, `o1`, `o3`, `o4`,
`claude`, `gemini`, or `grok`).

Rust applications can also define one-level Router aliases with
`Router::alias`. Those aliases are not part of the static registry and are not
configured by the stock CLI or proxy. See [Models and the Router](../concepts/routing.md).

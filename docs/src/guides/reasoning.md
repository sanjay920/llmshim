# Reasoning controls

llmshim exposes one reasoning vocabulary across providers. You state the
desired depth; the selected provider adapter maps it to what that model accepts.

> **Availability:** Rust: top-level fields · CLI: `high` by default · Proxy/clients: fields under `config`

## The two knobs

| Knob | Values | Meaning |
|---|---|---|
| `reasoning_effort` | `none` \| `low` \| `medium` \| `high` \| `xhigh` \| `max` | Requested thinking or reasoning depth |
| `reasoning_mode` | `standard` (default) \| `pro` | Requests substantially more model work, accepting higher latency and cost |

In a Rust request, both are top-level fields:

```json
{
  "model": "anthropic/claude-sonnet-5",
  "messages": [{"role": "user", "content": "Solve this carefully."}],
  "reasoning_effort": "high",
  "reasoning_mode": "pro"
}
```

In the proxy contract, put them under `config`:

```json
{
  "model": "anthropic/claude-sonnet-5",
  "messages": [{"role": "user", "content": "Solve this carefully."}],
  "config": {
    "reasoning_effort": "high",
    "reasoning_mode": "pro"
  }
}
```

## Preserve reasoning for replay

Responses carry an ordered `message.reasoning` array. Every block has `kind`
(`text`, `redacted`, or `encrypted`) and `origin`: provider, upstream model,
coarse model family, wire format, receipt time, and an optional issuer binding.
Text blocks carry `text`; opaque blocks carry `data`. Signatures, Responses
item ids, original structured payloads and container identity are retained when
present. Preserve the complete assistant message when building the next request.
The proxy exposes the same array inside `message`; its top-level `reasoning`
string remains a display-only convenience.

One replay policy applies to all adapters and fallback attempts: the target
family and wire must match, and encrypted blocks also require the same provider
and account binding. Unknown families and missing origins fail closed. Signed
Anthropic blocks precede text and tools in their original order. Responses
requests use `store:false`, include encrypted reasoning, and replay those items
locally. Native overrides cannot turn storage on or bypass reasoning provenance.
OpenAI and xAI use this stateless Responses path; see
[xAI's encrypted reasoning contract](https://docs.x.ai/developers/model-capabilities/text/reasoning).

API-key account bindings hash the endpoint and credential; they conservatively
stop matching after key rotation. OAuth bindings use the account identifier
captured from the actual request. No credential or raw account identifier is
emitted. Rejected replay is silent and increments a bounded-reason counter,
available through `llmshim::reasoning::replay_counters()`.

Gemini tool-call signatures are objects containing `data` and `origin`. Keep
that entire object on the tool call. Legacy `reasoning_content`,
`reasoning_signature`, `redacted_reasoning_content`, and string tool signatures
are read for one migration release, but untracked data is dropped. An explicit
`reasoning_origin` can accompany legacy message fields during migration; new
writers emit only the block array. Models absent from the catalog need a local
family assertion before their reasoning can be replayed.

Streaming reasoning uses the same blocks with a part `index` and an optional
`replace:true` flag for completed item snapshots. `ReasoningAccumulator` assembles
fragments and removes stream framing before storage:

```rust
let mut reasoning = llmshim::reasoning::ReasoningAccumulator::default();
// For each normalized chunk:
reasoning.push(&chunk["choices"][0]["delta"]);
// When the stream has completed successfully:
assistant["reasoning"] = serde_json::json!(reasoning.blocks());
```

Use `reasoning_text(&message_or_delta)` for display without inspecting opaque
data. A provider may expose only a summary or no readable reasoning at all.

## Portable intent or native control

Choose per request:

1. **Use the unified controls.** llmshim maps the requested effort and mode to
   the selected model family, clamping to the nearest accepted tier when the
   model has a smaller vocabulary.
2. **Use the provider's native dialect.** Pass an exact native object through
   `x-openai`, `x-anthropic`, or `x-gemini`. Native configuration wins over the
   unified controls.

```mermaid
flowchart LR
    U[Unified effort + mode] --> P{Native override present?}
    P -- Yes --> N[x-provider native config]
    P -- No --> F[Model-family mapping]
    F --> C[Clamp to supported tier]
    C --> W[Provider-native reasoning control]
```

See [Native provider controls](native-controls.md) for passthrough examples.

## Effort mapping tables

These mappings were verified against the live provider APIs. A bold value is a
clamp rather than a direct name-for-name mapping.

### OpenAI Responses API

| unified | GPT-6 Astra | GPT-5.6 Sol / Terra / Luna |
|---|---|---|
| `none` | **`low`** | `none` |
| `low` | `low` | `low` |
| `medium` | `medium` | `medium` |
| `high` | `high` | `high` |
| `xhigh` | `xhigh` | `xhigh` |
| `max` | `max` | `max` |

OpenAI receives `reasoning.effort`. Legacy `minimal` input clamps to `low`
on all four advertised models.

### Anthropic Messages API

Adaptive models use `thinking: {type}` plus `output_config: {effort}`:

| unified | Fable 5.1 | Opus 5 / Sonnet 5 |
|---|---|---|
| `none` | `adaptive` + **`low`** | `thinking: {type: "disabled"}` |
| `low` | `adaptive` + `low` | `adaptive` + `low` |
| `medium` | `adaptive` + `medium` | `adaptive` + `medium` |
| `high` | `adaptive` + `high` | `adaptive` + `high` |
| `xhigh` | `adaptive` + `xhigh` | `adaptive` + `xhigh` |
| `max` | `adaptive` + `max` | `adaptive` + `max` |

On models that can disable thinking, `reasoning_effort: "none"` maps to disabled
thinking. Fable 5.1 always uses adaptive thinking, so `none` maps to `low`.
Explicit native `thinking.type: "disabled"` or `"enabled"` is rejected locally
for Fable. Fable and Opus 5 omit `temperature`, `top_p`, and `top_k` regardless
of whether a thinking object was explicitly provided.

Fable behavior and its five effort levels follow Anthropic's
[migration guide](https://platform.claude.com/docs/en/models/fable-5-1/migration-guide)
and Models API. The low-effort and rejected-parameter paths are also checked
against the live API in this repository.

Haiku 4.5 uses enabled thinking with a token budget scaled from `max_tokens` and floored at 1024:

| unified | budget |
|---|---|
| `none` | no `thinking` key |
| `low` | 25% of `max_tokens` |
| `medium` | 50% |
| `high` | 75% |
| `xhigh` | 90% |
| `max` | `max_tokens - 1` |

### Gemini

Gemini uses the four-rung
`generationConfig.thinkingConfig.thinkingLevel` enum:

| unified | Gemini 3.5 Flash-Lite | Gemini 3.8 Flash |
|---|---|---|
| `none` | `minimal` (zero thinking tokens) | **`low`** (this model cannot disable thinking) |
| `low` | `low` | `low` |
| `medium` | `medium` | `medium` |
| `high` | `high` | `high` |
| `xhigh` | **`high`** | **`high`** |
| `max` | **`high`** | **`high`** |

Gemini 3.8 Flash rejects `minimal` and `thinkingBudget: 0`, so `none` clamps
to `low`. Gemini 3.5 Flash-Lite accepts `minimal` and can disable thinking.
Native controls remain available through `x-gemini.thinkingConfig`.

### ChatGPT subscription

GPT-5.6 Sol, Terra, and Luna use the GPT-5.6 mapping above.
GPT-6 Astra preserves `low`, `medium`, `high`, `xhigh`, and `max`;
`none` and `minimal` clamp to `low` because Astra cannot disable reasoning.
The shared Responses translator applies this mapping to Astra through either
ChatGPT or API-key OpenAI. These effort values follow the
[official Astra model documentation](https://developers.openai.com/api/docs/models/gpt-6-astra).
Native `x-chatgpt.reasoning` overrides the unified mapping.

### xAI Responses API

xAI receives the nested native shape `reasoning: {effort}`:

| unified | Grok 4.6 |
|---|---|
| `none` | **`low`** |
| `low` | `low` |
| `medium` | `medium` |
| `high` | `high` |
| `xhigh` | `xhigh` |
| `max` | **`xhigh`** |

Grok 4.6 cannot disable reasoning, so `none` clamps to `low`.

## Mode mapping: `reasoning_mode: "pro"`

| Provider / model | What `pro` does |
|---|---|
| OpenAI GPT-5.6 family | Native `reasoning.mode: "pro"` |
| OpenAI GPT-6 Astra | One-tier effort bump (`low → medium → high → xhigh`); explicit `max` stays `max` |
| Anthropic | One-tier effort bump (`low → medium → high → xhigh → max`) |
| Gemini | One-tier bump within its four-rung enum, capped at `high` |
| xAI Grok 4.6 | One-tier bump, capped at `xhigh` |

Rules that hold across providers:

- `standard` is the default and is not sent on the wire.
- An explicit effort of `none` wins over `pro`; a request to turn reasoning off
  is not bumped back on, except where the model itself cannot disable it.
- `pro` without an effort lets OpenAI native-mode models select their own
  effort. Other models behave as a default `medium` bumped to `high`.

## Precedence

1. Native passthrough under `x-openai`, `x-anthropic`, or `x-gemini` wins and
   is not translated into another dialect. Anthropic also accepts top-level
   `thinking` and `output_config` in the Rust contract.
2. Otherwise, unified `reasoning_effort` and `reasoning_mode` are mapped and
   clamped according to the tables above.
3. With neither, the provider/model default applies. The provider decides whether and how
   much to reason unless an explicit setting is supplied.

The adapter mappings are pinned by provider unit tests. The tables above
cover the advertised models; compatibility mappings for explicit older IDs
remain in the adapters and tests. Provider capabilities can change, so the
implementation and these tables must move together.

## Signature observations

The default `signature-introspection` feature makes a best-effort read of an
undocumented Anthropic signature header. Its public decoder,
`providers::anthropic_signature::serving_model_from_signature`, returns `Option`;
unknown formats (including headers without a model), malformed data, and the
feature-disabled stub return `None`. Tests use synthetic protobufs only.

This never changes `origin.model`, a stored message, replay eligibility, routing,
fallback, or cache decisions. The decoded value is unverified metadata, not
authentication or proof of which model ran. Dotted/dashed spellings and dated
snapshots compare as the same model line; an unrecognized internal codename may
be reported as a mismatch. Logs record `gateway_integrity` as `ok`, `mismatch`
(with requested/served names), or `unknown`. Unknowns produce no warning.

A mismatch increments `providers::anthropic_signature::mismatch_count()` and adds
`x-llmshim-served-model` outside the assistant message. Streaming adds it to the
terminal canonical chunk or the proxy's `done` event. Multiple observed names
are comma separated. Disable inspection with `--no-default-features`; model
requests and stored message contents remain identical, while observation fields
and metrics can differ.

Anthropic effort and budget selection now comes from catalog `reasoning_options`.
An effort entry selects adaptive thinking and clamps to an advertised tier; a
budget entry selects enabled thinking and respects its minimum, maximum, and
`max_tokens - 1`. An impossible budget fails before HTTP dispatch. Verified
builtin entries preserve the established mappings; a bounded table of historical
model lines supplies missing metadata. Unknown future names are not guessed.

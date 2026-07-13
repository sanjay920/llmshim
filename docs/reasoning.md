# Unified reasoning controls

llmshim exposes **one reasoning vocabulary across every provider**, with two
knobs:

| Knob | Values | Meaning |
|---|---|---|
| `reasoning_effort` | `none` \| `low` \| `medium` \| `high` \| `xhigh` \| `max` | How much thinking/reasoning depth to request |
| `reasoning_mode` | `standard` (default) \| `pro` | `pro` requests substantially more model work, tolerating higher latency/cost |

Set them via `config` on the proxy (`{"config": {"reasoning_effort": "xhigh", "reasoning_mode": "pro"}}`),
the equivalent fields in every language client, or as top-level keys on the
library's OpenAI-format request value.

## Two ways to control reasoning — pick per request

1. **Trust llmshim (this document):** use `reasoning_effort` / `reasoning_mode`.
   llmshim translates to each provider's native dialect and **clamps to the
   nearest tier the target model actually accepts**, so a request never 400s
   because a model lacks a tier. Every mapping below was verified against the
   live provider APIs.
2. **Speak the native dialect:** pass the provider's exact wire shape through
   untouched — llmshim never rewrites these, and an explicit native config
   always wins over the unified knobs:
   - OpenAI: `x-openai.reasoning` (full native object: `effort`, `mode`, `summary`, `context`)
   - Anthropic: `x-anthropic.thinking`, or top-level `thinking` / `output_config`
   - Gemini: `x-gemini.thinkingConfig` (including the legacy `thinkingBudget` int)
   - xAI: not needed — its native shape (`reasoning: {effort}`) is exactly what the unified mapping emits

## Effort mapping tables (verified live)

### OpenAI (Responses API, native `reasoning.effort`)

| unified | gpt-5.6-sol/terra/luna | gpt-5.5 | gpt-5.5-pro / gpt-5.4-pro | gpt-5.4 / -mini / -nano |
|---|---|---|---|---|
| `none` | `none` | `none` | **`medium`** (pro tier rejects none) | `none` |
| `low` | `low` | `low` | **`medium`** | `low` |
| `medium` | `medium` | `medium` | `medium` | `medium` |
| `high` | `high` | `high` | `high` | `high` |
| `xhigh` | `xhigh` | `xhigh` | `xhigh` | `xhigh` |
| `max` | `max` | **`xhigh`** (max is 5.6-only) | **`xhigh`** | **`xhigh`** |

Legacy `minimal` input is accepted and clamps to `low` everywhere it would 400
(pro tier, 5.6, 5.4 family) — gpt-5.5 still accepts it natively.

### Anthropic (Messages API)

Adaptive family (Opus 4.6/4.7/4.8, Sonnet 4.6, Sonnet 5) — sent as
`thinking: {type}` + `output_config: {effort}`:

| unified | Opus 4.7/4.8, Sonnet 5 | Opus/Sonnet 4.6 |
|---|---|---|
| `none` | `thinking: {type: "disabled"}` | `thinking: {type: "disabled"}` |
| `low` | `adaptive` + `low` | `adaptive` + `low` |
| `medium` | `adaptive` + `medium` | `adaptive` + `medium` |
| `high` | `adaptive` + `high` | `adaptive` + `high` |
| `xhigh` | `adaptive` + `xhigh` | `adaptive` + **`max`** (4.6 rejects xhigh; its tiers are low/medium/high/max) |
| `max` | `adaptive` + `max` | `adaptive` + `max` |

> Note: adaptive models **think by default** even when no reasoning config is
> sent. `effort: "none"` (→ `type: "disabled"`) is the only way to get zero
> thinking tokens.

Pre-4.6 family (Haiku 4.5, Claude 3.7, Sonnet/Opus 4.5-era) — sent as
`thinking: {type: "enabled", budget_tokens}` scaled to `max_tokens`
(floor 1024):

| unified | budget |
|---|---|
| `none` | *(no `thinking` key — these models don't think unless asked)* |
| `low` | 25% of max_tokens |
| `medium` | 50% |
| `high` | 75% |
| `xhigh` | 90% |
| `max` | max_tokens − 1 |

### Gemini (`generationConfig.thinkingConfig.thinkingLevel` — a 4-rung enum)

| unified | gemini-3.5-flash / 3-flash-preview / 3.1-flash-lite-preview | gemini-3.1-pro-preview |
|---|---|---|
| `none` | `minimal` (0 thinking tokens) | **`low`** (this model cannot disable thinking — verified: rejects both `minimal` and `thinkingBudget: 0`) |
| `low` | `low` | `low` |
| `medium` | `medium` | `medium` |
| `high` | `high` | `high` |
| `xhigh` | **`high`** (enum ceiling) | **`high`** |
| `max` | **`high`** | **`high`** |

Finer-than-enum control exists via the legacy `thinkingBudget` int, which
still works on Gemini 3.x but is undocumented for this generation — available
via `x-gemini.thinkingConfig` only.

### xAI (Responses API, native nested `reasoning: {effort}`)

| unified | grok-4.3 / grok-4-1-fast-* | grok-4.5 | grok-4.20-\*-reasoning / -non-reasoning |
|---|---|---|---|
| `none` | `none` | **`low`** (4.5 can't disable reasoning) | *(omitted — see below)* |
| `low` | `low` | `low` | *(omitted)* |
| `medium` | `medium` | `medium` | *(omitted)* |
| `high` | `high` | `high` | *(omitted)* |
| `xhigh` | `xhigh` | `xhigh` | *(omitted)* |
| `max` | **`xhigh`** (xAI rejects max) | **`xhigh`** | *(omitted)* |

grok-4.20 models are **name-locked**: reasoning on/off is encoded in the model
name and the API 400s on *any* reasoning parameter, so llmshim omits it
entirely for that family — pick the `-reasoning` or `-non-reasoning` name
instead. (Also note: the flat `reasoning_effort` string that OpenAI-compatible
APIs use is silently *ignored* by xAI — only the nested object works, which is
what llmshim now sends.)

## Mode mapping (`reasoning_mode: "pro"`)

| Provider / model | What `pro` does |
|---|---|
| OpenAI gpt-5.6 family, gpt-5.5-pro, gpt-5.4-pro | **Native**: sends `reasoning.mode: "pro"` (verified live; all other OpenAI models 400 on the field) |
| OpenAI other models | Emulated: one-tier effort bump (`low→medium→high→xhigh`) |
| Anthropic (all) | Emulated: one-tier effort bump (`low→medium→high→xhigh→max`) |
| Gemini (all) | Emulated: one-tier bump within the 4-rung enum (ceiling `high`) |
| xAI effort-controlled models | Emulated: one-tier bump (ceiling `xhigh`) |
| xAI grok-4.20 family | No effect (name-locked, see above) |

Rules that hold everywhere:
- `mode: "standard"` is the default and is **never sent on the wire**.
- An explicit `effort: "none"` always wins over `pro` — off means off.
- `pro` **without** an effort: OpenAI native-mode models get `{mode: "pro"}`
  with the model choosing its own effort; every other model behaves as
  `medium` bumped one tier (i.e. `high`).

## Precedence

1. Native passthrough (`x-<provider>.*`, top-level `thinking`/`output_config`
   for Anthropic) — always wins, never rewritten.
2. Unified `reasoning_effort` + `reasoning_mode` — translated + clamped per
   the tables above.
3. Neither set — the provider/model default applies (note Anthropic adaptive
   models and gemini-3.1-pro think by default; most others don't).

## Provenance

Every non-obvious cell above was verified against the live provider APIs
(HTTP 400-vs-200 boundary probes) as of 2026-07. Where a provider adds tiers
or models later, extend the clamp tables in the provider transforms
(`src/providers/*.rs`) and update this document — the unit tests in
`tests/unit_{openai,anthropic,gemini,xai}.rs` pin each mapping.

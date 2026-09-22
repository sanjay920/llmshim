# Request field map

Rust and the proxy express the same request semantics through different
envelopes. Rust uses an OpenAI-shaped `serde_json::Value`; the proxy groups its
portable subset under `config` and everything else under `provider_config`.

> **Availability:** Rust: flexible top-level JSON · Proxy/clients: typed compact envelope

## Rust engine request

These are the stable, adapter-recognized fields. A field marked “mapped” is
renamed or reshaped for the selected provider; unsupported controls are omitted.

| Field | Type | Class | Behavior |
|---|---|---|---|
| `model` | string | Routing | Required; explicit `provider/model` or a recognized bare prefix |
| `messages` | array | Portable | Required conversation history; translated to native message/content forms |
| `stream` | boolean | Portable | Set to `true` by `llmshim::stream`; selects native streaming |
| `max_tokens` | integer | Portable, mapped | Native output limit; `max_completion_tokens` is accepted as an alias |
| `temperature` | number | Conditional | Sent to Anthropic and Gemini; not copied by the OpenAI/xAI Responses adapters |
| `top_p` | number | Conditional | Sent to Anthropic and Gemini |
| `top_k` | integer | Conditional | Sent to Anthropic and Gemini |
| `stop` | array of strings | Conditional, mapped | Passed as `stop` to Anthropic or mapped to Gemini `stopSequences`; omitted by OpenAI/xAI |
| `reasoning_effort` | string | Portable, mapped/clamped | Unified effort from `none` through `max` |
| `reasoning_mode` | string | Portable, mapped/clamped | `standard` or `pro` |
| `reasoning_summary` | string | Anthropic (`auto`\|`none`) | Reasoning visibility → Anthropic `thinking.display`; `auto`→`summarized` (default with `reasoning_effort`), `none`→`omitted` for lower latency |
| `tools` | array | Portable, mapped | OpenAI Chat Completions function schema translated to native tools |
| `tool_choice` | string or object | Portable, mapped | Translated where the provider supports tool choice |
| `x-openai` | object | Native passthrough | Fields copied to an OpenAI Responses request |
| `x-anthropic` | object | Native passthrough/control | Anthropic body fields plus `extra_betas` and `disable_1m_context` controls |
| `x-gemini` | object | Native passthrough | Gemini body fields; `thinkingConfig` goes under `generationConfig` |
| `x-openrouter` | object | Native passthrough/control | OpenRouter body fields (`provider`, `models`, `transforms`, `route`, native `reasoning`); `http_referer`/`x_title` become request headers |
| `x-vllm` / `x-sglang` | object | Native passthrough | Server-specific body params for a self-hosted vLLM/SGLang server (`chat_template_kwargs`, `separate_reasoning`, `guided_json`, `top_k`, `min_p`, …) |

There is no `x-xai` namespace. OpenAI additionally recognizes `store`,
`prompt_cache_key`, `prompt_cache_retention`, `safety_identifier`, and `speed`.
Anthropic recognizes `cache_control`, `thinking`, `output_config`, and `speed`.
Those fields are provider-specific even when they are not wrapped in an `x-*`
namespace.

The request is a `Value`, so Rust does not reject unknown keys up front.
Adapters deliberately select only fields they understand. Do not assume an
arbitrary top-level key reaches the provider; use a documented native namespace.

Reasoning values and family-specific clamps are canonicalized in
[Reasoning controls](../guides/reasoning.md). Native namespaces and precedence
are covered in [Native provider controls](../guides/native-controls.md).

## Message fields

| Field | Type | Purpose |
|---|---|---|
| `role` | string | `system`, `developer`, `user`, `assistant`, or `tool` semantics |
| `content` | string, array, or null | Text and multimodal content blocks |
| `tool_calls` | array | Complete assistant tool requests, including owned `id`, `wire_ids`, and any signature object |
| `tool_call_id` | string | Connects a `role: "tool"` result to its request |
| `reasoning` | array | Ordered typed blocks with origin, text or opaque data, signature/item identity, and original structured payload when needed |
| `thought_signature` (on a tool call) | object | Opaque `data` plus `origin`; retain the whole object |
| Legacy reasoning siblings | strings | Migration input only; untracked data is dropped |

Content blocks may use OpenAI `image_url`, Anthropic `image`, or Gemini
`inline_data` input forms. See [Images and vision](../guides/images.md).

### Lossless reasoning round-trip

Preserve the complete assistant message, including `reasoning[]` and tool-call
signature objects. llmshim reconstructs native reasoning only when the stored
origin matches the target family and wire; encrypted data also requires a
matching issuer binding. Anthropic thinking remains first and ordered, and
Responses items retain their encrypted payloads. Unknown origins/families are
never replayed. See [reasoning replay](../guides/reasoning.md#preserve-reasoning-for-replay).

## Proxy request

| Field | Type | Class | Engine destination |
|---|---|---|---|
| `model` | string | Routing | top-level `model` |
| `messages` | array | Portable | top-level `messages` |
| `stream` | boolean | Transport | selects SSE only on `/v1/chat` |
| `config` | object | Portable | recognized children become top-level engine controls |
| `provider_config` | object | Passthrough container | each non-reserved child is merged into the engine request |
| `fallback` | array of strings | Proxy orchestration | ordered non-streaming backup routes; not sent to a provider |
| `x-cache` | object | Caller stability annotations | native breakpoints or `prompt_cache_key`; never forwarded verbatim |

The exact `config` children are:

| Child | Type | Engine field |
|---|---|---|
| `max_tokens` | unsigned integer | `max_tokens` |
| `temperature` | number | `temperature` |
| `top_p` | number | `top_p` |
| `top_k` | unsigned integer | `top_k` |
| `stop` | array of strings | `stop` |
| `reasoning_effort` | string | `reasoning_effort` |
| `reasoning_mode` | string | `reasoning_mode` |

For example, this proxy fragment:

```json
{
  "config": {"reasoning_effort": "high"},
  "provider_config": {
    "tools": [],
    "x-anthropic": {"disable_1m_context": true}
  }
}
```

becomes engine fields named `reasoning_effort`, `tools`, and `x-anthropic`.
Because `provider_config` is merged after `config`, a same-named child there
overrides the portable value. Prefer the `x-*` namespace for an intentional
native override; it makes that loss of portability visible.

`model` and `messages` are reserved at the `provider_config` root. Native
namespaces may not replace their wire equivalents (`model`, `messages`,
`input`, or `contents`). The proxy rejects those conflicts so routing, spend
checks, prompt validation, and dispatch all refer to the same request. Tool
definitions, schemas, output limits, and other documented native controls
remain valid passthrough fields.

## Output contracts

`response_format: {type:"json_schema", json_schema:{name?, schema, strict?}}`
requests validated JSON output. `x-shim` controls `structured_output`
(`auto|native|forced_tool|prompt`), `tool_calling` (`auto|native|prompt`), and
`reasoning_capture` (`off|forced_tool`). See [capability shims](../guides/capabilities.md)
for path selection, bounded repair, streaming behavior, and provenance.

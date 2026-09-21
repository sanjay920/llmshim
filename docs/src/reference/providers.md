# Provider behavior matrix

Every adapter accepts the same OpenAI-shaped engine request, but it targets a
different native API and translates only the fields that API understands.

| Provider | Native API | Bare-model inference | Native namespace |
|---|---|---|---|
| OpenAI | Responses API | `gpt*`, `o1*`, `o3*`, `o4*` | `x-openai` |
| ChatGPT subscription | Responses API over OAuth | none — use `chatgpt/<model>` | `x-chatgpt` |
| Anthropic | Messages API | `claude*` | `x-anthropic` |
| Google Gemini | `generateContent` / `streamGenerateContent` | `gemini*` | `x-gemini` |
| xAI | Responses API | `grok*` | none |
| OpenRouter | Chat Completions (aggregator) | none — address as `openrouter/<vendor>/<model>` | `x-openrouter` |
| vLLM | Chat Completions (self-hosted, `VLLM_BASE_URL`); Responses API with `VLLM_WIRE=responses` | none — address as `vllm/<served-model>` | `x-vllm` |
| SGLang | Chat Completions (self-hosted, `SGLANG_BASE_URL`); Responses API with `SGLANG_WIRE=responses` | none — address as `sglang/<served-model>` | `x-sglang` |

An explicit address such as `anthropic/claude-sonnet-5` avoids inference.
The named provider must be registered in the Router—that normally means its
API key is configured.

ChatGPT uses a saved device-code login instead: `llmshim login chatgpt`.
Both normal and streaming calls use upstream SSE. Normal calls aggregate a
terminal response into Chat Completions JSON; a failed or truncated stream
returns an error. Tools, image inputs, and reasoning use the Responses
translator. ChatGPT forces `store: false` and `stream: true` and strips token
limits, metadata, and sampling fields even from native overrides, following
[LiteLLM's backend contract](https://docs.litellm.ai/docs/providers/chatgpt).
It uses a short default instruction when no instructions are supplied.
Only `chatgpt/gpt-6-astra` and `chatgpt/gpt-5.6-{sol,terra,luna}` are accepted;
older and unlisted models fail locally before authentication or network calls.
Access to those models and limits depend on the signed-in account.
ChatGPT responses preserve the full `chatgpt/<model>` ID, including streaming
chunks, so server responses identify the subscription provider correctly when
API-key OpenAI is also configured.
Streamed function calls are emitted once their arguments are complete. Each
call includes its ID, name, and full JSON arguments, including in the proxy's
`tool_call` event; text and reasoning still arrive incrementally.

## Observable differences

| Provider | Messages and tools | Notable behavior |
|---|---|---|
| OpenAI | System/developer text becomes Responses `instructions`; Chat Completions tool definitions are flattened for Responses | `store` defaults to `false`; unified reasoning becomes the native `reasoning` object; `x-openai.reasoning` overrides that mapping |
| Anthropic | Messages become Anthropic content blocks; tools use `input_schema`, `tool_use`, and `tool_result` | `max_tokens` defaults to 8192 when absent; supported models receive the 1M-context beta by default; `x-anthropic.extra_betas` appends beta headers and `disable_1m_context` suppresses that automatic header |
| Gemini | Messages become `contents`; tools use `functionDeclarations`, `functionCall`, and `functionResponse` | Base64 images become `inline_data`, but a remote image URL becomes a text placeholder because Gemini cannot consume it directly; `x-gemini.thinkingConfig` replaces mapped thinking configuration |
| xAI | System/developer text becomes Responses `instructions`; tools are flattened like OpenAI Responses | Unified reasoning becomes `reasoning: {effort}` where the model accepts it; Grok 4.6 clamps `none` to `low` and `max` to `xhigh`; there is no `x-xai` namespace |
| OpenRouter | Passthrough — messages, tools, `image_url` vision, and `response_format` are already Chat Completions and forwarded unchanged | `reasoning_effort` maps 1:1 to OpenRouter's `reasoning:{effort}` (superset vocabulary, no clamping); reasoning is normalized to provenance-bearing blocks; the `middle-out` transform is disabled by default; `x-openrouter` carries `provider`/`models`/`transforms`/`route`/native `reasoning` (and `http_referer`/`x_title` headers) |
| vLLM / SGLang | Passthrough to a self-hosted server — configured by base URL (local or remote), auth optional | reasoning normalized to provenance-bearing blocks; `reasoning_effort` forwarded (honored per-model); server-specific params (`chat_template_kwargs`, `guided_json`, `top_k`, `separate_reasoning`, …) go under `x-vllm`/`x-sglang`. Reasoning/tool parsing depend on the server's launch flags (`--reasoning-parser`, `--tool-call-parser`) |

OpenAI and xAI do not receive `temperature`, `top_p`, `top_k`, or `stop` from
the portable top-level request. Anthropic and Gemini do. This is the
portable-core rule in practice: unsupported fields are omitted rather than
forwarded blindly.

## Tools and multimodal round trips

### Claude Fable 5.1

Fable 5.1 uses always-on adaptive thinking, with `low`, `medium`,
`high`, `xhigh`, and `max` effort. Unified `none` maps to `low`; explicit
native disabled/manual thinking and assistant prefill return local errors.
Sampling parameters are omitted for Fable 5.1 and Opus 5 because
those models reject them.

Fable 5.1 accepts only `auto` and `none`:
`required` or a named forced tool returns a local 400 rather than changing the
request's meaning. Describe the desired tool in the prompt, or use structured
output when the requirement is a JSON schema. Native `stop_reason: "refusal"`
maps to `finish_reason: "content_filter"` for Fable and Opus 5.

Fable 5.1 binds thinking blocks to the preceding system prompt, tools, and
conversation. Keep that prefix unchanged when replaying signed reasoning.
Appending turns is supported; llmshim preserves appended system/developer
instructions as system turns for Fable 5.1. If your application edits or
compacts prior history, remove stale thinking or use Anthropic's explicit
binding controls under `x-anthropic`. A binding failure remains an error.
See [Anthropic's migration and history rules](https://platform.claude.com/docs/en/models/fable-5-1/migration-guide).

The raw Rust response exposes normalized thinking signatures for replay.
The compact proxy response does not expose every native thinking field;
applications needing native history controls can supply exact messages under
`provider_config.messages`.

### Shared message formats

Tool calls are normalized back to the OpenAI `tool_calls` shape. Preserve the
complete assistant tool call when sending the next turn; provider adapters may
need fields beyond function name and arguments.

Gemini tool signatures carry `data` and `origin`; all typed clients expose this
object. Reasoning is replayed only to a compatible family and wire, and encrypted
blocks also require a matching account binding.

Image input accepts OpenAI `image_url`, Anthropic `image`, and Gemini
`inline_data` blocks. Base64 data URIs can be translated among providers.
Gemini's remote-URL fallback is literal text such as `[Image: URL]`; llmshim
does not download the URL.

## Reasoning and native controls

Unified `reasoning_effort` and `reasoning_mode` are mapped by model family, not
merely by provider. Floors, ceilings, and name-locked models are listed in the
[reasoning tables](../guides/reasoning.md).

Use [native provider controls](../guides/native-controls.md) when exact native
semantics matter. Native objects are not rewritten or made portable. Changing
the provider address means reconsidering the namespace as well.

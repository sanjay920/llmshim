# Provider behavior matrix

Every adapter accepts the same OpenAI-shaped engine request, but it targets a
different native API and translates only the fields that API understands.

| Provider | Native API | Bare-model inference | Native namespace |
|---|---|---|---|
| OpenAI | Responses API | `gpt*`, `o1*`, `o3*`, `o4*` | `x-openai` |
| Anthropic | Messages API | `claude*` | `x-anthropic` |
| Google Gemini | `generateContent` / `streamGenerateContent` | `gemini*` | `x-gemini` |
| xAI | Responses API | `grok*` | none |
| OpenRouter | Chat Completions (aggregator) | none — address as `openrouter/<vendor>/<model>` | `x-openrouter` |
| vLLM | Chat Completions (self-hosted, `VLLM_BASE_URL`) | none — address as `vllm/<served-model>` | `x-vllm` |
| SGLang | Chat Completions (self-hosted, `SGLANG_BASE_URL`) | none — address as `sglang/<served-model>` | `x-sglang` |

An explicit address such as `anthropic/claude-sonnet-5` avoids inference.
The named provider must be registered in the Router—that normally means its
API key is configured.

## Observable differences

| Provider | Messages and tools | Notable behavior |
|---|---|---|
| OpenAI | System/developer text becomes Responses `instructions`; Chat Completions tool definitions are flattened for Responses | `store` defaults to `false`; unified reasoning becomes the native `reasoning` object; `x-openai.reasoning` overrides that mapping |
| Anthropic | Messages become Anthropic content blocks; tools use `input_schema`, `tool_use`, and `tool_result` | `max_tokens` defaults to 8192 when absent; supported models receive the 1M-context beta by default; `x-anthropic.extra_betas` appends beta headers and `disable_1m_context` suppresses that automatic header |
| Gemini | Messages become `contents`; tools use `functionDeclarations`, `functionCall`, and `functionResponse` | Base64 images become `inline_data`, but a remote image URL becomes a text placeholder because Gemini cannot consume it directly; `x-gemini.thinkingConfig` replaces mapped thinking configuration |
| xAI | System/developer text becomes Responses `instructions`; tools are flattened like OpenAI Responses | Unified reasoning becomes `reasoning: {effort}` where the model accepts it; grok-4.20 reasoning is encoded in the model name; there is no `x-xai` namespace |
| OpenRouter | Passthrough — messages, tools, `image_url` vision, and `response_format` are already Chat Completions and forwarded unchanged | `reasoning_effort` maps 1:1 to OpenRouter's `reasoning:{effort}` (superset vocabulary, no clamping); `message.reasoning` is normalized to `reasoning_content`; the `middle-out` transform is disabled by default; `x-openrouter` carries `provider`/`models`/`transforms`/`route`/native `reasoning` (and `http_referer`/`x_title` headers) |
| vLLM / SGLang | Passthrough to a self-hosted server — configured by base URL (local or remote), auth optional | `reasoning`/`reasoning_content` normalized to `reasoning_content`; `reasoning_effort` forwarded (honored per-model); server-specific params (`chat_template_kwargs`, `guided_json`, `top_k`, `separate_reasoning`, …) go under `x-vllm`/`x-sglang`. Reasoning/tool parsing depend on the server's launch flags (`--reasoning-parser`, `--tool-call-parser`) |

OpenAI and xAI do not receive `temperature`, `top_p`, `top_k`, or `stop` from
the portable top-level request. Anthropic and Gemini do. This is the
portable-core rule in practice: unsupported fields are omitted rather than
forwarded blindly.

## Tools and multimodal round trips

Tool calls are normalized back to the OpenAI `tool_calls` shape. Preserve the
complete assistant tool call when sending the next turn; provider adapters may
need fields beyond function name and arguments.

In particular, raw proxy JSON round-trips Gemini `thought_signature`, but the
Go and Ruby typed structs do not currently surface it. Use raw JSON if you need
that field.

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

# HTTP API

The proxy wraps llmshim's translation engine in a compact JSON API. It has
four endpoints and two response encodings.

> **Availability:** HTTP JSON: non-streaming chat, models, health · HTTP SSE: streaming chat

## Endpoints

| Method | Path | Result |
|---|---|---|
| `POST` | `/v1/chat` | A `ChatResponse`, or typed SSE when `stream` is `true` |
| `POST` | `/v1/chat/stream` | Typed SSE, regardless of the request's `stream` value |
| `GET` | `/v1/models` | Models whose providers have configured API keys |
| `GET` | `/health` | Process health and configured provider names |

The canonical machine-readable contract is
[`api/openapi.yaml`](https://github.com/sanjay920/llmshim/blob/main/api/openapi.yaml).

## Chat request

Both chat endpoints accept the same body:

```json
{
  "model": "anthropic/claude-sonnet-5",
  "messages": [{"role": "user", "content": "Explain ownership briefly."}],
  "stream": false,
  "config": {
    "max_tokens": 300,
    "temperature": 0.2,
    "reasoning_effort": "medium"
  },
  "provider_config": {},
  "fallback": ["openai/gpt-5.4-mini"]
}
```

| Field | Type | Meaning |
|---|---|---|
| `model` | string, required | `provider/model` or an inferable bare model name |
| `messages` | array, required | Full conversation history |
| `stream` | boolean | On `/v1/chat`, switch the response from JSON to typed SSE; default `false` |
| `config` | object | Portable generation controls |
| `provider_config` | object | Fields merged into the engine request, including tools and `x-*` namespaces |
| `fallback` | string array | Ordered backup models for non-streaming requests only |

`config` accepts exactly these fields:

| Field | Type |
|---|---|
| `max_tokens` | unsigned integer |
| `temperature` | number |
| `top_p` | number |
| `top_k` | unsigned integer |
| `stop` | array of strings |
| `reasoning_effort` | `none`, `low`, `medium`, `high`, `xhigh`, or `max` |
| `reasoning_mode` | `standard` or `pro` |

Adapters use only controls supported by the selected provider. Reasoning is
mapped and clamped by model family; see [Reasoning controls](../guides/reasoning.md).

Messages require `role`; `content` defaults to `null` and may be text or
content blocks. Tool turns may also carry `tool_calls` or `tool_call_id`.
`reasoning_content` is accepted on a message when prior reasoning must be
round-tripped.

`provider_config` is merged as top-level engine fields. For example, tools go
at `provider_config.tools`, while an OpenAI-native override goes at
`provider_config["x-openai"]`. See the [request field map](../reference/request-fields.md).

Fallback first retries an eligible failure on the current route, then moves
through the listed routes. It is ignored by both streaming paths. See
[Fallback chains](../guides/fallbacks.md).

## Non-streaming response

`POST /v1/chat` returns a compact response when `stream` is false:

```json
{
  "id": "msg_123",
  "model": "claude-sonnet-5",
  "provider": "anthropic",
  "message": {
    "role": "assistant",
    "content": "Ownership gives each value one owner."
  },
  "usage": {
    "input_tokens": 14,
    "output_tokens": 9,
    "reasoning_tokens": 4,
    "total_tokens": 27
  },
  "latency_ms": 612
}
```

`message.tool_calls` appears when the model requests tools. `reasoning` appears
when the provider returns reasoning text. `reasoning_tokens` is omitted when
zero; the other usage fields are always present.

## Streaming response

`POST /v1/chat/stream` always streams. `POST /v1/chat` does the same when the
body contains `"stream": true`.

Each Server-Sent Event has an SSE `event:` name and JSON `data:` whose `type`
matches that name:

```text
event: reasoning
data: {"type":"reasoning","text":"..."}

event: content
data: {"type":"content","text":"Ownership gives each value one owner."}

event: tool_call
data: {"type":"tool_call","id":"call_1","name":"lookup","arguments":"{\"id\":7}"}

event: usage
data: {"type":"usage","input_tokens":14,"output_tokens":9,"total_tokens":23}

event: done
data: {"type":"done"}
```

| Event | JSON fields |
|---|---|
| `content` | `type`, `text` |
| `reasoning` | `type`, `text` |
| `tool_call` | `type`, `id`, `name`, `arguments` |
| `usage` | `type`, `input_tokens`, `output_tokens`, optional `reasoning_tokens`, `total_tokens` |
| `done` | `type` |
| `error` | `type`, `message` |

Admission failures happen before SSE begins and return normal HTTP `429` or
`503` responses. A failure after the stream starts is an `error` event. For
consumption patterns, see [Streaming](../guides/streaming.md).

## Models and health

`GET /v1/models` returns the registry entries for configured providers:

```json
{
  "models": [
    {"id": "openai/gpt-5.4-mini", "provider": "openai", "name": "gpt-5.4-mini"}
  ]
}
```

This is discovery, not an allowlist: an arbitrary provider model ID can still
be routed explicitly. See [Model discovery](../reference/models.md).

`GET /health` reports that the process can serve requests and names its
configured providers:

```json
{"status":"ok","providers":["openai","anthropic"]}
```

It does not probe upstream provider availability.

## Errors

Before a stream begins, errors use an HTTP status and this JSON envelope:

```json
{"error":{"code":"rate_limited","message":"Upstream provider rate limit reached; retry after the suggested delay"}}
```

Proactive `429` and `503` responses include `Retry-After` in whole seconds.
Provider failures normally retain the provider's status. See
[Errors and retries](../reference/errors.md) for the complete mapping.

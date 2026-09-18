# Tool use

llmshim translates tool definitions, tool calls, and tool results. It never
executes a tool. Your application owns that part of the loop.

> **Availability:** Rust: top-level `tools` · CLI: no tool loop · Proxy/clients: `provider_config.tools`

The loop is always:

```text
define tools → receive tool_calls → execute in your app → send tool results → continue
```

## 1. Define tools

Use the OpenAI Chat Completions function format. The `function` object is
nested inside the tool definition:

```json
{
  "type": "function",
  "function": {
    "name": "get_weather",
    "description": "Get the current weather for a city",
    "parameters": {
      "type": "object",
      "properties": {
        "city": {"type": "string"}
      },
      "required": ["city"]
    }
  }
}
```

In a Rust request, `tools` is a top-level field:

```json
{
  "model": "anthropic/claude-sonnet-5",
  "messages": [{"role": "user", "content": "What is the weather in Tokyo?"}],
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Get the current weather for a city",
        "parameters": {
          "type": "object",
          "properties": {"city": {"type": "string"}},
          "required": ["city"]
        }
      }
    }
  ]
}
```

Through the proxy, place the identical array at `provider_config.tools`:

```json
{
  "model": "anthropic/claude-sonnet-5",
  "messages": [{"role": "user", "content": "What is the weather in Tokyo?"}],
  "provider_config": {
    "tools": [
      {
        "type": "function",
        "function": {
          "name": "get_weather",
          "description": "Get the current weather for a city",
          "parameters": {
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
          }
        }
      }
    ]
  }
}
```

Python and Ruby expose convenience `tools` arguments that build this
`provider_config` field. TypeScript and Go expose `provider_config` directly.

## 2. Receive a tool call

llmshim normalizes a provider's request to call a function into the OpenAI
shape:

```json
{
  "role": "assistant",
  "content": null,
  "tool_calls": [
    {
      "id": "call_ls_example",
      "wire_ids": [{"provider":"openai","wire":"openai-responses","scope":"response_example","part_id":"0","id":"call_provider_123","item_id":"fc_example"}],
      "type": "function",
      "function": {
        "name": "get_weather",
        "arguments": "{\"city\":\"Tokyo\"}"
      }
    }
  ]
}
```

`arguments` is a JSON-encoded string. Parse and validate it before calling your
application code. Treat model-generated arguments as untrusted input.

## 3. Execute it in your application

Dispatch `get_weather` in your own code. llmshim does not have access to that
function and does not decide whether it is safe to run.

Keep the returned assistant message in the conversation, including its
`tool_calls`. Preserve any additional fields on those calls; for example,
Gemini can return a `thought_signature` object with `data` and `origin`.
Every generated call also has `wire_ids`; retain that complete array. Then append one tool-result message for each call:

```json
[
  {
    "role": "assistant",
    "content": null,
    "tool_calls": [
      {
        "id": "call_ls_example",
      "wire_ids": [{"provider":"openai","wire":"openai-responses","scope":"response_example","part_id":"0","id":"call_provider_123","item_id":"fc_example"}],
        "type": "function",
        "function": {
          "name": "get_weather",
          "arguments": "{\"city\":\"Tokyo\"}"
        }
      }
    ]
  },
  {
    "role": "tool",
    "tool_call_id": "call_ls_example",
    "content": "{\"temperature_c\":24,\"conditions\":\"clear\"}"
  }
]
```

The `tool_call_id` connects the result to the request. Send the expanded
history and tool definitions in another completion so the model can use the
result and produce an answer.

## Identity and validation

llmshim mints each canonical `id`. `wire_ids` keeps the native correlation id
and, for Responses, its separate item id. Reply with the canonical id and keep
the original assistant call unchanged; the adapter translates both sides back
to the same wire identity. Switching providers creates a deterministic target
wire mapping without changing stored history. Dropping the mapping from an
owned call is a local error.

All adapters validate that each tool call is answered exactly once before the
next assistant turn. Unknown results, unanswered calls, duplicate owned ids,
and invalid argument JSON fail before the provider request. Anthropic thinking
blocks must precede text/tools. Invalid history is reported, rather than silently
removing calls or results.

Gemini preserves whether the original function call had an id. When it did,
its function response gets the same id. Otherwise result ordering preserves
correlation, including repeated calls to the same function. The first call in
a current Gemini 3 parallel batch carries the required signature; later calls
retain their original signature presence/absence. See
[Google's signature contract](https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures).
A compacted tool history must retain the user turn that introduced its calls.

## Streaming calls

`llmshim::stream` uses one per-response assembler for every transport, including
ChatGPT. Argument fragments, ids, and signatures accumulate under stable part
ids. Completed calls appear once after terminal metadata, with full JSON
arguments and their wire mappings; incomplete JSON never becomes a callable
record. This applies to typed proxy `tool_call` events too.

For manually read SSE, create `provider.stream_normalizer(model)` and feed
native data strings to `push`. Call `finish` on EOF. The lower-level stateless
`transform_stream_chunk` only handles ordinary content/reasoning; it cannot
assemble tool calls across events. Rust chunks retain separate choices; the
single-message proxy contract projects choice zero.

## What llmshim translates

| Provider | Native representation |
|---|---|
| OpenAI Responses | Flat function definitions, `function_call`, and `function_call_output` items |
| Anthropic Messages | `input_schema`, `tool_use`, and `tool_result` blocks |
| Gemini | `functionDeclarations`, `functionCall`, and `functionResponse` parts |
| xAI Responses | Flat function definitions and Responses-style call/result items |

Responses and stream chunks are translated back to the OpenAI `tool_calls`
shape before they cross the Rust boundary. The proxy then exposes the same
call as `message.tool_calls` or a typed `tool_call` stream event.

For the broader rule behind the surface-specific placement, see
[Two contracts, one engine](../concepts/contracts.md#where-this-goes).

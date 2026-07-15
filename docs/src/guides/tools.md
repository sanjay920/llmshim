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
  "model": "anthropic/claude-sonnet-4-6",
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
  "model": "anthropic/claude-sonnet-4-6",
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
      "id": "call_123",
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
Gemini can return a `thought_signature` that its adapter needs on the next
turn. Then append one tool-result message for each call:

```json
[
  {
    "role": "assistant",
    "content": null,
    "tool_calls": [
      {
        "id": "call_123",
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
    "tool_call_id": "call_123",
    "content": "{\"temperature_c\":24,\"conditions\":\"clear\"}"
  }
]
```

The `tool_call_id` connects the result to the request. Send the expanded
history and tool definitions in another completion so the model can use the
result and produce an answer.

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

# HTTP proxy quickstart

The proxy puts a network boundary around the Rust engine. Use it from any
language that can send JSON over HTTP.

> **Availability:** Rust: engine inside the server · CLI: starts the server · HTTP: compact llmshim API · Clients: connect to this API

The proxy has its own compact contract. It is not an OpenAI-compatible proxy.

## 1. Install a proxy-enabled binary

The proxy is behind the crate's `proxy` feature when building from source:

```bash
cargo install llmshim --features proxy
```

The Homebrew binary also includes the proxy:

```bash
brew install sanjay920/tap/llmshim
```

Configure at least one provider before starting it:

```bash
llmshim configure
```

## 2. Start the server

```bash
llmshim proxy
```

The default bind address is `0.0.0.0:3000`. Override it for the process with
`LLMSHIM_HOST` and `LLMSHIM_PORT`:

```bash
LLMSHIM_HOST=127.0.0.1 LLMSHIM_PORT=8080 llmshim proxy
```

The examples below assume the default port and use `localhost` to reach it.

## 3. Send a chat request

```bash
curl http://localhost:3000/v1/chat \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "openai/gpt-5.6-sol",
    "messages": [
      {"role": "user", "content": "What is Rust in one sentence?"}
    ],
    "config": {
      "max_tokens": 128
    },
    "provider_config": {
      "x-anthropic": {
        "disable_1m_context": true
      }
    }
  }'
```

`config` contains portable controls. `provider_config` is optional and merges
provider-specific fields into the core request; omit it when you do not need a
native control.

The non-streaming response uses llmshim's compact `ChatResponse`:

```json
{
  "id": "msg_...",
  "model": "gpt-5.6-sol",
  "provider": "openai",
  "message": {
    "role": "assistant",
    "content": "Rust is a systems programming language..."
  },
  "usage": {
    "input_tokens": 15,
    "output_tokens": 12,
    "total_tokens": 27
  },
  "latency_ms": 640
}
```

IDs, text, token counts, and timing vary by request. `reasoning` and
`message.tool_calls` appear only when the provider returns them.

Set top-level `"stream": true` on the same request to receive typed SSE events
instead of a JSON response. `POST /v1/chat/stream` is the always-streaming
equivalent.

See the [HTTP API](../proxy/http-api.md) for the full contract.

## Do not expose the bare proxy publicly

The server has permissive CORS and provides no authentication or TLS. For local
use, bind it to `127.0.0.1`. For a public deployment, put it behind an external
gateway that supplies authentication and TLS. The supported topology and
deployment checklist are covered in
[Deploy the proxy safely](../proxy/deployment.md).


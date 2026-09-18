# Streaming

Streaming exposes the same kinds of incremental output on every surface, but
the Rust crate and the proxy encode them differently.

> **Availability:** Rust: normalized JSON chunks · CLI: rendered live · Proxy: typed SSE · Clients: language-native stream iterators

The useful distinction is: **same semantic events, two encodings.**

| Signal | Rust chunk | Proxy/client event |
|---|---|---|
| Answer text | `choices[0].delta.content` | `content` |
| Provider-returned reasoning | `choices[0].delta.reasoning` | `reasoning` |
| Tool call | `choices[0].delta.tool_calls` | `tool_call` |
| Token counts | top-level `usage` | `usage` |
| Completion | `finish_reason` | `done` |
| Stream failure | Rust `Err` | `error` |

## Rust: normalized Chat Completions chunks

`llmshim::stream` returns a stream whose items are JSON strings. Each provider
adapter and the shared per-response normalizer have translated native events
into Chat Completions-style deltas. Tool calls are complete records emitted once
at termination, after their argument and signature fragments have been assembled.
Retain each call's `wire_ids` when storing it or sending its result back.

```rust
use futures::StreamExt;
use std::io::{self, Write};

let mut stream = llmshim::stream(&router, &request).await?;
let mut reasoning = llmshim::reasoning::ReasoningAccumulator::default();

while let Some(chunk) = stream.next().await {
    let chunk = chunk?;
    let parsed: serde_json::Value = serde_json::from_str(&chunk)?;

    let delta = &parsed["choices"][0]["delta"];
    reasoning.push(delta);
    eprint!("{}", llmshim::reasoning::reasoning_text(delta));

    if let Some(text) = parsed
        .pointer("/choices/0/delta/content")
        .and_then(|value| value.as_str())
    {
        print!("{text}");
        io::stdout().flush()?;
    }
}
```

Not every chunk contains text. Inspect only the fields your application needs,
and keep handling `Err` items until the stream ends. Store `reasoning.blocks()`
with the final assistant message only after successful completion. An EOF or
`[DONE]` without a terminal provider chunk is an error. Cache usage events are
cumulative snapshots; use the latest values, rather than summing events.

The [Rust quickstart](../start/rust.md#3-stream-content) contains a complete
runnable program.

## Proxy: typed SSE events

Send the compact request to the always-streaming endpoint:

```bash
curl -N http://localhost:3000/v1/chat/stream \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "anthropic/claude-sonnet-5",
    "messages": [{"role": "user", "content": "Write a haiku."}],
    "config": {"max_tokens": 128}
  }'
```

The response is an SSE stream. The SSE `event` name and the JSON `type` agree:

```text
event: reasoning
data: {"type":"reasoning","text":"..."}

event: content
data: {"type":"content","text":"Rust shapes silent thought"}

event: usage
data: {"type":"usage","input_tokens":12,"output_tokens":7,"total_tokens":19}

event: done
data: {"type":"done"}
```

The six event types are:

| Type | Payload |
|---|---|
| `content` | `text` |
| `reasoning` | `text` |
| `tool_call` | `id`, `name`, JSON-encoded `arguments` |
| `usage` | input, output, optional reasoning, and total token counts |
| `done` | no additional fields |
| `error` | `message` |

Setting `"stream": true` on `POST /v1/chat` produces the same SSE encoding.
An admission error can still arrive as an HTTP error before streaming begins;
an error after the stream begins arrives as an `error` event.

## Client ergonomics

The language clients decode the typed SSE stream without changing its event
vocabulary:

- Python returns an iterator of event dictionaries.
- TypeScript returns an async iterator of typed events.
- Go returns a channel of typed events.
- Ruby yields typed events to a block.

See the canonical client guides for complete loops:
[Python](https://github.com/sanjay920/llmshim/blob/main/clients/python/README.md#streaming),
[TypeScript](https://github.com/sanjay920/llmshim/blob/main/clients/typescript/README.md#30-second-quickstart),
[Go](https://github.com/sanjay920/llmshim/blob/main/clients/go/README.md#streaming), and
[Ruby](https://github.com/sanjay920/llmshim/blob/main/clients/ruby/README.md#streaming).


# Rust quickstart

Use the crate when the application making LLM requests is already written in
Rust. The translation engine runs in-process; there is no proxy to start.

> **Availability:** Rust: direct engine API · CLI/HTTP/clients: not used in this path

## 1. Add the dependencies

```bash
cargo add llmshim tokio serde_json
```

`tokio` and `serde_json` are direct dependencies of your application. llmshim
does not re-export them.

Set at least one provider key before running the program:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

## 2. Send a completion

```rust
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let router = llmshim::router::Router::from_env();

    // The public request contract is a serde_json::Value.
    let request = json!({
        "model": "openai/gpt-5.6-sol",
        "messages": [
            {"role": "user", "content": "What is Rust in one sentence?"}
        ],
        "max_tokens": 128
    });

    let response = llmshim::completion(&router, &request).await?;
    let text = response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");

    println!("{text}");
    Ok(())
}
```

Run it with `cargo run`. `Router::from_env()` registers the providers whose
environment variables are present. Change only the `model` address to route
the same conversation elsewhere.

The result uses an OpenAI Chat Completions-style shape, regardless of which
provider answered. The assistant text is at
`response["choices"][0]["message"]["content"]`.

## 3. Stream content

Streaming uses the `StreamExt` trait, so add `futures` as a direct dependency:

```bash
cargo add futures
```

This complete example prints text deltas as they arrive:

```rust
use futures::StreamExt;
use serde_json::json;
use std::io::{self, Write};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let router = llmshim::router::Router::from_env();
    let request = json!({
        "model": "anthropic/claude-sonnet-5",
        "messages": [
            {"role": "user", "content": "Write a haiku about Rust."}
        ],
        "max_tokens": 128
    });

    let mut stream = llmshim::stream(&router, &request).await?;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let parsed: serde_json::Value = serde_json::from_str(&chunk)?;

        if let Some(text) = parsed
            .pointer("/choices/0/delta/content")
            .and_then(|value| value.as_str())
        {
            print!("{text}");
            io::stdout().flush()?;
        }
    }

    println!();
    Ok(())
}
```

Each item is a JSON string in the normalized Chat Completions delta shape. A
chunk can carry something other than text, such as reasoning, usage, or a tool
call, so production code should inspect the fields it needs.

For config-file loading, aliases, and model inference, see
[Models and the Router](../concepts/routing.md). Runnable repository examples
are available in
[`examples/chat.rs`](https://github.com/sanjay920/llmshim/blob/main/examples/chat.rs)
and
[`examples/stream.rs`](https://github.com/sanjay920/llmshim/blob/main/examples/stream.rs).


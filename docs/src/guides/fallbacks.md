# Fallback chains

A fallback chain keeps the conversation fixed while trying an ordered list of
model addresses. **Retry the route, then change the route.**

> **Availability:** Rust: `completion_with_fallback` · CLI: not exposed · Proxy/clients: non-streaming `fallback` field

Fallback is useful for temporary provider failures. It is not model selection:
llmshim never chooses or reorders the models for you.

## Rust

The Rust `FallbackConfig` list contains the complete order, including the
primary model first:

```rust
use llmshim::{completion_with_fallback, FallbackConfig};
use serde_json::json;

let request = json!({
    "model": "anthropic/claude-sonnet-4-6",
    "messages": [{"role": "user", "content": "Hello"}],
    "max_tokens": 128
});

let fallback = FallbackConfig::new(vec![
    "anthropic/claude-sonnet-4-6".into(),
    "openai/gpt-5.5".into(),
    "gemini/gemini-3.5-flash".into(),
]);

let response = completion_with_fallback(
    &router,
    &request,
    &fallback,
    None,
).await?;
```

When `FallbackConfig.models` is non-empty, that list supplies the primary and
all fallback addresses. The function rewrites the request's `model` for each
attempt.

The builder also allows callers to change the fallback layer's retries per
model and its initial backoff:

```rust
use std::time::Duration;

let fallback = FallbackConfig::new(models)
    .max_retries(1)
    .initial_backoff(Duration::from_millis(250));
```

## Proxy and clients

The proxy keeps the primary in `model`; `fallback` contains only the addresses
to try afterward:

```json
{
  "model": "anthropic/claude-sonnet-4-6",
  "messages": [{"role": "user", "content": "Hello"}],
  "config": {"max_tokens": 128},
  "fallback": [
    "openai/gpt-5.5",
    "gemini/gemini-3.5-flash"
  ]
}
```

The proxy prepends `model` to the supplied fallback array before calling the
Rust fallback API. Language clients expose the same request field; some offer
a convenience argument for it.

## What triggers retry and failover

The default fallback statuses are:

```text
429  500  502  503  529
```

Transport errors are retryable too. For an eligible failure, the fallback
layer retries the current model with exponential backoff, doubling the delay
between attempts. If the model still fails, llmshim advances to the next
address. A success returns immediately. If every address fails, Rust returns
`ShimError::AllFailed` with the collected errors; the proxy returns an
`all_failed` error response.

Each fallback model must resolve to a provider registered on the Router. A
chain can cross providers, but only when their keys are configured.

## Fallback is non-streaming only

There is no streaming fallback API in the Rust crate. The proxy's streaming
path calls `llmshim::stream` directly and does not inspect `fallback`.

That means both of these ignore the fallback array:

- `POST /v1/chat/stream`;
- `POST /v1/chat` with `"stream": true`.

Do not send `fallback` on a streaming request expecting failover. Implement a
new stream attempt in the caller only if your application can safely decide
what to do with content already emitted by the failed model.


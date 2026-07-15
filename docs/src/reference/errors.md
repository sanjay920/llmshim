# Errors and retries

There are two decisions to make when a request fails: how the current surface
reports it, and whether repeating the request is safe.

## Rust error type

Rust APIs return `llmshim::error::Result<T>`, whose error is `ShimError`:

| Variant | Meaning |
|---|---|
| `UnknownProvider(String)` | The model address could not identify a registered provider |
| `MissingModel` | The request has no string `model` field |
| `Http(reqwest::Error)` | Provider transport failed |
| `Json(serde_json::Error)` | A required JSON conversion failed |
| `ProviderError { status, body }` | A provider returned a non-success status or native error |
| `Stream(String)` | A provider stream could not be parsed or translated |
| `AllFailed(Vec<String>)` | Every route in a fallback chain failed |

`llmshim::completion` returns one final error. `llmshim::stream` can fail while
opening the stream, and each yielded item is also a `Result<String>` because a
failure can occur after streaming begins.

## What the shared client retries

The shared provider client automatically retries:

- transport connect, timeout, request, and body failures;
- HTTP `429`, `500`, `502`, `503`, `504`, and `529`.

By default it performs three retries after the initial request, for at most
four attempts. It honors usable `Retry-After` and recognized provider reset
headers, otherwise uses exponential backoff with jitter. Configure the count
and wait cap with `LLMSHIM_MAX_RETRIES` and `LLMSHIM_MAX_BACKOFF_SECS`.

Other statuses are terminal at this layer. In particular, `400`, `401`, `403`,
and `404` normally indicate a request, credential, permission, or route problem;
retrying the same request will not repair them.

Non-streaming fallback has its own policy after the shared-client attempt. It
can retry/fall through on transport errors and `429`, `500`, `502`, `503`, or
`529`; it does not add `504` to its default fallback list. See
[Fallback chains](../guides/fallbacks.md).

## Proxy JSON errors

Before streaming begins, proxy errors use an HTTP status and a stable envelope:

```json
{
  "error": {
    "code": "unknown_provider",
    "message": "Unknown provider or model: example"
  }
}
```

| Source | HTTP status | `error.code` |
|---|---:|---|
| Missing model | `400` | `missing_model` |
| Unknown provider/model | `400` | `unknown_provider` |
| Provider error | provider status when valid | `provider_error` |
| Provider HTTP transport failure | `502` | `http_error` |
| JSON conversion failure | `500` | `json_error` |
| Stream setup/translation failure | `500` | `stream_error` |
| Exhausted fallback chain | `502` | `all_failed` |
| Proactive rate-limit rejection | `429` | `rate_limited` |
| Concurrency queue timeout | `503` | `overloaded` |

The proxy-generated `429 rate_limited` and `503 overloaded` responses include
`Retry-After` in whole seconds. An upstream provider's `429` or `503` status is
preserved, but its final response is not guaranteed to include that header.

Malformed JSON or a body that cannot be deserialized can be rejected by the
HTTP framework before llmshim's error mapping runs; clients should use the HTTP
status as well as the JSON body.

## Errors during SSE

Admission happens before the SSE response is committed, so proactive `429` and
`503` failures are ordinary JSON HTTP responses. Once streaming has begun, a
failure is a final typed event:

```text
event: error
data: {"type":"error","message":"stream error: ..."}
```

Do not expect an HTTP status change after headers have been sent. Consumers
must handle both the initial HTTP response and `error` events in the stream.

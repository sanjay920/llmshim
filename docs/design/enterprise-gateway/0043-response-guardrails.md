# 0043: Response guardrails

Status: slice 2 of the guardrail pipeline, built (non-streaming chat and transcription). Redaction, streaming
filters and external classifiers are not built.

## The rule

0040's `[[guardrails]]` rules gain a `scope`:

```toml
[[guardrails]]
name = "no-leak"
scope = "response"           # "request" (the default, as 0040), "response", or "both"
tenants = ["platform"]       # optional, as before
block_if_matches = ["AKIA[0-9A-Z]{16}"]
```

- **`response`** rules read the model's answer, after the provider has answered and before the caller sees it.
  An answer whose text matches is replaced by `502 guardrail_blocked_response` ("response blocked by guardrail
  <name>"), naming only the rule. **The provider has answered and been paid**, so the attempt is recorded as
  completed with its tokens and cost (the secret is in no log: the record holds counters), and
  `llmshim_gateway_rejected_total{reason="guardrail_response"}` counts it by provider. A blocked answer is never
  cached for an idempotency replay (the check runs before the cache is written) and is not retried on another
  target: it is not a provider failure.
- **`request`** rules are exactly 0040. **`both`** reads both directions with the same patterns.
- **What is scanned.** For chat, everything under `choices` of the answer (content, tool-call arguments,
  reasoning text, and the keys of objects, as 0040 scans requests); for transcription, the transcript. **Not
  scanned:** embeddings (vectors), speech (audio), a generated image's revised prompt, and the answer's envelope
  (`id`, `model`, usage).
- **Streaming is refused to a caller under a response rule.** A stream cannot be checked after its first byte has
  gone out, and buffering it would end streaming in all but name, so a caller whose tenant has any rule that reads
  responses is refused `400` when it asks for `stream: true` (before anything is sent), on every route that
  streams. Request-only rules never affect streaming. The refusal is explicit rather than a silent unchecked
  stream: a response rule that a stream could slip past is not a rule.
- The patterns, matching engine, validation, tenant scoping and the not-echoed rules are 0040's. The rules still
  compile into one combined set per scope and direction, and are fixed at start.

## Not built

- **Redaction** (rewriting the answer), **streaming filters** (cutting a stream when a pattern appears, after
  bytes have gone out), **external classifiers** and moderation APIs.
- Response checks for the Responses wire's and the native facades' *streaming* shapes (they stream, so a caller
  under a rule cannot use them), for images and for speech.
- A dry-run mode, and a per-key scope.

## Tests

`serve::guardrails::tests`: a request-only rule reads requests and never responses and a response rule the
reverse; a response rule is its tenant's alone; `both` reads both; every tenant is under a rule that names none;
only request rules leave a tenant free to stream. `integration_serve_guardrails` (process): a leaking answer
becomes a 502 naming the rule while the request went through, the attempt is recorded as completed, the secret is
in no log and the reason is counted; a clean answer is returned, a stream by the caller under the rule is a 400,
and a request-only rule lets the same answer through. Skipping the response check, or allowing the stream, each
fails a process test. The JSON Schema is regenerated.

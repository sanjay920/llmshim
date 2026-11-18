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
  An answer whose text matches is replaced by `422 guardrail_blocked_response` ("response blocked by guardrail
  <name>"), naming only the rule. **The provider has answered and been paid**, so the attempt is recorded as
  completed with its tokens and cost (the secret is in no log: the record holds counters), and
  `llmshim_gateway_rejected_total{reason="guardrail_response"}` counts it by provider. A blocked answer is never
  cached for an idempotency replay (the check runs before the cache is written) and is not retried on another
  target: it is not a provider failure.
- **`request`** rules are exactly 0040. **`both`** reads both directions with the same patterns.
- **What is scanned.** The text of the answer, from the `choices` of the normalized response: the message's
  content (the parts of a content array joined into one string, so a secret split across two parts is whole),
  refusal, tool-call names and arguments (the arguments are JSON text and are **parsed**, so a secret written with
  an escape such as `\u0041KIA...` is read as the text it decodes to), and reasoning text; object keys are matched
  as in 0040. **Skipped, because they are not the answer's text:** signatures and encrypted reasoning
  (`thought_signature`, `signature`, `encrypted_content`, `responses_output`, `wire_ids`, `redacted_data`: opaque
  blobs a broad pattern would match inside by chance, at a cost in CPU too) and structure (`id`, `item_id`,
  `tool_call_id`, `role`, `finish_reason`, `index`, `type`, `origin`). Unlike a request, a response is shaped by
  llmshim, so the skipped keys are known. **Not scanned:** embeddings (vectors), speech (audio), a generated
  image's revised prompt, and the answer's envelope (`model`, usage). For transcription the transcript is read.
- **The status is 422, not a 5xx.** The OpenAI and Anthropic SDKs retry 5xx automatically, and a blocked answer
  is deterministic enough that each retry would be paid for again; a 422 is not retried. It is also not a provider
  failure, so it stays out of dashboards keyed on 5xx.
- **A replayed answer is judged too.** An idempotency cache (Redis, across a restart or a rolling update) can hold
  an answer stored before a rule existed, so a replay is checked with today's rules; a blocked answer is never
  stored.
- **Streaming is refused to a caller under a response rule.** A stream cannot be checked after its first byte has
  gone out, and buffering it would end streaming in all but name, so a caller whose tenant has any rule that reads
  responses is refused `400` when it asks for `stream: true` (before anything is sent), on every route that
  streams. Request-only rules never affect streaming. The refusal is explicit rather than a silent unchecked
  stream: a response rule that a stream could slip past is not a rule.
- The patterns, matching engine, validation, tenant scoping and the not-echoed rules are 0040's. The rules still
  compile into one combined set per scope and direction, and are fixed at start.

## Not built

- **Redaction of answers** (request redaction is 0044), **streaming filters** (cutting a stream when a pattern appears, after
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

## Found in review

A first draft scanned every string of `choices` (signatures and ids included), was a 502, and did not check a
replayed answer; review found the false positives, the SDK retries and the replay gap, and a secret split across
content parts or escaped inside tool arguments. `serve::guardrails::tests::a_response_is_read_as_text_not_as_opaque_blobs_and_split_or_escaped_secrets_are_whole`
and `integration_serve_guardrails::an_escaped_tool_call_argument_and_every_streaming_route_are_covered` pin them.
The replay check cannot be exercised by a process test (a cache entry from before a configuration change needs a
second configuration); it is one call and is covered by review. With `[control_plane]` a rule's `tenants` cannot
be checked against the snapshot's, so `serve` prints a warning at start.

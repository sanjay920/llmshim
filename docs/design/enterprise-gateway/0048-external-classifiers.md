# 0048: External guardrail classifiers

Status: slice 6 of the guardrail pipeline. Written on the owner's instruction to implement it. Request and
non-streaming response classification; **streams are not classified** (below).

## What it is

A guardrail rule (0040, 0043) whose verdict comes from an HTTP endpoint the operator runs, instead of from patterns:
a moderation model, a PII service, a policy engine. The gateway sends it the text and acts on `allow` or `block`.
The classifier is the operator's, so nothing here knows what it checks.

```toml
[classifiers.moderation]
url = "https://classifier.internal.example/check"   # https, or http with allow_insecure_http
token = { env = "CLASSIFIER_TOKEN" }                # optional bearer token, by reference
timeout_ms = 2000                                   # 100 to 30000, default 2000
max_bytes = 262144                                  # the most text sent, 1 KiB to 4 MiB, default 256 KiB

[[guardrails]]
name = "moderation"
classifier = "moderation"        # instead of block_if_matches / redact_matches
scope = "both"                   # request (default) | response | both
tenants = ["platform"]           # optional, as every rule
on_failure = "block"             # block (default) | allow
```

A rule sets exactly one of `block_if_matches`, `redact_matches`, `classifier`. `on_failure` belongs to a classifier
rule only. Both tables are fixed at start, like `[[guardrails]]`: a reload that changes them is refused by path.

## The wire

`POST <url>`, `content-type: application/json`, `authorization: Bearer <token>` when a token is set:

```json
{"direction": "request", "rule": "moderation", "text": "…"}
```

`text` is every string value of the request payload (an object's members in key order; structure such as a role is
sent too, since a string the model sees is a string judged) or of the answer (read as 0043 reads it: the answer's text,
not its signatures or ids), joined with newlines. Inline images, audio and video are left out. **Nothing else is sent**: no
tenant, key, model, request id or header, so the classifier learns nothing about who asked.

The answer must be `200` with a JSON body of at most 4 KiB: `{"verdict": "allow"}` or `{"verdict": "block"}`. Other
fields are ignored (a category, a score): they are the classifier's own, never echoed to the caller and never a
metric label, so a hostile or buggy classifier cannot put text into a response or grow the metrics.

## Decisions

- **Fail closed by default, per rule.** A classifier that is down, slow, answers anything but a valid verdict, or is
  sent more than `max_bytes` is a *failure*. With `on_failure = "block"` the request is refused `503
  guardrail_unavailable` ("guardrail <name> could not be evaluated"), before anything is reserved for a request
  and after the provider was paid for an answer. 503 is retried by SDKs, which is right for an outage; an answer
  already paid for is paid again by the retry, which is the price of a closed rule. With `on_failure = "allow"` the
  text passes and the failure is counted (`failed_open`). Oversize is a failure, never a truncation: a classifier
  that saw the first 256 KiB has not seen the text.
- **Redirects are not followed**, and the endpoint is the operator's alone: a 3xx is a failure.
- **A block** is exactly the regex rule's refusal: 400 `guardrail_blocked` for a request, 422
  `guardrail_blocked_response` for an answer, naming the rule and nothing else. The answer was paid for and is
  recorded as completed; a blocked answer is never cached for an idempotency replay; a replayed one is judged again.
- **Order.** A request is judged after the regex rules and redaction (the classifier sees the text that would be
  sent), and before anything is reserved. An answer is judged after redaction and the regex rules. Rules run in the
  file's order, one at a time, and the first block or closed failure ends it; the latency added is the sum of the
  calls, up to `timeout_ms` each.
- **Streams are not classified, and a caller under a response classifier cannot stream.** A verdict needs the whole
  text; holding a stream until it arrives ends streaming in all but name, and releasing text before the verdict is
  the thing the rule exists to prevent. The tenant is refused `400` when it asks for `stream: true` (before anything
  is sent), on every route that streams, exactly as 0043 refused streaming before 0046. Request-only classifier rules
  never affect streaming.
- **What is recorded.** `llmshim_gateway_guardrail_classifier_total{rule,outcome}` with `outcome` one of `allow`,
  `block`, `failed_open`, `failed_closed` (the rule is the operator's name: bounded by the file); a refusal also
  counts in `llmshim_gateway_rejected_total` (`reason` `guardrail` or `guardrail_response`, `guardrail_unavailable`
  for a closed failure). **The audit log (0047) records the configuration, not each verdict**: `server.started`
  gains `guardrails` (each rule's name, kind, scope, and for a classifier its host and `on_failure`), so a change of
  classifier, endpoint or failure policy is visible at the restart that makes it, next to the file's hash. A record
  per verdict would make the log's fsync a lever on request latency for anyone who can send a blocked text, so
  verdicts are the metric's, and the usage and access logs already carry the refusal's status.
- **Not echoed.** The URL's path and the token are never in a message, a log or a metric (the host only, in the audit
  record). The text goes to the classifier and nowhere else.
- **The call is bounded.** One shared HTTP client without redirects or ambient proxy, a total timeout per call, at
  most 4 KiB read from the answer, and a concurrency cap per classifier (64 in flight; over it a call fails at once,
  by the rule's policy) so a slow classifier cannot hold every request.

## Not built

- Redaction by a classifier (a verdict is allow or block; rewriting text is the regex rules' job).
- Classifying streams (above). Request classification covers the chat routes (chat, the native facades, the
  Responses wire); the requests of embeddings, speech, images and transcription are not sent to a classifier, though a
  transcript is judged as an answer.
- Retries of a failed call, caching of verdicts, calling a classifier in parallel.
- Per-key scope, a dry-run mode, classifier health in `/ready`.

## Tests

`serve::classifier::tests` (unit): the text sent is the values only, media left out, a response read as 0043 reads
it; verdict parsing is strict; every failure kind (timeout, 500, redirect, bad JSON, unknown verdict, oversize,
over the concurrency cap) follows `on_failure`; the token is sent and the rule's name is in the call.
`integration_serve_classifier` (process, a mock classifier): a blocked request is a 400 and never reaches the provider;
a clean one is served; a blocked answer is a 422 and was paid for; a down classifier is a 503 closed and passes open;
a stream by a tenant under a response classifier is a 400 and a request-only classifier lets it stream; only the text
reaches the classifier (no tenant, no key); the audit record names the rule and host and no path or token. The JSON
Schema is regenerated.

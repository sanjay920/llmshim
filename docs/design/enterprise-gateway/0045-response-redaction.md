# 0045: Response redaction

Status: slice 4 of the guardrail pipeline, built for non-streaming chat and transcription. Streaming filters and
external classifiers are not built.

## Decision

Decided on the owner's standing instruction to decide. 0044's `redact_matches` rules read the request only; the same
rules can now read the **answer**, by `scope`:

```toml
[[guardrails]]
name = "mask-keys"
scope = "response"                      # or "both"
redact_matches = ["AKIA[0-9A-Z]{16}"]
replacement = "[hidden]"
```

- `scope` is `request` (the default, 0044), `response`, or `both` (the same patterns in both directions).
- A response rule rewrites the **text a caller will read** in a non-streaming answer, after the provider has
  answered and been paid and **before the answer is judged, cached or returned**: each choice's message `content`
  (a string or the `text` of its parts), `refusal`, `tool_calls` (the arguments are decoded, masked and
  re-encoded as in 0044), the readable items of `responses_output` (a `web_search_call`'s query, a tool-search
  output), and a transcript's `text`. **Not touched:** the envelope (`id`, `model`, usage), the reasoning and its
  signatures, opaque encrypted items (`encrypted_content`), structure keys (`id`, `type`, `name`, `role`), and an
  image's revised prompt. **An answer is never exempt for starting like a file:** the `data:` and `data` /
  `file_data` exemptions of 0044 are for requests, which carry encoded files; an answer that begins
  `data:image/png, key AKIA...` is masked (and judged) like any other text.
- **The answer is masked, not refused, with a 200**; the attempt is recorded as completed with its tokens and cost;
  the rewrite is counted by rule name (`llmshim_gateway_guardrail_redactions_total{rule}`, the same counter as
  requests) and nothing about the matched text is logged.
- **What it could not reach is refused.** As in 0044, a redaction rule's patterns also act as blocking patterns for
  the direction it reads: after the rewrite, a match left in the answer that the rewrite does not cover (reasoning
  text, for one) is refused `422 guardrail_blocked_response`, not returned. A response rule masks what it can reach
  and refuses what it cannot.
- **Order.** Redaction, then the response blocking rules, then the idempotency cache write and the reply: a cached
  answer is the masked one, and a replayed answer (a cache that outlived a configuration) is masked and judged by
  today's rules.
- **A caller under a response rule cannot stream** (0043): a stream cannot be rewritten after its first byte, so it
  is refused 400 before anything is sent. This holds for redaction rules as for blocking ones.
- The bound on growth (four times the text plus 4 KiB), the patterns that match everywhere and the replacement
  rules (no quote, no backslash, not matched by its own patterns) are 0044's, and apply to answers.

## Found in review

- A model answer beginning `data:image/...` was skipped by both the rewrite and the check (the exemption was
  written for requests): a prompt-injected or quirky model could carry a secret out under that prefix. The
  exemption now applies to requests only.
- `responses_output` was skipped by both, yet the Responses facade copies its `web_search_call` and tool-search
  items to the caller. Its readable items are now masked and judged; only encrypted content is skipped.
- A secret split across content parts is refused when the array also holds a non-text part (the parts' text is
  joined whatever else the array holds); masking is per part, so such a split passes the rewrite and is caught by
  the residual check, which refuses it.
- The growth overflow has its own refusal (`422 guardrail_redaction_too_large`, reason `guardrail_redaction`, the
  provider's label), not the block's.

## Known limits

- **Errors are not cached.** A refusal after the provider answered returns before the idempotency write, so a retry
  with the same key pays for the call again (0043's block has the same property).
- **The growth bound compounds** across rules: four times the text per rule, as on the request side.
- **Signed thinking.** A masked answer is what a caller sends back as history, and the native facades issue replay
  receipts from the answer. On models that bind thinking signatures to the conversation prefix a changed assistant
  turn may fail its check on the next turn. Not traced and not tested; mask the user's side, or accept the 400.

## Not built

- **Streaming filters**: rewriting or cutting a stream while it runs, and so streaming for a caller under a response
  rule.
- **External classifiers** and moderation APIs (a network call in the request path, with its own failure modes).
- Redaction of reasoning text, of embeddings, image and speech answers, and reversible masking (restoring what a
  request redaction removed).

## Tests

`serve::guardrails::tests::a_response_redaction_masks_the_text_a_caller_reads_and_nothing_else` (content parts,
refusal, tool arguments and a transcript masked; the envelope, reasoning and its signature untouched; a response rule
is not applied to a request nor a request rule to an answer). `integration_serve_guardrails::a_response_redaction_rule_masks_the_answer_the_caller_receives`
(process): the provider's leaking answer reaches the caller masked with a 200 on the engine route and on the Anthropic
facade, both attempts are recorded as completed and paid, the secret is in no log, and a stream by that caller is a
400. Not redacting the answer fails it.

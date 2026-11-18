# 0044: Request redaction

Status: slice 3 of the guardrail pipeline, built (request-side redaction). Response redaction is not built.

## Decision

Decided on the owner's standing instruction to decide: a refusal is the right answer to a request that must not go,
but often the right answer is to **mask the part that must not leave** and send the rest. A rule can now rewrite
instead of refuse:

```toml
[[guardrails]]
name = "mask-keys"
tenants = ["platform"]                  # optional, as before
redact_matches = ["AKIA[0-9A-Z]{16}"]   # instead of block_if_matches
replacement = "[hidden]"                # optional, default "[redacted]"
```

- **A rule is a blocker or a redactor, not both** (`block_if_matches` or `redact_matches`; setting both, or
  neither, is a configuration error by path: use two rules). A redaction rule is **request scope only**: its
  `scope` must be `request` (the default); redaction rewrites what the provider is sent, and rewriting an answer
  is not built.
- **What is rewritten.** Each match, in the text of the request's **messages**: `content` (a string, or the
  `text` of its parts) and the text of `tool_calls` (the arguments, which are JSON text). **Never rewritten:**
  structure (`id`, `type`, `name`, `role`, `tool_call_id`: a replaced id or tool name would break the request), an
  inline image, audio or video (`data:image/`, `data:audio/`, `data:video/`), and everything outside the
  messages (tool definitions, `response_format`, provider passthrough fields, the embeddings and media routes).
  The last are still **scanned by blocking rules**: a secret that cannot be redacted can still be refused.
- **The replacement is literal.** `$1` or `\1` in it is inserted as written, never as a group reference.
- **Order.** Redaction runs first, on the request as the caller sent it, then blocking rules judge what is left:
  a request whose secret was masked is not refused for it, and a pattern a mask did not cover still refuses.
  Both are by the caller's tenant (`tenants`, as 0040).
- **It covers every chat route**: `/v1/chat`, `/v1/chat/completions`, `/v1/messages` (the Anthropic `system` is
  a message by then), `/v1/responses`, the Gemini facade and `/v1/chat/stream`, unary and streaming alike (the
  rewrite happens before the request is prepared). A caller that does not authenticate is left to be refused by
  the handler.
- **Observability.** `llmshim_gateway_guardrail_redactions_total{rule}` counts requests a rule rewrote (the label
  is the operator's rule name; cardinality is bounded by the file). Nothing about the matched text, the
  replacement count per text or the original is logged or counted.
- **Cost.** A text is first tested against the rule's pattern set and rewritten only if one matches, so a clean
  text costs one pass per redaction rule; the rules are few (at most 64 by the file's bound) and fixed at start.
- Fixed at start, like every guardrail: a reload that changes the rules is refused by path.

## Limits (not built)

- **Response redaction**, and redaction in the embeddings, image, speech and transcription routes (they are
  *scanned* by blocking rules, not rewritten).
- Redaction of tool definitions, `response_format` and provider passthrough fields.
- Reversible masking (restoring the original in the answer), per-pattern replacements, and dry-run.
- As in 0040, **normalization is the pattern author's job**: case, homoglyphs and encodings are not seen.

## Tests

`serve::guardrails::tests::redaction_rewrites_message_text_and_never_structure` (every match in a text and in the
parts of an array, an inline image and a clean part untouched, tool-call arguments rewritten while the call's id,
type and name are not, a replacement that looks like a group reference inserted literally) and
`::a_redaction_rule_for_one_tenant_leaves_the_others_text_alone`. `integration_serve_guardrails` (process, a
provider mock matched on what it receives): through chat, content parts, the streaming route and the Anthropic
facade the provider receives the masked text and never the secret (a mock that matches the secret expects zero
calls), and the redaction counter is 4; redaction and blocking compose (a masked secret is served, a remaining
blocked pattern refuses), and a rule that sets both, or a redaction rule with a response scope, is refused by path.
Not calling the redaction at either entry point fails the process test.

# 0044: Request redaction

Status: slice 3 of the guardrail pipeline, built (request-side redaction). Response redaction is 0045.

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
  neither, is a configuration error by path: use two rules). A redaction rule's `scope` is `request` by default; `response` and `both` are 0045 (which supersedes this note's
  original request-only restriction).
- **What is rewritten.** Each match, in the text of the request's **messages**: `content` (a string, or the
  `text` of its parts) and the text of `tool_calls`. A tool call's `arguments` is JSON text: it is **decoded,
  masked and re-encoded**, so a secret written with an escape is found, the result stays valid JSON, and inside it
  every key is the caller's data (a key named `name` or `id` is masked like any other). **Never rewritten:**
  structure (`id`, `type`, `name`, `role`, `tool_call_id` of a message or call), encoded files (any string
  beginning `data:`, and a `data` or `file_data` field: a pattern must not shred a PDF or audio), and everything
  outside the messages (tool definitions, `response_format`, provider passthrough, the embeddings and media routes).
- **What redaction cannot reach is refused, not sent.** The patterns of a redaction rule **also act as blocking
  patterns on the prepared request after the rewrite**: a match left anywhere (a tool description, a passthrough
  field, anything outside the message text) is refused `400 guardrail_blocked` naming the rule. So a rule masks
  what it can reach and refuses what it cannot, and a redact-only configuration never lets a secret through in a
  field it does not rewrite. (A first draft did, which review found.)
- **Patterns that would match everywhere are refused at start**: one that matches the empty string (`x*`, `\b`)
  would put the replacement between every character, and the same check applies to blocking patterns, where it
  would refuse every request. A replacement must also not be matched by its own rule's patterns, and **may not
  contain a quote or a backslash** (it is inserted into JSON text).
- **Growth is bounded.** A rewritten text may be at most four times its length plus 4 KiB; a rule whose patterns
  would grow it further (one pattern per character with a long replacement, applied three times) refuses the
  request `400 guardrail_blocked` naming the rule, instead of building a multiple of the body in memory.
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

## Known limits

- **Idempotency.** The key's fingerprint is computed from the redacted request, so two requests that differ only in
  the masked secret share a fingerprint: a retry with a different secret is replayed rather than refused as a
  conflict (the answer was computed without the secret either way).
- **Signed history.** Redaction rewrites assistant text and tool arguments in a replayed turn as it does any
  message; a turn that carries a signed thinking block or an owned tool id may then fail its signature or
  "edited call" check with a 400 (never a leak). Untested; mask the user's side, or accept the 400.
- **Cost.** The rewrite runs before admission, on the request's own task, over a body of at most the route's
  size bound (2 MiB): a clean text costs one pass per redaction rule.
- `redact_request` identifies the caller itself, then the handler does again; both read the same key store, and
  an unauthenticated caller is left to be refused by the handler.

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
blocked pattern refuses), and a rule that sets both, or a redaction rule with a response scope, is refused by path. Review's findings are pinned by `redaction_keeps_tool_arguments_valid_json_and_leaves_files_and_structure_inside_them`, `a_redaction_that_would_multiply_a_text_is_refused_not_applied`, `a_redaction_rules_patterns_also_refuse_what_redaction_could_not_reach` and the process test `a_secret_redaction_cannot_reach_is_refused_and_unsafe_rules_are_refused_at_start`.
Not calling the redaction at either entry point fails the process test.

# 0046: Streaming guardrails

Status: slice 5 of the guardrail pipeline, built. It supersedes 0043's and 0045's refusal to stream for a caller under a
response rule. External classifiers are not built.

## The change

0043 refused `stream: true` to a caller under any response rule, because a stream cannot be rewritten after its first
byte has gone out. A caller that needs streaming had to leave the gateway. Streams are now **filtered as they run**:

- **Blocking rules** (`scope = response|both`, `block_if_matches`): when the streamed text matches, the stream is
  **cut** with an `error` event `response blocked by guardrail <name>`. The head and any text already released have
  gone out (the status stays 200), but text is **held back** so a match is normally seen before its first byte is
  released (below), and nothing after the cut is sent.
- **Redaction rules** (`scope = response|both`, `redact_matches`): each match is **replaced** in place as the text is
  released, including a match that arrives split across chunks.
- **The hold-back.** The filter keeps the last **256 bytes** of streamed text unreleased and releases text only once
  it is that far behind the newest, on a character boundary, and never the start of a redaction match that the cut
  would split. A match up to 256 bytes long is therefore seen whole before any of it is released; a **longer match
  can leak its first bytes** before it completes and is cut, and a redaction match longer than the hold-back is
  masked only where it is found whole. The cost is a delay of up to 256 bytes of text, which for an ordinary
  token stream is a few tokens.
- **Flushing.** Any event that is not content text (a tool call, reasoning, usage, `done`) flushes the held text
  first, filtered; so does the end of the stream. A secret split across a content / reasoning / content boundary is
  therefore not seen whole, since the flush ends the window.
- **Tool calls** arrive complete and are masked and judged whole, as in an unary answer (0043, 0045). **Not
  filtered:** reasoning text, the `done` event's `responses_output`, and usage.
- The streaming routes are all covered: the engine route, `/v1/chat/stream` and the native facades, since the filter
  sits on the typed event stream they all translate from. A deployment stream's first-item retry (0042) is
  unaffected: the first item is forwarded through the same filter.
- Redaction rules' patterns act as blockers in an unary answer for what redaction cannot reach (0045); in a stream the
  filter masks the text it releases, so they are **not** also blockers there.
- Patterns, growth bound (four times the text plus 4 KiB per released piece), replacement rules, tenant scoping and
  the not-echoed rules are 0040, 0044 and 0045's.

## Found in review

- **One error, and nothing after a cut.** A cut (a blocking match, a redaction that would grow too far, or an upstream
  error) drops the text still held back; the held text is **released only when the stream ends by itself**. The first
  draft flushed after a cut, which sent a second error event and up to 256 bytes of text after the first.
- **The held text is bounded.** A redaction pattern that can match an arbitrarily long run (`[A-Za-z0-9+/=]{40,}`)
  keeps the start of a match unreleased until it ends, and the filter rescans what it holds on every chunk. The held
  text is therefore capped at **64 KiB**: past it the stream is cut, naming the rule, instead of holding and
  rescanning without bound. (`StreamFilter::check` scans the held text in place, without copying it.)
- The process tests assemble the `delta.content` pieces a caller reads, not the body as a whole (a secret released
  in two pieces would not appear in a body search): a long multibyte stream is masked exactly, and a cut is one error
  with no secret and no text after it.

## Known limits

- **Redaction is applied to each released piece on its own**, so a pattern with `^`, `$`, `\b` or `(?m)` behaves at
  the edge of a piece as it would at the edge of a text.
- A block match longer than the hold-back can leak its first bytes (above). The upstream-error flush path (an error
  chunk from the provider while text is held) is by construction a cut, and is covered by the unit test of a cut
  stream, not by a process test.

## Not built

- **External classifiers** and moderation APIs (a network call in the request path).
- Matches longer than the hold-back (a configurable window would trade latency for coverage), filtering of reasoning
  streams, and rewriting across a non-content event.

## Tests

`serve::guardrails::tests`: a streamed key split across four chunks, among a lot of text, is masked whole and
everything else comes out exactly as sent (a short stream, and multibyte text, too); a blocking rule cuts the stream
when the match completes and none of the key had been released; a clean stream passes; `stream_filter` is present
for exactly the tenants under a response rule. `integration_serve_guardrails` (process, a streaming provider mock):
the masked text reaches the caller on the engine route, `/v1/chat/stream` and `/v1/messages`, never any part of the
secret; a streamed leak under a blocking rule is cut with the rule's name and leaks nothing, while a clean stream is
untouched. Sending the stream unfiltered, or setting the hold-back to zero, fails them. `::a_cut_stream_releases_nothing_afterwards_and_a_match_that_never_ends_is_cut_not_held` pins the review's two findings (an unbounded run is cut at the cap, a cut leaves nothing held), and `integration_serve_guardrails::a_long_stream_releases_masked_text_in_pieces_and_a_cut_is_one_error_with_no_secret` the exact assembled text and the single error.

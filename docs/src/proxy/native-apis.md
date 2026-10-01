# Native API endpoints

Both `llmshim proxy` and `llmshim gateway` serve:

| Endpoint | Request and response format |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions |
| `POST /v1/messages` | Anthropic Messages |
| `POST /v1/responses` | Stateless Responses JSON or SSE |
| `POST /v1beta/models/{model}:generateContent` | Google Gemini Generate Content |
| `POST /v1beta/models/{model}:streamGenerateContent` | Google Gemini Generate Content, SSE |

Point a native SDK at the server base URL and use a configured model route, such
as `anthropic/claude-sonnet-4-6`, `openai/gpt-6-astra` or
`gemini/gemini-3.8-flash`. Provider credentials stay in the server. On an
authenticated gateway, the client key is a gateway key: `Authorization: Bearer …`,
Anthropic's `x-api-key` and Gemini's `x-goog-api-key` are accepted. An explicit
Authorization header takes precedence. Native requests use the existing gateway
queue, authenticated tier, quotas and request IDs, or the proxy's concurrency
and rate-limit admission. Rejections preserve HTTP status and Retry-After.
Native idempotency keys are scoped to credential and wire format.

Gemini names its model and its action in the path, so a routing id's slash
survives (`/v1beta/models/gemini/gemini-3.8-flash:generateContent`) and the body
carries neither `model` nor `stream`. Its turns are `contents[].parts`: text
becomes a message, `inlineData` becomes an image block, `functionCall` and
`functionResponse` become a tool call and its result, and a `thought` part is
restored from its receipt like any other opaque reasoning. `systemInstruction`,
`functionDeclarations`, `toolConfig`, `generationConfig` (including
`thinkingLevel`) and `responseSchema` translate through the shared engine.
`safetySettings` and `cachedContent` ride the `x-gemini` namespace to Google
verbatim; a field this server cannot express — `candidateCount` other than 1,
`thinkingBudget`, `allowedFunctionNames`, `includeThoughts: false`, a hosted
tool, an unknown part, or `x-cache`, whose explicit markers the engine applies on
the Anthropic wire only — is refused by name, because forwarding it would fail
upstream or silently disappear. Errors are Google's error object;
`generateContent` returns a `GenerateContentResponse` whose `modelVersion` is
the model that answered, and `streamGenerateContent` returns Gemini SSE chunks
with no event names and no terminator — the finish reason ends the stream. That
SSE form is the only one served: the query string is not read, so `alt=sse` is
accepted and ignored while a client asking for Google's JSON-array form would
have to parse frames it did not ask for. The credential is the
`x-goog-api-key` header (or an explicit `Authorization`, which wins); `?key=`
is not read. Any other action under `/v1beta/models/` — `:countTokens`,
`:embedContent`, a misspelled or missing action — is refused with Google's 404
error object instead of reaching the chat handler, because the path is routed by
a wildcard.
`:countTokens`, `:embedContent` and the cached-content endpoints are not served.

```json
{
  "model": "anthropic/claude-sonnet-4-6",
  "max_tokens": 1024,
  "system": "Answer briefly.",
  "messages": [{"role": "user", "content": "Hello"}],
  "stream": true
}
```

Messages system/content blocks, custom function tools, tool choice, tool results
(including `is_error`), stop sequences, cache markers, and native JSON Schema
output configuration translate through the shared engine. Chat Completions accepts
its standard message/function shapes and `response_format`; JSON-object mode also
gets object validation. Generation fields supported by the destination retain
their ordinary adapter behavior. The facade supports one completion (`n:1`) and
custom function tools; it rejects provider-hosted tool definitions rather than
turning them into client-executed functions. `n > 1` is refused rather than
emulated — the OpenAI backend is the Responses API, which has no `n`, and
neither do Anthropic Messages or Gemini; fanning out N requests would change
the cost, rate-limit footprint and cache behavior of what was asked for. The
refusal is a properly shaped OpenAI error naming the parameter:

```json
{"error": {"message": "Unsupported value: 'n' must be 1. …",
           "type": "invalid_request_error", "param": "n",
           "code": "unsupported_parameter"}}
```
 Provider-specific endpoints such as
batches, token counting, uploads and Responses are not served by these aliases;
Gemini's `:generateContent` family is, and nothing else under `/v1beta` is.

With `stream:true`, text arrives incrementally. Chat Completions emits
`chat.completion.chunk` records followed by `[DONE]`. Messages emits
`message_start`, content block events, `message_delta`, and `message_stop`.
Completed reasoning and tool blocks are emitted after text so their metadata is
complete before exposure; a Messages stream can therefore place these blocks
after the text block. Keepalives continue while the engine runs. Managed
structured-output/capture modes retain their validation buffering, as described
in [capability shims](../guides/capabilities.md). Errors during a stream are native
error events, not successful completion frames.

Native clients usually discard canonical tool wire maps. The facade stores the
metadata it issued in private local receipts under the OS data directory's
`llmshim/replay-receipts` folder. Set `LLMSHIM_REPLAY_RECEIPTS_DIR` to choose the
location or mount a shared durable directory for several server instances. Files
are written atomically with private permissions; they contain tool-call arguments
and reasoning/signature metadata, not provider credentials or whole prompts.
Receipts are scoped to a digest of the inbound credential. Preserve that directory
and credential when replaying native histories across restarts.

New receipts are retained for 30 days, with limits of 100,000 files and 256 MiB
of serialized receipt data. The oldest issued receipts are removed first when a
limit is reached. Configure these trusted-operator limits with
`LLMSHIM_REPLAY_RECEIPTS_TTL_SECS`, `LLMSHIM_REPLAY_RECEIPTS_MAX_ENTRIES`, and
`LLMSHIM_REPLAY_RECEIPTS_MAX_BYTES`. Receipt lock acquisition waits at most two
seconds by default; `LLMSHIM_REPLAY_RECEIPTS_LOCK_TIMEOUT_MS` can set a trusted
operator value from 1 to 30,000 milliseconds. Native HTTP conversion uses two
bounded blocking slots per application, with at most one used by ingress so a
completed response always has reserved egress capacity. The ingress and egress
wait queues are separately capped; excess or timed-out work receives a retryable
503. Canceled queued work performs no I/O, while a started worker retains its
capacity until it exits. The journal is capped at 64 MiB; crash recovery can also
use one fixed receipt staging file of at most 4 MiB and one fixed journal staging
file of at most 64 MiB.

One native request may restore at most 4,096 receipt occurrences, including
repeated uses of the same valid handle. Those occurrences share limits of 2 MiB
of encoded receipt data, 32,768 decoded JSON nodes, and 8 MiB of estimated owned
JSON memory. Each occurrence is admitted before its record is read and parsed;
repeating a key consumes the limits again because the canonical history repeats
the value. The reconstructed canonical request is also limited to 32,768 nodes,
8 MiB estimated owned JSON, and 2 MiB when serialized for the shared handler.
Excess restoration fails as a native 413 without truncating history or starting
provider inference. These conservative estimates bound conversion allocations;
they are not process RSS measurements.

On first use after an upgrade, llmshim counts existing receipt files once and
includes them in admission without expiring or deleting them because they have no
trustworthy issue time. If that bounded pass cannot establish a complete total,
existing receipts remain readable but new writes fail closed. After raising a
limit, set `LLMSHIM_REPLAY_RECEIPTS_RESCAN_INCOMPLETE_BASELINE=1` for one explicit
retry in that process. Managed replacements preserve existing files on disk and
record that the older value must never resurface after expiry. All processes
sharing a directory must run a version that participates in the retention lock;
an older binary can write outside its accounting. Deleting a retained receipt
makes its owned tool ID unavailable, and the call fails explicitly. A changed
call's arguments cannot reuse an issued receipt.

Untracked native thinking has no provenance and is dropped. Issued reasoning
restores its original typed block and still passes through the common family,
wire and account replay filter. Cross-wire opaque reasoning uses a receipt handle
in native Messages fields so the original block can be restored without pretending
it originated on that wire. Chat Completions includes a `reasoning_details`
extension (and readable `reasoning_content`); clients that discard them also
discard replayable reasoning. Keep full native
assistant messages and tool IDs when saving a conversation. Receipt metadata is
separate from the caller-owned conversation history.

Requests and reconstructed canonical handler bodies are limited to 2 MiB, and
buffered response conversion is limited to 32 MiB.
`x-cache` segment indices on native requests refer to their input `messages`
array; the facade remaps boundaries when system/tool-result conversion expands
that array. `x-shim` applies through the shared client. A signature mismatch's
`x-llmshim-served-model` observation appears in JSON and the HTTP header for unary
responses, or in terminal stream metadata. It never changes message provenance.

## Responses streams and reasoning history

`POST /v1/responses` accepts `stream: true` through the same engine and
admission path as ordinary requests. The SSE event name matches the JSON
`type`, and `sequence_number` increases across creation, progress, item/part
addition, text or function-argument deltas, item completion, and the terminal
response. Text arrives incrementally except when hosted search requires buffering
for native item order; function arguments arrive once the shared engine has
assembled and validated the complete call. The terminal event is
`response.completed` or `response.incomplete` with final output and usage.
Upstream failure or premature EOF emits `response.failed`. Disconnecting cancels
the body and drops the upstream stream.

Reasoning input items require a `summary` array of `summary_text` parts and may
carry string `id` and `encrypted_content` fields. Return the issued item unchanged
in the next request. Credential-scoped receipts restore the original provider
block, including encrypted content or an Anthropic thinking signature. The
existing replay policy checks the target wire, model family and issuer binding;
a client cannot supply its own provenance. Unknown, changed, expired or
cross-credential items are dropped. Incompatible target reasoning is dropped by
the engine. `metadata.reasoning_dropped` lists drop reasons rather than refusing
the full request. Summary-only external items do not establish provenance.

Use `include: ["reasoning.encrypted_content"]` to request encrypted output from
upstreams that support it. Other providers return replayable receipt-backed
reasoning items without publishing their private thinking text or signatures.
Receipt retention and restoration limits apply as on the Messages facade.

The endpoint remains stateless: `previous_response_id`, `conversation`,
and `store: true` are refused by name. Send explicit full history. Custom and
namespaced tools are supported, including Codex CLI request controls and
`additional_tools` declarations. Hosted search declarations require a native
Responses provider. See [Responses requests](../guides/responses.md) for exact
translation behavior and the accepted controls that may be ignored.

## Agent CLI integration

This endpoint supplies the inference transport for an agent CLI; it does not
launch that CLI or implement a host's parent-turn provider. That integration
belongs in the embedding harness. Its turn provider should launch the CLI with
an isolated home and workspace, direct inference to an authenticated metered
endpoint, and translate the CLI's completion into the parent turn result.
Cancellation must stop the child and its requests; failed or incomplete turns
must remain distinguishable from successful completion. Preserve full request
history, tool inputs, result items and issued reasoning receipts across turns.

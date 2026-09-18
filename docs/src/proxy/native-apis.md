# Native API endpoints

Both `llmshim proxy` and `llmshim gateway` serve:

| Endpoint | Request and response format |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions |
| `POST /v1/messages` | Anthropic Messages |

Point a native SDK at the server base URL and use a configured model route, such
as `anthropic/claude-sonnet-4-6` or `openai/gpt-6-astra`. Provider credentials stay
in the server. On an authenticated gateway, the client key is a gateway key:
`Authorization: Bearer …` and Anthropic's `x-api-key` are accepted. An explicit
Authorization header takes precedence. Native requests use the existing gateway
queue, authenticated tier, quotas and request IDs, or the proxy's concurrency
and rate-limit admission. Rejections preserve HTTP status and Retry-After.
Native idempotency keys are scoped to credential and wire format.

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
batches, token counting, uploads and Responses are not served by these aliases.

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
and credential when replaying native histories across restarts. There is no
automatic expiry. Deleting receipts makes their owned tool IDs unavailable; those
calls fail explicitly. A changed call's arguments cannot reuse an issued receipt.

Untracked native thinking has no provenance and is dropped. Issued reasoning
restores its original typed block and still passes through the common family,
wire and account replay filter. Cross-wire opaque reasoning uses a receipt handle
in native Messages fields so the original block can be restored without pretending
it originated on that wire. Chat Completions includes a `reasoning` extension;
clients that discard it also discard replayable reasoning. Keep full native
assistant messages and tool IDs when saving a conversation. Receipt metadata is
separate from the caller-owned conversation history.

Requests are limited to 2 MiB and buffered metadata/response conversion to 32 MiB.
`x-cache` segment indices on native requests refer to their input `messages`
array; the facade remaps boundaries when system/tool-result conversion expands
that array. `x-shim` applies through the shared client. A signature mismatch's
`x-llmshim-served-model` observation appears in JSON and the HTTP header for unary
responses, or in terminal stream metadata. It never changes message provenance.

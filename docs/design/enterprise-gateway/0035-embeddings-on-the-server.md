# 0035: Embeddings on the server

Status: slice 1 built (embeddings only; images and audio are not built).

## Decision

`llmshim serve` (and `llmshim gateway`) answer `POST /v1/embeddings`, OpenAI-shaped, so an OpenAI SDK's
`client.embeddings.create(...)` works against the gateway unmodified. The library already embeds
(`llmshim::embeddings`, 0.17): this note puts that behind the gateway's admission, not beside it. An
embeddings call is an **attempt** like a completion: it takes the caller's key, the allowlist, the
provider's lane, rate and concurrency limits, the breaker, and it leaves a usage record and a spend
observation. There is no second dispatch, auth or queue path (the rule the native facades follow).

## The contract

- **Request.** `{model, input, dimensions?, encoding_format?}`. `model` is a routing id
  (`openai/text-embedding-3-small`, `embed/<served-model>`); a route or deployment name is not an
  embeddings model and is a 400 (they resolve to chat targets). `input` is a string or an array of
  strings; a token array is a 400 (a vector from a tokenizer llmshim does not know is a wrong answer).
  `encoding_format` is `float` (default) or `base64` (little-endian f32, which the OpenAI SDKs request and
  decode by default); anything else is a 400. Extra fields are ignored.
- **Response.** `{object: "list", data: [{object: "embedding", index, embedding}], model, usage}`, in input
  order. `model` is the model the provider's own response named, else the one requested.
  `usage` is `{prompt_tokens, total_tokens, cost_usd, cost_source}`: `cost_usd` is `null` when the price is
  unknown, never zero (note 0034's rule).
- **Refusals before queueing.** The dispatch's own three checks are asked first, cheap and in the same
  order, so they are a 400 and never a 502 out of the dispatcher: the batch against the provider's bounds, the
  provider's own "no embeddings API" (Anthropic, xAI, OpenRouter and the ChatGPT backend refuse by name,
  never by forwarding to another vendor), and the catalog's embedding assertion for the model and width.
- **Auth and allowlist.** The caller is identified exactly as for chat (401 otherwise). A key with
  `allowed_models` may embed only a listed routing id (403 `model_not_allowed`, before the provider). The
  key's rpm/tpm apply through the same buckets.
- **Spend.** The reservation model is built for the chat wires: it assumes one request fits in one
  context window, which a batch of texts does not, so an embeddings attempt has **no enforceable pre-send
  bound**. A key with a spend cap (its own or its tenant's) is therefore refused with
  `400 unpriceable_under_budget`, **even with `budget_allow_unpriced`**: an unbounded attempt holds the level's
  whole remaining allowance until a provider's own bill settles it, embeddings carry none, and the first call
  would starve the key for the rest of its window (a process test found this: the second call was a 429). A key
  with no cap is served, and its spend is observed and recorded (billed tokens, catalog price). A bound for
  embeddings (batch size times the model's input limit, at the input rate) is the next slice; until then a
  capped key cannot embed.
- **Where it runs.** The local scheduler only. With `[fleet]` the answer is `501 not_implemented`: a
  distributed job carries a chat payload, and running an embeddings one as chat would be a wrong answer.
- **Not built:** images and audio endpoints; chunking or splitting a batch (never: work the caller did not
  ask for); base64 for non-f32; idempotency keys for embeddings; embeddings through a fleet.

### Token accounting on the OpenAI wires (found in review)

An embeddings body counts `prompt_tokens` and `total_tokens` and has no output. The OpenAI Responses
normalization read only `input_tokens` and wrote 0 into `prompt_tokens`, so a usage record for an OpenAI
embedding would have said 0 input tokens next to a correct cost. It now keeps `prompt_tokens` when
`input_tokens` is absent, and an input-only body (input counter equal to the total, no output counter at
all) counts as complete counters on the chat and Responses wires. Pinned by
`integration_serve_embeddings::the_openai_wire_records_the_prompt_tokens_an_embeddings_body_reports`.

## How it is built

An admitted request carries what it is in the **trusted policy context** (`DispatchOperation`, set by the
route from its own state), never in the body: a caller cannot turn a chat body into an embeddings call, or
the reverse, by what it sends. The context stores it in its shared state so the context (carried by every
queued job) does not grow. `RealDispatch::dispatch_with_policy` asks it and calls
`ShimClient::embeddings_with_policy` (new, additive public API: the existing embeddings dispatch with the
attempt policy passed through) instead of the completion path. Everything after the dispatch (usage
recorder, spend observation, breaker, `cost::stamp`) is the code the completion path uses.

## Tests

`integration_serve_embeddings` (process): shape, order and usage record; a single string and base64 and bad
shapes; 401, and 403 for an unlisted model before the provider; a zero rpm refuses before the provider; a
budgeted key is refused and the opt-in serves. Mutations: skipping the allowlist check fails the 403 test;
not marking the request as embeddings fails four of the five (the dispatcher would run it as chat). The 501
for a fleet is **not tested** (it needs a Redis fleet process); the guard is one `matches!` ahead of the
route's first side effect.

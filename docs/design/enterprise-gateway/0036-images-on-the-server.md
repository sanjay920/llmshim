# 0036: Image generation on the server

Status: slice 1 built (image generation; speech and transcription are not built).

## Decision

`POST /v1/images/generations`, OpenAI-shaped, on the gateway, taking the path note 0035 built for
embeddings: an image generation is an **attempt** like a completion, with the caller's key, allowlist, the
provider's lane, rate and concurrency limits, the breaker, a usage record and a spend observation, and no
second dispatch, auth or queue path. The library already generates images (`llmshim::images`, OpenAI's
`gpt-image-*` and Gemini's image output); this puts that behind the gateway's admission.

## The contract

- **Request.** `{model, prompt, ...}`. `model` is a routing id (`openai/gpt-image-1`); a route or deployment
  name is a 400. The fields the provider adapter reads (`n` 1 to 10, `size`, `quality`, `background`,
  `output_format`, `output_compression`, `moderation`, `user` for OpenAI) ride along unchanged and the adapter
  refuses the rest, by name. `response_format` may only be `b64_json` (or absent): generated images are not
  hosted, so `url` is a 400. A model that is not an image model is refused by the adapter (a 400 before the
  provider, never forwarded to another vendor).
- **Response.** `{created, data: [{b64_json, revised_prompt?}], output_format, usage}`. `usage` is the
  provider's counters with `cost_usd` and `cost_source`; **`cost_usd` is `null` when the usage does not split
  text and image tokens**, because the catalog has one rate per direction, not per modality (the library's own
  rule, `images::response`).
- **Refusals before queueing.** The prompt and count, then the adapter's own request refusals, so a request no
  provider could accept is a 400 and never a 502 out of the dispatcher.
- **Auth, allowlist, rate.** As for chat and embeddings: 401, 403 `model_not_allowed` before the provider, the
  key's rpm/tpm through the same buckets.
- **Spend.** A key with a spend cap (its own or its tenant's) **cannot generate images**: the size of the
  output is the model's to choose, so there is no enforceable pre-send bound, and it is refused
  `400 unpriceable_under_budget`, with or without `budget_allow_unpriced` (an unbounded attempt would hold
  the level's whole allowance until a provider bill that never comes, as for embeddings in 0035). A key with
  no cap is served and its spend recorded: the counters come from the native body, the price from the image
  response's own rule (above), not the generic per-direction stamp, which would price an unclassified
  count anyway. A bound for images (the model's maximum output tokens per image, times `n`) is possible and not
  built.
- **Where it runs.** The local scheduler only; a fleet answers `501 not_implemented`.
- **Not built:** speech (`/v1/audio/speech`, binary audio out), transcription (`/v1/audio/transcriptions`,
  multipart in), image edits and variations, hosted URLs, streaming, a spend bound, fleet.

## How it is built

The request's operation lives in the trusted policy context as in 0035 (`DispatchOperation::Images`), never in
the body. `ShimClient::images_with_policy` (new, additive public API) is the image dispatch with the attempt
policy passed through and the attempt reported as it ends: its body is read, the adapter turns it into images
(an invalid answer is reported as such), and the tracker is told the final counters and price. The dispatcher
returns the images as a JSON value (`images[]` with base64 bytes, media type and revised prompt, plus
`usage`) and the route renders OpenAI's shape. Embeddings and images now share one `submit_media_call` (auth,
fleet refusal, resolution, allowlist, route-specific pre-checks, admission); a later audio route uses it too.

## Tests

`integration_serve_images` (process): shape, the usage record's counters and exact price ($0.004025 for 5 text
and 100 image tokens); unclassified usage gives a null price in both the response and the record (replacing the
price with the generic stamp fails it); bad requests (url format, blank prompt, `n` 99, a non-image model, a
route, a deployment) are 400 before the provider; 401, 403 and a zero rpm; capped keys refused, with and without
the opt-in (treating images like embeddings in the quote fails it). The fleet 501 is not tested (it needs a
Redis fleet).

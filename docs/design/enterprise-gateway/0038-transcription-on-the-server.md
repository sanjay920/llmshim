# 0038: Transcription on the server

Status: slice 1 built. With 0035 to 0037 this completes embeddings, images and audio on the server for the
local scheduler.

## Decision

`POST /v1/audio/transcriptions`, OpenAI's multipart upload, on `llmshim serve`, on the path notes 0035 to 0037
built: a transcription is an **attempt** like a completion (key, allowlist, the provider's lane, rate and
concurrency limits, breaker, usage record), the operation carried in the trusted policy context, the route
through the shared `submit_media_call`. An OpenAI SDK's `client.audio.transcriptions.create(...)` works.

This is the one route that needs a new dependency: the multipart reader. It is **`multer` 3, an optional
dependency enabled only by `serve`** (it is the parser axum's own `Multipart` is built on, used directly so the
`proxy` and `gateway` builds do not gain it). Default, `proxy`, `gateway` and `gateway-redis` builds are
unchanged, and so is the route table of a build without `serve`: the route and its entry in the ingress path
list exist only under it. The control plane's lockfile gained the same crates (an optional dependency of a path
dependency is locked).

## The contract

- **Request.** `multipart/form-data` with `file` (the audio), `model` (a routing id) and optionally `language`
  (two lowercase letters), `prompt` (up to 16 KiB), `response_format` (`json` default or `text`; the GPT models
  take only `json`; `verbose_json` is refused by the OpenAI adapter) and `temperature`. `timestamp_granularities[]`
  and `stream` are read and ignored; any other field is a 400. A route or deployment name is a 400, and a model
  that is not a transcription model is refused by the adapter (`whisper-1`, `gpt-4o-transcribe`,
  `gpt-4o-mini-transcribe` for OpenAI).
- **Size.** The audio is at most the library's 25,000,000 bytes. The reader enforces it while streaming, so an
  oversized upload is a **413 before it is queued**, never buffered whole first; the whole stream is bounded to
  that plus 64 KiB of fields. Authentication comes first (the ingress layer), so an unauthenticated caller cannot
  make the gateway read an upload.
- **Response.** `{text, usage}` for `json`, `text/plain` for `text`. `usage` is the provider's counters with
  `cost_usd` and `cost_source`, both `null` when the price cannot be applied (a per-minute model such as
  `whisper-1` has no duration to price, and a token model needs a complete input/output split).
- **Auth, allowlist, rate.** As for the other routes: 401, 403 `model_not_allowed` before the provider, the key's
  rpm/tpm through the same buckets (an upload weighs one permit per KiB).
- **Spend.** A key with a spend cap cannot transcribe: the cost follows the audio's length (or tokens), which its
  compressed bytes do not bound, so it is refused `400 unpriceable_under_budget`, with or without
  `budget_allow_unpriced` (an unbounded attempt would hold the level's whole allowance until a provider bill
  that never comes; 0035). Uncapped keys are served and the attempt recorded with the counters the provider
  reported.
- **Where it runs.** The local scheduler only; a fleet answers `501 not_implemented`.
- **Not built:** `verbose_json` and timestamps, translations (`/v1/audio/translations`), streaming, other
  providers' transcription models beyond what the adapters accept, a spend bound (a caller-measured duration is
  not trusted), fleet.

## How it is built

`DispatchOperation::Transcription` and `ShimClient::transcription_with_policy` (new, additive public API; the
existing `transcription` is the same code with no policy). The upload is read into the library's request, carried
to the dispatcher as a JSON value (the audio as base64 beside its filename, media type and fields; a fleet never
sees it), and decoded again for the send. Held in memory a few times at the 25 MB limit: the bucket weighs the
upload, but the concurrency cap, not the byte count, bounds how many are held at once.

## Tests

`integration_serve_transcription` (process, a real multipart upload from an HTTP client): the transcript, the
record's counters and exact price ($0.000134 for 14 audio and 5 output tokens); `text` answers plain text; bad
uploads (not multipart, no file, no model, a non-transcription model, a bad language, an empty file, a route,
`verbose_json`, a non-numeric temperature) are 400 before the provider; an upload one byte over the limit is a
413 (an unbounded reader fails it); 401, 403 and a zero rpm; capped keys refused with and without the opt-in
(skipping the refusal fails it). The fleet 501 is not tested (it needs a Redis fleet).

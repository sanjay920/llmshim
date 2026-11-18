# 0037: Speech synthesis on the server

Status: slice 1 built (speech; transcription is not built).

## Decision

`POST /v1/audio/speech`, OpenAI-shaped, on the gateway, on the path notes 0035 and 0036 built:
a speech synthesis is an **attempt** like a completion (key, allowlist, the provider's lane, rate and
concurrency limits, breaker, usage record, spend), with the operation carried in the trusted policy context
and the route sharing `submit_media_call`. The library already synthesizes speech (`llmshim::speech`, OpenAI
`tts-1` and `tts-1-hd`); this puts it behind the gateway's admission.

## The contract

- **Request.** `{model, input, voice, response_format?, speed?}`. `input` is 1 to 4096 characters; `voice` is an
  ASCII word; `response_format` is `mp3` (default), `wav`, `flac`, `opus`, `aac` or `pcm`; `speed` is 0.25 to 4;
  `stream` is refused. A route or deployment name is a 400, and a model that is not a speech model is refused
  by the adapter (`tts-1`, `tts-1-hd` for OpenAI), never forwarded to another vendor.
- **Response.** The audio bytes as the body, `content-type` the format's media type (`audio/mpeg`, ...), as the
  OpenAI endpoint answers. No JSON envelope: an OpenAI SDK's `client.audio.speech.create(...)` works.
- **Auth, allowlist, rate.** As for the other routes: 401, 403 `model_not_allowed` before the provider, the key's
  rpm/tpm through the same buckets.
- **Spend, and why speech differs from images and embeddings.** The provider's binary endpoint returns no usage,
  but **the price is fixed by the request**: its characters at the catalog's per-character rate for the model
  (`llmshim_catalog::audio::SPEECH_MODELS`, a builtin and trusted row). So the reservation is **exact**, not a
  bound, and the attempt settles at the same figure with complete counters and `cost_source: "catalog"`: a
  capped key can use speech. A model with no rate has no price and is refused at every capped level
  (`unpriceable_under_budget`). The usage record carries `input_characters`'s price; token counts are zero.
- **Where it runs.** The local scheduler only; a fleet answers `501 not_implemented`.
- **Not built:** transcription (`/v1/audio/transcriptions`, a multipart upload with its own contract), streaming
  audio, other providers' speech, fleet.

## How it is built

`DispatchOperation::Speech` and `ShimClient::speech_with_policy` (new, additive public API): the policy-admitted
send, the body read as bytes, the adapter's media-type check (a wrong or empty audio answer is the provider's
fault and counts toward its breaker, unlike a declined image prompt), and the tracker told the characters, the
exact price and complete counters. The dispatcher returns the audio as base64 beside its media type and usage
(the scheduler carries JSON values); the route decodes it and answers with the bytes. The reservation is
`budget::quote_speech`.

## Limits

The audio is copied several times on the way (read, base64 for the scheduler's JSON value, decoded for the
body); a long synthesis holds a few copies in memory, and the bucket weighs the input's length, not the audio's.

## Tests

`integration_serve_speech` (process): the bytes and content type arrive intact and the record is priced exactly
($0.000165 for 11 characters at $15 per million); bad requests (blank input, a non-word voice, a bad format, a
speed of 9, a non-speech model, a route) are 400 before the provider; 401, 403 and a zero rpm; a cap of 400,000
nanodollars admits two 165,000-nanodollar calls and refuses the third; one request above the cap is refused
before the provider. `gateway::budget::tests::a_speech_reservation_equals_its_settlement_for_every_allowed_length` compares the
reservation with the adapter's own settled cost for every length from 1 to 4096 on both models (an integer
formula fails it); a multibyte input is priced by characters, and `tts-1-hd` by its own rate. A reservation that ignores the characters, or treating speech as unbounded like images,
each fails two of them. The fleet 501 is not tested (it needs a Redis fleet).

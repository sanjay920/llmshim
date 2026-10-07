# Audio

Speech synthesis is available from Rust through `llmshim::speech(router, request)`
and `ShimClient::speech(provider, model, request)`. Router aliases and named routes
work as they do for images. Other providers refuse locally.

```rust,no_run
# async fn example(router: &llmshim::router::Router) -> llmshim::error::Result<()> {
let response = llmshim::speech(router, &serde_json::json!({
    "model": "openai/tts-1",
    "input": "Hello there",
    "voice": "alloy",
    "response_format": "mp3"
})).await?;
std::fs::write("hello.mp3", response.bytes).unwrap();
# Ok(()) }
```

Speech JSON fields:

| Field | Contract |
|---|---|
| `model` | Required router model or alias; OpenAI supports `tts-1` and `tts-1-hd` here. |
| `input` | Required nonblank string of at most 4096 Unicode characters. |
| `voice` | Required nonempty ASCII word of at most 64 letters; provider decides availability. |
| `response_format` | Optional; defaults to `mp3`. |
| `speed` | Optional number from 0.25 to 4; provider default is 1. |
| `stream` | Omit or set to `false`; other values are refused. |

Other speech fields are ignored. OpenAI `tts-1` and `tts-1-hd` use
`/audio/speech`. Inputs contain at most
4096 Unicode characters. Voice names must contain 1 to 64 ASCII letters.
The provider decides which names it supports, so new voices do not need a crate
release; an unknown voice returns the provider's error.
Formats are mp3 (default), wav, flac, opus, aac and pcm. Optional speed ranges
from 0.25 to 4. Streaming is refused. Output carries bytes and media type;
applications own playback. Empty bodies and unexpected content types fail.
The shared transport bounds decoded output and retains its retry, deadline,
redirect refusal and circuit-breaker behavior.

The endpoint returns binary audio without usage counters or a provider bill.
`usage.input_characters` counts submitted Unicode scalar values, including
whitespace; `cost_usd` estimates the charge at $15 or $30 per million characters,
with `cost_source: "catalog"`. These dedicated catalog rows live in
`llmshim_catalog::audio::SPEECH_MODELS`, separate from token pricing.
See OpenAI's [TTS-1 price](https://developers.openai.com/api/docs/models/tts-1) and
[TTS-1 HD price](https://developers.openai.com/api/docs/models/tts-1-hd) and
[speech reference](https://developers.openai.com/api/reference/resources/audio/subresources/speech/methods/create).
There is no proxy, CLI, streaming or dispatch-policy callback surface.

## Transcription

`llmshim::transcription(router, &request)` and
`ShimClient::transcription(provider, model, &request)` accept caller-owned bytes.
The crate never reads a filename as a path. Model aliases work as they do for
embeddings; route settings are not applied to this typed request.

```rust,no_run
# async fn example(router: &llmshim::router::Router, bytes: Vec<u8>) -> llmshim::error::Result<()> {
let mut request = llmshim::audio::TranscriptionRequest::new(
    "openai/gpt-4o-mini-transcribe", bytes, "recording.wav", "audio/wav"
);
request.language = Some("en".into());
let response = llmshim::transcription(router, &request).await?;
println!("{}", response.text);
# Ok(()) }
```

OpenAI `whisper-1`, `gpt-4o-transcribe` and `gpt-4o-mini-transcribe` use
`/audio/transcriptions`. Uploads contain 1 to 25,000,000 bytes (a conservative
decimal interpretation of the published 25 MB limit). The filename is a basename
of at most 255 bytes. A MIME type of at most 128 bytes is checked by reqwest's MIME
parser. Optional language is a two-letter lowercase ISO-639-1 code; prompt is
bounded to 16 KiB; temperature ranges from 0 to 1.

The multipart `file` part retains the supplied bytes, filename and MIME type;
other parts are `model`, `response_format` and the supplied optional controls.
Reqwest builds the form, with a fresh boundary and replayable bytes per retry.
The existing sender handles status errors, retries, header/attempt deadlines and
redirect refusal. JSON and plain-text responses use the shared bounded body
reader with idle/total deadlines and circuit-breaker classification.

`response_format` defaults to `json`. For OpenAI, only `json` and `text` are
exposed here; the GPT transcription models support only `json`, while Whisper
also supports `text`. OpenRouter accepts `json` and `verbose_json`. Plain text must have a `text/plain` media type and valid UTF-8. JSON must
contain a text string. An empty string is valid for silence.

`TranscriptionResponse.usage` preserves native usage. A valid provider
`usage.cost` wins (`cost_source: "provider"`). Otherwise the dedicated catalog
transcription rows price reported tokens (`cost_source: "catalog"`). The
audio-input/text-input/text-output catalog rates per million tokens are $6/$2.50/$10
for GPT-4o Transcribe and $3/$1.25/$5 for Mini Transcribe. Audio and text input
counts are priced separately. Native `type: "tokens"`, integer input/output/total
counts and consistent audio/text input details are required; missing or
inconsistent accounting stays null with `cost_source: "unknown"`. Chat-model
rates are never used to estimate audio costs.

The [OpenAI Cookbook](https://developers.openai.com/cookbook/examples/realtime_out_of_band_transcription)
explicitly separates GPT-4o Transcribe audio and text prices. The model pages
read on 2026-10-01 show a single input table instead. Mini's $3 audio-input
rate retains the previously published price; that separate rate could not be
reconfirmed from the current model page.

Whisper costs $0.006 per minute. Its JSON/text response does not supply duration,
so cost is unknown by default. An optional `duration_seconds` supplied by the
caller enables a catalog estimate, recorded separately as
`usage.caller_duration_seconds`; it is neither provider-measured usage nor sent
upstream. No duration is inferred from byte size or transcript length.

Sources: [transcription reference](https://developers.openai.com/api/reference/resources/audio/subresources/transcriptions/methods/create),
[file transcription limits](https://developers.openai.com/api/docs/guides/speech-to-text),
[GPT-4o Transcribe](https://developers.openai.com/api/docs/models/gpt-4o-transcribe),
[Mini Transcribe](https://developers.openai.com/api/docs/models/gpt-4o-mini-transcribe)
and [Whisper](https://developers.openai.com/api/docs/models/whisper-1).

OpenRouter transcribes through its OpenAI-compatible `/audio/transcriptions`
with the same multipart upload and bounds. Use an OpenRouter slug, such as
`openrouter/openai/whisper-1` or `openrouter/openai/whisper-large-v3`. Slugs are
not checked locally; OpenRouter refuses a model it does not serve. Its schema
has no `prompt` field and answers only `json` or `verbose_json`, so a prompt
and `text` are refused locally. An error inside a 200 answer becomes an error
with OpenRouter's own code and message, as on the chat path. Usage keeps
OpenRouter's `seconds` and token counts. The reported `usage.cost` is the bill
(`cost_source: "provider"`); without it the cost is `null` with
`cost_source: "unknown"`, and no catalog estimate is made.
See OpenRouter's [speech-to-text guide](https://openrouter.ai/docs/guides/overview/multimodal/stt)
and [transcription reference](https://openrouter.ai/docs/api/api-reference/stt/create-transcription).

Other providers refuse locally. Transcription has no proxy/CLI, streaming,
translation, timestamp or dispatch-policy callback surface.

## Gemini transcription design

Gemini audio input requires `generateContent` JSON with base64 `inlineData`,
whereas the implemented transcription preparation returns multipart fields and
one replayable file. Sending that form to Gemini is not a valid protocol, and
forwarding audio as text would silently lose the input. This increment keeps
Gemini's local refusal.

A future preparation variant must carry JSON as well as multipart, while both
continue to build each attempt inside the shared sender. Encode inline audio
only after admission, use a transcription instruction plus caller prompt and
language, require a successful terminal candidate, and collect only non-thought
text. Keep that response's `usageMetadata` with audio/text modality counters;
add published Gemini audio rates to the catalog before estimating a charge.
Reusing text-input rates or inferring duration from bytes is rejected.

Tests must inspect inline bytes and MIME type, pair successful terminal text
with blocked/truncated/thought-only replies, exercise identical transport bounds
and deadlines, and pair priced complete modality counters with mixed, cached,
missing and inconsistent counters. This needs a distinct native preparation and
accounting implementation; the multipart-only seam does not serve it cleanly.

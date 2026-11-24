//! The transcription models OpenRouter documents for `/audio/transcriptions`.

/// `(slug, name)`, both verbatim from OpenRouter's public listing of
/// speech-to-text models, read 2026-10-07 without a key:
/// <https://openrouter.ai/api/v1/models?output_modalities=transcription>,
/// the discovery route its speech-to-text guide names
/// (<https://openrouter.ai/docs/guides/overview/multimodal/stt>). Each name is
/// `"<vendor>: <model>"`.
///
/// `qwen/qwen3-asr-flash-2026-02-10` is left out: the listing gives it an
/// `expiration_date` of 2026-10-09. The list is an offline floor for a person
/// choosing a model, never a gate: a slug that is not here is still sent.
const MODELS: &[(&str, &str)] = &[
    (
        "assemblyai/universal-3-5-pro",
        "AssemblyAI: Universal-3.5 Pro",
    ),
    ("deepgram/nova-3", "Deepgram: Nova-3"),
    ("elevenlabs/scribe-v2", "ElevenLabs: Scribe v2"),
    (
        "elevenlabs/scribe-v2-medical",
        "ElevenLabs: Scribe v2 Medical",
    ),
    ("fish-audio/transcribe-1", "Fish Audio: Transcribe 1"),
    (
        "fish-audio/transcribe-1-pro",
        "Fish Audio: Transcribe 1 Pro",
    ),
    ("google/chirp-3", "Google: Chirp 3"),
    (
        "google/gemini-3.5-transcribe",
        "Google: Gemini 3.5 Transcribe",
    ),
    (
        "meta/muse-voice-transcribe-1.0",
        "Meta: Muse Voice Transcribe 1.0",
    ),
    (
        "microsoft/mai-transcribe-1.5",
        "Microsoft AI: MAI-Transcribe 1.5",
    ),
    (
        "microsoft/mai-transcribe-2",
        "Microsoft AI: MAI-Transcribe 2",
    ),
    (
        "mistralai/voxtral-mini-3b-2507",
        "Mistral: Voxtral Mini 3B 2507",
    ),
    (
        "mistralai/voxtral-mini-transcribe",
        "Mistral: Voxtral Mini Transcribe",
    ),
    (
        "mistralai/voxtral-small-24b-2507-stt",
        "Mistral: Voxtral Small 24B 2507 STT",
    ),
    (
        "nvidia/nemotron-3.5-asr-streaming-multilingual-0.6b",
        "NVIDIA: Nemotron 3.5 ASR Streaming Multilingual 0.6B",
    ),
    (
        "nvidia/parakeet-tdt-0.6b-v3",
        "NVIDIA: Parakeet TDT 0.6B v3",
    ),
    (
        "openai/gpt-4o-mini-transcribe",
        "OpenAI: GPT-4o Mini Transcribe",
    ),
    ("openai/gpt-4o-transcribe", "OpenAI: GPT-4o Transcribe"),
    ("openai/gpt-transcribe", "OpenAI: GPT Transcribe"),
    ("openai/whisper-1", "OpenAI: Whisper 1"),
    ("openai/whisper-large-v3", "OpenAI: Whisper Large V3"),
    (
        "openai/whisper-large-v3-turbo",
        "OpenAI: Whisper Large V3 Turbo",
    ),
    ("qwen/qwen3-asr-0.6b", "Qwen: Qwen3 ASR 0.6B"),
    ("qwen/qwen3-asr-1.7b", "Qwen: Qwen3 ASR 1.7B"),
    ("x-ai/grok-stt-1.0", "SpaceXAI: Grok STT 1.0"),
];

pub(super) fn transcription_models(provider: &str) -> Vec<crate::audio::TranscriptionModelInfo> {
    MODELS
        .iter()
        .map(|(slug, name)| {
            let (vendor, model) = name.split_once(": ").unwrap_or(("", name));
            crate::audio::TranscriptionModelInfo::new(
                provider,
                *slug,
                format!("{model} ({vendor} via OpenRouter)"),
            )
        })
        .collect()
}

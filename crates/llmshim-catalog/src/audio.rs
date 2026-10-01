//! Speech prices use characters, never the token rates in `Cost`.
//! Sources (2026-10-01):
//! https://developers.openai.com/api/docs/models/tts-1
//! https://developers.openai.com/api/docs/models/tts-1-hd

#[derive(Debug, Clone, Copy)]
pub struct SpeechModel {
    pub provider: &'static str,
    pub model: &'static str,
    pub usd_per_million_characters: f64,
}

pub const SPEECH_MODELS: &[SpeechModel] = &[
    SpeechModel {
        provider: "openai",
        model: "tts-1",
        usd_per_million_characters: 15.0,
    },
    SpeechModel {
        provider: "openai",
        model: "tts-1-hd",
        usd_per_million_characters: 30.0,
    },
];

/// Transcription rates, separate from chat-model token pricing.
/// Sources read 2026-10-01:
/// https://developers.openai.com/cookbook/examples/realtime_out_of_band_transcription
/// https://developers.openai.com/api/docs/models/gpt-4o-transcribe,
/// https://developers.openai.com/api/docs/models/gpt-4o-mini-transcribe and
/// https://developers.openai.com/api/docs/models/whisper-1
/// The current model pages collapse input modalities; Mini retains its previously
/// published $3 audio rate, which those pages no longer separately confirm.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptionModel {
    pub model: &'static str,
    pub usd_per_million_audio_input_tokens: Option<f64>,
    pub usd_per_million_text_input_tokens: Option<f64>,
    pub usd_per_million_output_tokens: Option<f64>,
    pub usd_per_minute: Option<f64>,
}

pub const TRANSCRIPTION_MODELS: &[TranscriptionModel] = &[
    TranscriptionModel {
        model: "whisper-1",
        usd_per_million_audio_input_tokens: None,
        usd_per_million_text_input_tokens: None,
        usd_per_million_output_tokens: None,
        usd_per_minute: Some(0.006),
    },
    TranscriptionModel {
        model: "gpt-4o-transcribe",
        usd_per_million_audio_input_tokens: Some(6.0),
        usd_per_million_text_input_tokens: Some(2.5),
        usd_per_million_output_tokens: Some(10.0),
        usd_per_minute: None,
    },
    TranscriptionModel {
        model: "gpt-4o-mini-transcribe",
        usd_per_million_audio_input_tokens: Some(3.0),
        usd_per_million_text_input_tokens: Some(1.25),
        usd_per_million_output_tokens: Some(5.0),
        usd_per_minute: None,
    },
];

//! Non-streaming speech synthesis and transcription with caller-owned audio.
use crate::error::{provider_error as error, Result};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct SpeechResponse {
    pub bytes: Vec<u8>,
    pub media_type: String,
    /// Character count from the submitted text, with an estimated USD cost.
    /// The binary speech endpoint does not return provider usage or a bill.
    pub usage: Value,
}

pub(crate) fn validate(request: &Value) -> Result<()> {
    if !request
        .get("input")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty() && s.chars().count() <= 4096)
    {
        return Err(error(400, "speech input must contain 1 to 4096 characters"));
    }
    if !request
        .get("voice")
        .and_then(Value::as_str)
        .is_some_and(|voice| {
            !voice.is_empty()
                && voice.len() <= 64
                && voice.bytes().all(|byte| byte.is_ascii_alphabetic())
        })
    {
        return Err(error(
            400,
            "speech voice must be an ASCII word of 1 to 64 letters",
        ));
    }
    if !matches!(
        request
            .get("response_format")
            .map_or(Some("mp3"), Value::as_str),
        Some("mp3" | "wav" | "flac" | "opus" | "aac" | "pcm")
    ) {
        return Err(error(400, "unsupported speech format"));
    }
    if request
        .get("speed")
        .is_some_and(|v| !v.as_f64().is_some_and(|n| (0.25..=4.0).contains(&n)))
    {
        return Err(error(400, "speech speed must be from 0.25 to 4"));
    }
    if request
        .get("stream")
        .is_some_and(|v| v != &Value::Bool(false))
    {
        return Err(error(400, "speech streaming is not supported"));
    }
    Ok(())
}

/// Audio supplied by the caller; filenames are metadata, never filesystem paths.
#[derive(Debug, Clone)]
pub struct TranscriptionRequest {
    pub model: String,
    pub bytes: Vec<u8>,
    pub filename: String,
    pub media_type: String,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub response_format: Option<String>,
    pub temperature: Option<f64>,
    /// Optional caller-measured duration for a per-minute catalog estimate.
    /// This is not provider-reported usage and is never sent upstream.
    pub duration_seconds: Option<f64>,
}

impl TranscriptionRequest {
    pub fn new(
        model: impl Into<String>,
        bytes: Vec<u8>,
        filename: impl Into<String>,
        media_type: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            bytes,
            filename: filename.into(),
            media_type: media_type.into(),
            language: None,
            prompt: None,
            response_format: None,
            temperature: None,
            duration_seconds: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptionResponse {
    pub text: String,
    /// Native usage with nullable `cost_usd` and `cost_source` added.
    pub usage: Value,
}

/// Replayable multipart file and provider-prepared string fields.
pub struct TranscriptionUpload {
    pub request: crate::provider::ProviderRequest,
    pub bytes: bytes::Bytes,
    pub filename: String,
    pub media_type: String,
}

/// Build the OpenAI-style multipart upload shared by compatible endpoints:
/// `{base_url}/audio/transcriptions` with a bearer key and string fields.
/// Model and format checks that differ per provider stay with the caller.
pub(crate) fn transcription_upload(
    base_url: &str,
    api_key: &str,
    model: &str,
    request: &TranscriptionRequest,
) -> Result<TranscriptionUpload> {
    validate_transcription(request)?;
    let format = request.response_format.as_deref().unwrap_or("json");
    let mut body = serde_json::json!({"model": model, "response_format": format});
    for (key, value) in [("language", &request.language), ("prompt", &request.prompt)] {
        if let Some(value) = value {
            body[key] = value.clone().into();
        }
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = temperature.to_string().into();
    }
    Ok(TranscriptionUpload {
        request: crate::provider::ProviderRequest {
            url: format!("{}/audio/transcriptions", base_url.trim_end_matches('/')),
            headers: vec![("Authorization".into(), format!("Bearer {api_key}"))],
            body,
        },
        bytes: bytes::Bytes::copy_from_slice(&request.bytes),
        filename: request.filename.clone(),
        media_type: request.media_type.clone(),
    })
}

/// Read the transcript text and native usage (an empty object when absent)
/// from an OpenAI-style JSON answer.
pub(crate) fn transcript(response: Value) -> Result<(String, Value)> {
    let text = response["text"]
        .as_str()
        .ok_or_else(|| error(502, "transcription response has no text string"))?
        .to_owned();
    // Silence can legitimately transcribe to an empty string.
    let mut usage = response["usage"].clone();
    if !usage.is_object() {
        usage = serde_json::json!({});
    }
    Ok((text, usage))
}

/// Conservative decimal interpretation of the provider's published 25 MB limit.
pub const MAX_TRANSCRIPTION_BYTES: usize = 25_000_000;

pub(crate) fn validate_transcription(request: &TranscriptionRequest) -> Result<()> {
    if request.bytes.is_empty() || request.bytes.len() > MAX_TRANSCRIPTION_BYTES {
        return Err(error(
            400,
            "transcription audio must contain 1 to 25000000 bytes (25 MB limit)",
        ));
    }
    if request.filename.is_empty()
        || request.filename.len() > 255
        || request
            .filename
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(error(
            400,
            "transcription filename must be a basename of 1 to 255 bytes",
        ));
    }
    if request.media_type.len() > 128 {
        return Err(error(400, "transcription media type exceeds 128 bytes"));
    }
    if request
        .language
        .as_ref()
        .is_some_and(|s| s.len() != 2 || !s.bytes().all(|b| b.is_ascii_lowercase()))
    {
        return Err(error(
            400,
            "transcription language must be a two-letter ISO-639-1 code",
        ));
    }
    if request.prompt.as_ref().is_some_and(|s| s.len() > 16 * 1024) {
        return Err(error(400, "transcription prompt exceeds 16384 bytes"));
    }
    if !matches!(
        request.response_format.as_deref().unwrap_or("json"),
        "json" | "text" | "verbose_json"
    ) {
        return Err(error(
            400,
            "transcription response format must be json, text or verbose_json",
        ));
    }
    if request
        .temperature
        .is_some_and(|n| !(0.0..=1.0).contains(&n))
    {
        return Err(error(400, "transcription temperature must be from 0 to 1"));
    }
    if request
        .duration_seconds
        .is_some_and(|n| !n.is_finite() || n <= 0.0)
    {
        return Err(error(
            400,
            "transcription duration must be finite and positive",
        ));
    }
    Ok(())
}

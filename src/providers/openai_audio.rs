use super::openai::OpenAi;
use crate::{error::Result, provider::ProviderRequest};
use serde_json::{json, Value};

pub(super) fn transcription_request(
    provider: &OpenAi,
    model: &str,
    request: &crate::audio::TranscriptionRequest,
) -> Result<crate::audio::TranscriptionUpload> {
    use crate::error::provider_error as error;
    crate::audio::validate_transcription(request)?;
    if !llmshim_catalog::audio::TRANSCRIPTION_MODELS
        .iter()
        .any(|row| row.model == model)
    {
        return Err(error(
            400,
            "transcription requires whisper-1, gpt-4o-transcribe or gpt-4o-mini-transcribe",
        ));
    }
    let format = request.response_format.as_deref().unwrap_or("json");
    if model != "whisper-1" && format != "json" {
        return Err(error(
            400,
            "GPT transcription models require json response format",
        ));
    }
    let mut body = json!({"model": model, "response_format": format});
    for (key, value) in [("language", &request.language), ("prompt", &request.prompt)] {
        if let Some(value) = value {
            body[key] = value.clone().into();
        }
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = temperature.to_string().into();
    }
    Ok(crate::audio::TranscriptionUpload {
        request: ProviderRequest {
            url: format!(
                "{}/audio/transcriptions",
                provider.base_url.trim_end_matches('/')
            ),
            headers: vec![(
                "Authorization".into(),
                format!("Bearer {}", provider.api_key),
            )],
            body,
        },
        bytes: bytes::Bytes::copy_from_slice(&request.bytes),
        filename: request.filename.clone(),
        media_type: request.media_type.clone(),
    })
}

pub(super) fn transcription_response(
    model: &str,
    request: &crate::audio::TranscriptionRequest,
    response: Value,
) -> Result<crate::audio::TranscriptionResponse> {
    use crate::error::provider_error as error;
    let text = response["text"]
        .as_str()
        .ok_or_else(|| error(502, "transcription response has no text string"))?
        .to_owned();
    // Silence can legitimately transcribe to an empty string.
    let mut usage = response["usage"].clone();
    if !usage.is_object() {
        usage = json!({});
    }
    let reported = crate::cost::reported(&usage);
    let rates = llmshim_catalog::audio::TRANSCRIPTION_MODELS
        .iter()
        .find(|row| row.model == model);
    let estimate = transcription_cost(rates, request, &usage);
    if rates.is_some_and(|row| row.usd_per_minute.is_some()) {
        if let Some(seconds) = request
            .duration_seconds
            .filter(|n| n.is_finite() && *n > 0.0)
        {
            usage["caller_duration_seconds"] = seconds.into();
        }
    }
    usage["cost_usd"] = reported.or(estimate).map_or(Value::Null, Value::from);
    usage["cost_source"] = if reported.is_some() {
        "provider"
    } else if estimate.is_some() {
        "catalog"
    } else {
        "unknown"
    }
    .into();
    Ok(crate::audio::TranscriptionResponse { text, usage })
}

pub(super) fn speech_request(
    provider: &OpenAi,
    model: &str,
    request: &Value,
) -> Result<ProviderRequest> {
    crate::audio::validate(request)?;
    if !matches!(model, "tts-1" | "tts-1-hd") {
        return Err(crate::error::provider_error(
            400,
            "speech synthesis requires tts-1 or tts-1-hd",
        ));
    }
    let mut body = json!({
        "model": model,
        "input": request["input"],
        "voice": request["voice"],
        "response_format": request.get("response_format").cloned().unwrap_or(json!("mp3"))
    });
    if let Some(speed) = request.get("speed") {
        body["speed"] = speed.clone();
    }
    Ok(ProviderRequest {
        url: format!("{}/audio/speech", provider.base_url.trim_end_matches('/')),
        headers: vec![(
            "Authorization".into(),
            format!("Bearer {}", provider.api_key),
        )],
        body,
    })
}

pub(super) fn speech_response(
    model: &str,
    request: &Value,
    bytes: Vec<u8>,
    media_type: &str,
) -> Result<crate::audio::SpeechResponse> {
    use crate::error::provider_error as error;
    let format = request["response_format"].as_str().unwrap_or("mp3");
    let expected = match format {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "opus" => "audio/ogg",
        "aac" => "audio/aac",
        "pcm" => "audio/pcm",
        _ => return Err(error(502, "unsupported speech format")),
    };
    // OpenAI may label speech as an opaque binary stream. The requested
    // format is authoritative only for this documented binary endpoint.
    if bytes.is_empty()
        || !(media_type.eq_ignore_ascii_case(expected)
            || media_type.eq_ignore_ascii_case("application/octet-stream"))
    {
        return Err(error(502, "empty speech or unexpected speech media type"));
    }
    let characters = request["input"]
        .as_str()
        .ok_or_else(|| error(400, "missing speech input"))?
        .chars()
        .count();
    let rate = llmshim_catalog::audio::SPEECH_MODELS
        .iter()
        .find(|row| row.model == model);
    let cost = rate.map(|row| characters as f64 * row.usd_per_million_characters / 1_000_000.0);
    Ok(crate::audio::SpeechResponse {
        bytes,
        media_type: expected.into(),
        usage: json!({"input_characters":characters,"cost_usd":cost,"cost_source":if cost.is_some(){"catalog"}else{"unknown"}}),
    })
}

/// Returns no estimate without rates, caller duration for per-minute models,
/// or complete token accounting with consistent input and total counts.
fn transcription_cost(
    rates: Option<&llmshim_catalog::audio::TranscriptionModel>,
    request: &crate::audio::TranscriptionRequest,
    usage: &Value,
) -> Option<f64> {
    let rates = rates?;
    if let Some(rate) = rates.usd_per_minute {
        let seconds = request
            .duration_seconds
            .filter(|n| n.is_finite() && *n > 0.0)?;
        return Some(seconds / 60.0 * rate);
    }
    if usage["type"] != "tokens" {
        return None;
    }
    let input = usage["input_tokens"].as_u64()?;
    let output = usage["output_tokens"].as_u64()?;
    let audio = usage
        .pointer("/input_token_details/audio_tokens")?
        .as_u64()?;
    let text = usage
        .pointer("/input_token_details/text_tokens")?
        .as_u64()?;
    if audio.checked_add(text)? != input
        || input.checked_add(output)? != usage["total_tokens"].as_u64()?
    {
        return None;
    }
    Some(
        (audio as f64 * rates.usd_per_million_audio_input_tokens?
            + text as f64 * rates.usd_per_million_text_input_tokens?
            + output as f64 * rates.usd_per_million_output_tokens?)
            / 1_000_000.0,
    )
}

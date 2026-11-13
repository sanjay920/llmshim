//! Legacy subscription dictation uses the selected account and the existing bounded upload client.
//! Wire contract from openai/codex, Apache-2.0, commit 4e119a3b38e4a4decfccb003acecabc4614142b6,
//! codex-rs/tui/src/voice.rs:787-867.
//! https://github.com/openai/codex/blob/4e119a3b38e4a4decfccb003acecabc4614142b6/codex-rs/tui/src/voice.rs
use super::{auth::auth_error, tokens::Tokens, ChatGpt};
use crate::audio::{TranscriptionRequest, TranscriptionResponse, TranscriptionUpload};
use crate::{error::Result, provider::ProviderRequest};
use serde_json::{json, Value};

pub(super) fn endpoint(
    provider: &ChatGpt,
    model: &str,
    request: &TranscriptionRequest,
) -> Result<String> {
    crate::audio::validate_transcription(request)?;
    if model != "transcribe" {
        return Err(auth_error(
            400,
            "use chatgpt/transcribe for subscription dictation",
        ));
    }
    if request.media_type != "audio/wav"
        || request.language.is_some()
        || request.prompt.is_some()
        || request.temperature.is_some()
        || request
            .response_format
            .as_deref()
            .is_some_and(|format| format != "json")
    {
        return Err(auth_error(
            400,
            "ChatGPT dictation accepts WAV audio and JSON output only",
        ));
    }
    let mut url = reqwest::Url::parse(&provider.base_url)
        .map_err(|_| auth_error(400, "invalid ChatGPT transcription base URL"))?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(auth_error(400, "invalid ChatGPT transcription base URL"));
    }
    let codex_base = url.path().trim_end_matches('/').ends_with("/codex");
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| auth_error(400, "invalid ChatGPT transcription base URL"))?;
        segments.pop_if_empty();
        if codex_base {
            segments.pop();
        }
        segments.push("transcribe");
    }
    Ok(url.into())
}

pub(super) fn upload(
    provider: &ChatGpt,
    request: &TranscriptionRequest,
    endpoint: String,
    tokens: Tokens,
) -> Result<TranscriptionUpload> {
    let mut headers = vec![
        (
            "Authorization".into(),
            format!("Bearer {}", tokens.access_token),
        ),
        ("User-Agent".into(), provider.user_agent.clone()),
    ];
    if let Some(account) = tokens.account_id.filter(|value| !value.is_empty()) {
        headers.push(("ChatGPT-Account-Id".into(), account));
    }
    Ok(TranscriptionUpload {
        request: ProviderRequest {
            url: endpoint,
            headers,
            body: json!({}),
        },
        bytes: request.bytes.clone().into(),
        filename: request.filename.clone(),
        media_type: request.media_type.clone(),
    })
}

pub(super) fn response(response: Value) -> Result<TranscriptionResponse> {
    let text = response
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| auth_error(502, "ChatGPT transcription returned no words"))?;
    // The backend can return an asset reference and retention metadata. Keep both out of
    // normalized usage. This route supplies no model identity or billing measurements.
    Ok(TranscriptionResponse {
        text: text.into(),
        usage: json!({"cost_usd": null, "cost_source": null}),
    })
}

//! Non-streaming image generation. Applications own saving or displaying bytes.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;

use crate::error::{Result, ShimError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub revised_prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImageResponse {
    pub images: Vec<GeneratedImage>,
    /// Provider-native counters, with `cost_usd` and `cost_source` added.
    /// An unknown price is null, never an invented zero.
    pub usage: Value,
}

pub(crate) fn error(status: u16, body: &str) -> ShimError {
    ShimError::ProviderError {
        status,
        body: body.into(),
        retry_after: None,
    }
}

pub(crate) fn validate(request: &Value) -> Result<()> {
    if !request
        .get("prompt")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
    {
        return Err(error(400, "image prompt must be a nonempty string"));
    }
    if request
        .get("n")
        .is_some_and(|n| !n.as_u64().is_some_and(|n| (1..=10).contains(&n)))
    {
        return Err(error(400, "image count must be an integer from 1 to 10"));
    }
    if request
        .get("stream")
        .is_some_and(|v| v != &Value::Bool(false))
    {
        return Err(error(400, "image streaming is not supported"));
    }
    Ok(())
}

pub(crate) fn decode(
    encoded: Option<&str>,
    media_type: &str,
    revised_prompt: Option<&str>,
) -> Result<GeneratedImage> {
    if !matches!(media_type, "image/png" | "image/jpeg" | "image/webp") {
        return Err(error(502, "unsupported generated image media type"));
    }
    let bytes = STANDARD
        .decode(encoded.ok_or_else(|| error(502, "missing generated image bytes"))?)
        .map_err(|_| error(502, "invalid generated image base64"))?;
    if bytes.is_empty() {
        return Err(error(502, "empty generated image bytes"));
    }
    Ok(GeneratedImage {
        bytes,
        media_type: media_type.into(),
        revised_prompt: revised_prompt.map(str::to_owned),
    })
}

pub(crate) fn response(
    images: Vec<GeneratedImage>,
    mut usage: Value,
    provider: &str,
    model: &str,
) -> Result<ImageResponse> {
    if images.is_empty() {
        return Err(error(502, "provider returned no generated images"));
    }
    if !usage.is_object() {
        usage = serde_json::json!({});
    }
    let reported = crate::cost::reported(&usage);
    // The catalog has one rate per direction, not per modality. Require fully
    // classified text input and image output before applying those rates.
    let estimate = (|| {
        let rates = crate::cost::for_target(provider, model)?;
        let (input, output) = if provider == "gemini" {
            if ["cachedContentTokenCount", "thoughtsTokenCount"]
                .iter()
                .any(|key| usage.get(key).is_some_and(|n| n.as_u64() != Some(0)))
            {
                return None;
            }
            let mut counts = [0_u64; 2];
            for (index, (details, total, modality)) in [
                ("promptTokensDetails", "promptTokenCount", "TEXT"),
                ("candidatesTokensDetails", "candidatesTokenCount", "IMAGE"),
            ]
            .into_iter()
            .enumerate()
            {
                for part in usage[details].as_array()? {
                    let count = part["tokenCount"].as_u64()?;
                    let kind = part["modality"].as_str()?;
                    if kind != modality && count != 0 {
                        return None;
                    }
                    counts[index] = counts[index].checked_add(count)?;
                }
                if counts[index] != usage[total].as_u64()? {
                    return None;
                }
            }
            (counts[0], counts[1])
        } else {
            let input = usage["input_tokens"].as_u64()?;
            let text = usage
                .pointer("/input_tokens_details/text_tokens")?
                .as_u64()?;
            let image = usage
                .pointer("/input_tokens_details/image_tokens")?
                .as_u64()?;
            if image != 0
                || input != text
                || usage
                    .pointer("/input_tokens_details/cached_tokens")
                    .is_some_and(|n| n.as_u64() != Some(0))
            {
                return None;
            }
            let output = usage["output_tokens"].as_u64()?;
            let images = usage
                .pointer("/output_tokens_details/image_tokens")?
                .as_u64()?;
            if output != images
                || usage
                    .pointer("/output_tokens_details/text_tokens")
                    .is_some_and(|n| n.as_u64() != Some(0))
            {
                return None;
            }
            (input, output)
        };
        if input == 0 && output == 0 {
            return None;
        }
        Some((input as f64 * rates.input? + output as f64 * rates.output?) / 1_000_000.0)
    })();
    usage["cost_usd"] = reported.or(estimate).map_or(Value::Null, Value::from);
    usage["cost_source"] = if reported.is_some() {
        "provider"
    } else if estimate.is_some() {
        crate::cost::SOURCE_CATALOG
    } else {
        "unknown"
    }
    .into();
    Ok(ImageResponse { images, usage })
}

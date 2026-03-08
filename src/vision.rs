//! Vision/image content block translation between providers.
//!
//! Canonical input format (OpenAI-style):
//! ```json
//! {"type": "image_url", "image_url": {"url": "https://..." or "data:image/jpeg;base64,..."}}
//! ```
//! or (Responses API):
//! ```json
//! {"type": "input_image", "image_url": "https://..." or "data:image/jpeg;base64,..."}
//! ```

use serde_json::{json, Value};

/// Parse a data URI into (media_type, base64_data).
/// Returns None if not a data URI.
fn parse_data_uri(url: &str) -> Option<(&str, &str)> {
    let stripped = url.strip_prefix("data:")?;
    let (header, data) = stripped.split_once(",")?;
    let media_type = header.strip_suffix(";base64")?;
    Some((media_type, data))
}

/// Translate image content blocks from any format to Anthropic format.
/// Handles OpenAI Chat Completions, Responses API, and passthrough.
pub fn to_anthropic(block: &Value) -> Option<Value> {
    let block_type = block.get("type").and_then(|t| t.as_str())?;

    match block_type {
        // OpenAI Chat Completions format: {"type": "image_url", "image_url": {"url": "..."}}
        "image_url" => {
            let url = block.pointer("/image_url/url").and_then(|u| u.as_str())?;
            Some(url_to_anthropic(url))
        }

        // OpenAI Responses API format: {"type": "input_image", "image_url": "..."}
        "input_image" => {
            let url = block.get("image_url").and_then(|u| u.as_str())?;
            Some(url_to_anthropic(url))
        }

        // Already Anthropic format
        "image" => Some(block.clone()),

        _ => None,
    }
}

fn url_to_anthropic(url: &str) -> Value {
    if let Some((media_type, data)) = parse_data_uri(url) {
        json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data
            }
        })
    } else {
        json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": url
            }
        })
    }
}

/// Translate image content blocks from any format to Gemini format.
pub fn to_gemini(block: &Value) -> Option<Value> {
    let block_type = block.get("type").and_then(|t| t.as_str())?;

    match block_type {
        // OpenAI Chat Completions: {"type": "image_url", "image_url": {"url": "..."}}
        "image_url" => {
            let url = block.pointer("/image_url/url").and_then(|u| u.as_str())?;
            Some(url_to_gemini(url))
        }

        // OpenAI Responses API: {"type": "input_image", "image_url": "..."}
        "input_image" => {
            let url = block.get("image_url").and_then(|u| u.as_str())?;
            Some(url_to_gemini(url))
        }

        // Anthropic format: {"type": "image", "source": {"type": "base64", ...}}
        "image" => {
            let source = block.get("source")?;
            match source.get("type").and_then(|t| t.as_str())? {
                "base64" => Some(json!({
                    "inline_data": {
                        "mime_type": source.get("media_type").and_then(|m| m.as_str()).unwrap_or("image/jpeg"),
                        "data": source.get("data").and_then(|d| d.as_str()).unwrap_or("")
                    }
                })),
                "url" => {
                    // Gemini doesn't support URL images inline — pass through as text note
                    let url = source.get("url").and_then(|u| u.as_str()).unwrap_or("");
                    Some(json!({"text": format!("[Image: {}]", url)}))
                }
                _ => None,
            }
        }

        _ => None,
    }
}

fn url_to_gemini(url: &str) -> Value {
    if let Some((media_type, data)) = parse_data_uri(url) {
        json!({
            "inline_data": {
                "mime_type": media_type,
                "data": data
            }
        })
    } else {
        // Gemini doesn't support URL images inline
        json!({"text": format!("[Image: {}]", url)})
    }
}

/// Translate image content blocks from any format to OpenAI Responses API format.
pub fn to_openai(block: &Value) -> Option<Value> {
    let block_type = block.get("type").and_then(|t| t.as_str())?;

    match block_type {
        // Already OpenAI Responses API format
        "input_image" => Some(block.clone()),

        // OpenAI Chat Completions format → Responses API format
        "image_url" => {
            let url = block.pointer("/image_url/url").and_then(|u| u.as_str())?;
            Some(json!({
                "type": "input_image",
                "image_url": url
            }))
        }

        // Anthropic format → OpenAI format
        "image" => {
            let source = block.get("source")?;
            match source.get("type").and_then(|t| t.as_str())? {
                "base64" => {
                    let media_type = source
                        .get("media_type")
                        .and_then(|m| m.as_str())
                        .unwrap_or("image/jpeg");
                    let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
                    Some(json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", media_type, data)
                    }))
                }
                "url" => {
                    let url = source.get("url").and_then(|u| u.as_str())?;
                    Some(json!({
                        "type": "input_image",
                        "image_url": url
                    }))
                }
                _ => None,
            }
        }

        _ => None,
    }
}

/// Translate all content blocks in a message's content array.
/// If content is a string, returns it unchanged.
/// If content is an array, translates image blocks using the given translator.
pub fn translate_content_blocks(content: &Value, translator: fn(&Value) -> Option<Value>) -> Value {
    match content {
        Value::Array(blocks) => {
            let translated: Vec<Value> = blocks
                .iter()
                .map(|block| {
                    let block_type = block.get("type").and_then(|t| t.as_str());
                    match block_type {
                        Some("text") => block.clone(),
                        Some("image_url" | "input_image" | "image") => {
                            translator(block).unwrap_or_else(|| block.clone())
                        }
                        _ => block.clone(), // unknown types pass through
                    }
                })
                .collect();
            Value::Array(translated)
        }
        _ => content.clone(), // string or null — pass through
    }
}

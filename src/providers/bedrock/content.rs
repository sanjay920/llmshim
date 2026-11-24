//! Text blocks shared by conversation content and tool-result content.
use super::request::invalid;
use crate::error::Result;
use serde_json::{json, Value};

pub(super) fn tool_result(value: &Value) -> Result<Vec<Value>> {
    let blocks = blocks(value)?;
    if blocks.is_empty() {
        return Err(invalid("Bedrock tool content requires nonempty text"));
    }
    Ok(blocks)
}

pub(super) fn blocks(value: &Value) -> Result<Vec<Value>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    if let Some(text) = value.as_str() {
        return Ok(if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"text":text})]
        });
    }
    let mut blocks = Vec::new();
    for part in value
        .as_array()
        .ok_or_else(|| invalid("content must be text or blocks"))?
    {
        if part
            .get("cache_control")
            .is_some_and(|value| !value.is_null())
        {
            return Err(invalid(
                "Bedrock content cache_control requires native cachePoint blocks",
            ));
        }
        match part["type"].as_str() {
            Some("text" | "input_text") => {
                let text = part["text"]
                    .as_str()
                    .ok_or_else(|| invalid("text must be a string"))?;
                if !text.is_empty() {
                    blocks.push(json!({"text":text}));
                }
            }
            kind => {
                return Err(invalid(&format!(
                    "unsupported Bedrock content type: {}",
                    kind.unwrap_or("missing type")
                )))
            }
        }
    }
    Ok(blocks)
}

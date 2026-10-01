//! Responses input items translated into explicit message history.
use super::{array, Result};
use serde_json::{json, Value};

/// Accepts string input or message/call/output items and refuses other item kinds.
pub(super) fn messages(native: &Value) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    if let Some(instructions) = native.get("instructions") {
        let text = instructions
            .as_str()
            .ok_or("instructions must be a string")?;
        messages.push(json!({"role": "system", "content": text}));
    }
    if let Some(text) = native["input"].as_str() {
        messages.push(json!({"role": "user", "content": text}));
        return Ok(messages);
    }
    let mut calls = Vec::new();
    for item in array(&native["input"], "input")? {
        let kind = match item.get("type") {
            None => "message",
            Some(value) => value.as_str().ok_or("input item type must be a string")?,
        };
        if kind == "function_call" {
            calls.push(function_call(item)?);
            continue;
        }
        flush_function_calls(&mut calls, &mut messages);
        messages.push(match kind {
            "message" => message(item)?,
            "function_call_output" => function_call_output(item)?,
            kind => return Err(format!("unsupported Responses input item: {kind}")),
        });
    }
    flush_function_calls(&mut calls, &mut messages);
    Ok(messages)
}

/// Accepts adjacent function calls as one assistant turn and omits empty call groups.
fn flush_function_calls(calls: &mut Vec<Value>, messages: &mut Vec<Value>) {
    // Separate assistant turns would make parallel call results look orphaned.
    if !calls.is_empty() {
        messages.push(json!({
            "role": "assistant",
            "content": null,
            "tool_calls": std::mem::take(calls),
        }));
    }
}

/// Accepts Responses message roles with string/part content and refuses other roles.
fn message(item: &Value) -> Result<Value> {
    let role = item["role"]
        .as_str()
        .ok_or("input message role is required")?;
    if !matches!(role, "user" | "assistant" | "system" | "developer") {
        return Err(format!("unsupported input message role: {role}"));
    }
    Ok(json!({"role": role, "content": content_parts(&item["content"])?}))
}

/// Accepts text and URL-image parts and refuses missing payloads and unknown part types.
fn content_parts(content: &Value) -> Result<Value> {
    if content.is_string() {
        return Ok(content.clone());
    }
    let mut parts = Vec::new();
    for part in array(content, "input message content")? {
        parts.push(match part["type"].as_str() {
            Some("input_text" | "output_text") => {
                let text = part["text"].as_str().ok_or("text part requires text")?;
                json!({"type": "text", "text": text})
            }
            Some("input_image") => {
                let url = part["image_url"]
                    .as_str()
                    .ok_or("input_image requires image_url")?;
                let mut image = json!({"url": url});
                if let Some(detail) = part.get("detail") {
                    if !matches!(detail.as_str(), Some("auto" | "low" | "high")) {
                        return Err("invalid input_image detail".into());
                    }
                    image["detail"] = detail.clone();
                }
                json!({"type": "image_url", "image_url": image})
            }
            _ => return Err(format!("unsupported input part: {}", part["type"])),
        });
    }
    Ok(json!(parts))
}

/// Accepts function calls with nonempty call_id/name and string arguments; refuses missing fields.
fn function_call(item: &Value) -> Result<Value> {
    let id = item["call_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("function_call requires call_id")?;
    let name = item["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("function_call requires name")?;
    let arguments = item["arguments"]
        .as_str()
        .ok_or("function_call requires arguments text")?;
    Ok(json!({
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    }))
}

/// Accepts function outputs with nonempty call_id and string output; refuses missing fields.
fn function_call_output(item: &Value) -> Result<Value> {
    let id = item["call_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("function_call_output requires call_id")?;
    let output = item["output"]
        .as_str()
        .ok_or("function_call_output requires output text")?;
    Ok(json!({"role": "tool", "tool_call_id": id, "content": output}))
}

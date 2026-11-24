//! Responses input items translated into explicit message history.
use super::{array, Result};
use serde_json::{json, Value};

/// Accepts string input or message/call/output/reasoning items and refuses other item kinds.
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
        if kind == "additional_tools" {
            continue;
        }
        if matches!(kind, "function_call" | "custom_tool_call") {
            calls.push(function_call(item)?);
            continue;
        }
        flush_function_calls(&mut calls, &mut messages);
        messages.push(match kind {
            "message" => message(item)?,
            "function_call_output" | "custom_tool_call_output" => function_call_output(item)?,
            "reasoning" => reasoning(item)?,
            "tool_search_call" | "tool_search_output" | "web_search_call" => {
                if kind == "tool_search_call" && !item["arguments"].is_object() {
                    return Err("tool_search_call requires arguments object".into());
                }
                if kind == "tool_search_output" {
                    array(&item["tools"], "tool_search_output.tools")?;
                }
                json!({"role": "developer", "content": "", "x-responses-item": item})
            }
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
    let mut message = json!({"role": role, "content": content_parts(&item["content"])?});
    if let Some(phase) = item.get("phase") {
        if !matches!(phase.as_str(), Some("commentary" | "final_answer")) {
            return Err("invalid message phase".into());
        }
        message["phase"] = phase.clone();
    }
    Ok(message)
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
                    if !matches!(detail.as_str(), Some("auto" | "low" | "high" | "original")) {
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
    let arguments = if item["type"] == "custom_tool_call" {
        let input = item["input"]
            .as_str()
            .ok_or("custom_tool_call requires input text")?;
        json!({"input": input}).to_string()
    } else {
        item["arguments"]
            .as_str()
            .ok_or("function_call requires arguments text")?
            .to_owned()
    };
    if item.get("namespace").is_some_and(|namespace| {
        !namespace
            .as_str()
            .is_some_and(|namespace| !namespace.is_empty())
    }) {
        return Err("call namespace must be a nonempty string".into());
    }
    let name = crate::responses_tools::name(name, item["namespace"].as_str());
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
    let output = &item["output"];
    if output.is_string() {
        return Ok(json!({"role": "tool", "tool_call_id": id, "content": output}));
    }
    if !output.is_array() {
        return Err("function_call_output requires output text or content array".into());
    }
    let mut content = Vec::new();
    for part in array(output, "tool output")? {
        content.push(match part["type"].as_str() {
            Some("input_text" | "input_image") => content_parts(&json!([part]))?[0].clone(),
            Some("input_audio") if part["audio_url"].is_string() => part.clone(),
            Some("encrypted_content") if part["encrypted_content"].is_string() => part.clone(),
            _ => return Err("malformed tool output content".into()),
        });
    }
    Ok(json!({"role": "tool", "tool_call_id": id,
        "content": content, "x-responses-output": output}))
}

/// Keep the issued item intact until credential-scoped restoration.
fn reasoning(item: &Value) -> Result<Value> {
    for part in array(&item["summary"], "reasoning.summary")? {
        if part["type"] != "summary_text" || !part["text"].is_string() {
            return Err("malformed reasoning.summary".into());
        }
    }
    if item
        .get("encrypted_content")
        .is_some_and(|value| !value.is_string())
    {
        return Err("malformed reasoning.encrypted_content".into());
    }
    if item.get("id").is_some_and(|value| !value.is_string()) {
        return Err("malformed reasoning.id".into());
    }
    Ok(json!({"role": "assistant","content": null,"x-responses-reasoning": item}))
}

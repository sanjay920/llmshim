//! Native blocks retain signed reasoning and tool identity through shared Messages semantics.
use super::BedrockError;
use crate::{error::Result, reasoning::ReplayTarget};
use serde_json::{json, Value};

pub(super) fn stop_reason(value: &Value) -> std::result::Result<&'static str, BedrockError> {
    match value.as_str() {
        Some("end_turn" | "stop_sequence") => Ok("stop"),
        Some("max_tokens" | "model_context_window_exceeded") => Ok("length"),
        Some("tool_use") => Ok("tool_calls"),
        Some("guardrail_intervened" | "content_filtered") => Ok("content_filter"),
        Some(code @ ("malformed_model_output" | "malformed_tool_use")) => {
            Err(BedrockError::service(502, code))
        }
        _ => Err(BedrockError::protocol()),
    }
}

pub(super) fn usage(native: &Value) -> Value {
    let mut usage = json!({"bedrock":native});
    for (source, target) in [
        ("inputTokens", "input_tokens"),
        ("outputTokens", "output_tokens"),
        ("cacheReadInputTokens", "cache_read_input_tokens"),
        ("cacheWriteInputTokens", "cache_creation_input_tokens"),
    ] {
        if let Some(count) = native[source].as_u64() {
            usage[target] = json!(count);
        }
    }
    usage
}

pub(super) fn block(native: &Value) -> Result<Value> {
    if let Some(text) = native.get("text") {
        if !text.is_string() {
            return Err(BedrockError::protocol().into_shim(None));
        }
        return Ok(json!({"type":"text","text":text}));
    }
    if let Some(tool) = native.get("toolUse") {
        if !tool["toolUseId"].is_string() || !tool["name"].is_string() || !tool["input"].is_object()
        {
            return Err(BedrockError::protocol().into_shim(None));
        }
        return Ok(
            json!({"type":"tool_use","id":tool["toolUseId"],"name":tool["name"],
            "input":tool["input"]}),
        );
    }
    if let Some(reasoning) = native.get("reasoningContent") {
        if let Some(redacted) = reasoning.get("redactedContent") {
            return Ok(json!({"type":"redacted_thinking","data":redacted}));
        }
        if !reasoning["reasoningText"]["text"].is_string() {
            return Err(BedrockError::protocol().into_shim(None));
        }
        let mut block = json!({"type":"thinking","thinking":reasoning["reasoningText"]["text"]});
        if let Some(signature) = reasoning["reasoningText"].get("signature") {
            if !signature.is_string() {
                return Err(BedrockError::protocol().into_shim(None));
            }
            block["signature"] = signature.clone();
        }
        return Ok(block);
    }
    Err(BedrockError::protocol().into_shim(None))
}

pub(super) fn normalize(target: &ReplayTarget, response: Value) -> Result<Value> {
    let reason = stop_reason(&response["stopReason"]).map_err(|error| error.into_shim(None))?;
    let content = response
        .pointer("/output/message/content")
        .and_then(Value::as_array)
        .ok_or_else(|| BedrockError::protocol().into_shim(None))?;
    let blocks = content.iter().map(block).collect::<Result<Vec<_>>>()?;
    let native_usage = usage(&response["usage"]);
    let input = native_usage["input_tokens"].as_u64().unwrap_or(0);
    let output = native_usage["output_tokens"].as_u64().unwrap_or(0);
    let read = native_usage["cache_read_input_tokens"]
        .as_u64()
        .unwrap_or(0);
    let write = native_usage["cache_creation_input_tokens"]
        .as_u64()
        .unwrap_or(0);
    let prompt = input.saturating_add(read).saturating_add(write);
    let mut usage = json!({"prompt_tokens":prompt,"completion_tokens":output,
        "total_tokens":prompt.saturating_add(output),"bedrock":response["usage"]});
    crate::usage::normalize_cache(&native_usage, &mut usage);
    let text: String = blocks
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect();
    let calls = blocks
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| {
            json!({"id":block["id"],"type":"function","function":{"name":block["name"],
            "arguments":block["input"].to_string()}})
        })
        .collect::<Vec<_>>();
    let content = if text.is_empty() {
        Value::Null
    } else {
        json!(text)
    };
    let mut message = json!({"role":"assistant","content":content});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    let mut normalized = json!({"id":"","object":"chat.completion","model":target.model,
        "choices":[{"index":0,"message":message,"finish_reason":reason}],"usage":usage});
    let semantic = json!({"content":blocks});
    crate::derived_response::capture_unary(target, &semantic, &mut normalized)?;
    Ok(normalized)
}

/// Bedrock input excludes both cache buckets; the OpenAI prompt total includes them.
pub(super) fn project_usage(native: &Value, normalized: &mut Value) {
    if normalized["usage"].is_object() {
        let usage = &mut normalized["usage"];
        let prompt = [
            "uncached_input_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
        ]
        .into_iter()
        .fold(0u64, |sum, key| {
            sum.saturating_add(usage[key].as_u64().unwrap_or(0))
        });
        usage["prompt_tokens"] = json!(prompt);
        usage["total_tokens"] =
            json!(prompt.saturating_add(usage["completion_tokens"].as_u64().unwrap_or(0)));
        if let Some(raw) = native.pointer("/usage/bedrock") {
            usage["bedrock"] = raw.clone();
        }
    }
    if native.pointer("/delta/bedrock_stop").is_some_and(|stop| {
        matches!(
            stop.as_str(),
            Some("guardrail_intervened" | "content_filtered")
        )
    }) {
        normalized["choices"][0]["finish_reason"] = json!("content_filter");
    }
}

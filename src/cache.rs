//! Caller-owned stability annotations, provider-owned placement mechanics.
use crate::{
    error::{Result, ShimError},
    reasoning::WireFormat,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const ANTHROPIC_BREAKPOINT_LIMIT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stability {
    Static,
    Session,
    Turn,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheSegment {
    pub upto_message: usize,
    #[serde(default)]
    pub label: String,
    pub stability: Stability,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CachePolicy {
    #[serde(default)]
    pub segments: Vec<CacheSegment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

fn invalid(message: &str) -> ShimError {
    ShimError::ProviderError {
        status: 400,
        body: format!("invalid x-cache: {message}"),
    }
}
fn policy(request: &Value) -> Result<Option<CachePolicy>> {
    request
        .get("x-cache")
        .map(|v| {
            serde_json::from_value(v.clone()).map_err(|_| {
                invalid("expected segments with message indices and static/session/turn stability")
            })
        })
        .transpose()
}

/// Marker locations only. Schema properties and literal user JSON named
/// cache_control are not recursively interpreted as placement instructions.
fn markers(value: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    let mut add = |path: String, node: &Value| {
        if node.get("cache_control").is_some() {
            paths.push(path);
        }
    };
    for (i, tool) in value["tools"].as_array().into_iter().flatten().enumerate() {
        add(format!("/tools/{i}"), tool);
    }
    for (i, block) in value["system"].as_array().into_iter().flatten().enumerate() {
        add(format!("/system/{i}"), block);
    }
    for (i, message) in value["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        add(format!("/messages/{i}"), message);
        for (j, block) in message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            add(format!("/messages/{i}/content/{j}"), block);
        }
        for (j, call) in message["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            add(format!("/messages/{i}/tool_calls/{j}"), call);
        }
    }
    add(String::new(), value);
    paths
}

fn trim_markers(value: &mut Value) {
    let paths = markers(value);
    let excess = paths.len().saturating_sub(ANTHROPIC_BREAKPOINT_LIMIT);
    for path in paths.into_iter().take(excess) {
        if let Some(obj) = value.pointer_mut(&path).and_then(Value::as_object_mut) {
            obj.remove("cache_control");
        }
    }
}

/// Annotate the original message indices before system extraction or native
/// tool-message conversion. Without x-cache this is an exact clone/no-op.
pub fn prepare_request(request: &Value, wire: WireFormat) -> Result<Value> {
    let mut request = request.clone();
    let Some(policy) = policy(&request)? else {
        return Ok(request);
    };
    if wire != WireFormat::AnthropicMessages {
        return Ok(request);
    }
    let count = request["messages"].as_array().map(Vec::len).unwrap_or(0);
    if policy.segments.iter().any(|s| s.upto_message >= count) {
        return Err(invalid("segment message index is out of range"));
    }
    // Explicit stability boundaries supersede automatic end-of-request caching.
    if !policy.segments.is_empty() {
        request.as_object_mut().unwrap().remove("cache_control");
    }
    trim_markers(&mut request);
    let mut used = markers(&request).len();
    let mut boundaries = BTreeMap::new();
    for segment in policy.segments {
        boundaries.insert(segment.upto_message, segment.stability);
    }
    for (index, stability) in boundaries.into_iter().rev() {
        if stability == Stability::Turn {
            continue;
        }
        let message = &mut request["messages"][index];
        let existing = anchor_has_marker(message);
        if !existing && used >= ANTHROPIC_BREAKPOINT_LIMIT {
            continue;
        }
        let Some(anchor) = anchor(message) else {
            continue;
        };
        anchor["cache_control"] =
            json!({"type":"ephemeral","ttl":if stability==Stability::Static {"1h"}else{"5m"}});
        if !existing {
            used += 1;
        }
    }
    Ok(request)
}

fn anchor_has_marker(message: &Value) -> bool {
    if message["role"] == "tool" {
        return message.get("cache_control").is_some();
    }
    if message["role"] == "assistant" {
        if let Some(call) = message["tool_calls"].as_array().and_then(|a| a.last()) {
            return call.get("cache_control").is_some();
        }
    }
    message["content"]
        .as_array()
        .and_then(|a| a.last())
        .is_some_and(|b| b.get("cache_control").is_some())
}

fn anchor(message: &mut Value) -> Option<&mut Value> {
    if message["role"] == "tool" {
        return Some(message);
    }
    if message["role"] == "assistant"
        && message["tool_calls"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    {
        return message["tool_calls"].as_array_mut()?.last_mut();
    }
    if let Some(text) = message["content"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    {
        message["content"] = json!([{"type":"text","text":text}]);
    }
    let block = message.get_mut("content")?.as_array_mut()?.last_mut()?;
    if !block.is_object()
        || matches!(
            block["type"].as_str(),
            Some("thinking" | "redacted_thinking")
        )
    {
        return None;
    }
    Some(block)
}

/// Final native-body pass, after provider overrides. Never forward x-cache.
pub fn finish_request(original: &Value, body: &mut Value, wire: WireFormat) -> Result<()> {
    let policy = policy(original)?;
    if let Some(obj) = body.as_object_mut() {
        obj.remove("x-cache");
    }
    if let Some(policy) = policy {
        if wire == WireFormat::OpenAiResponses {
            if let Some(key) = policy.key {
                body["prompt_cache_key"] = json!(key);
            }
        }
        if wire == WireFormat::AnthropicMessages {
            if !policy.segments.is_empty() {
                body.as_object_mut().unwrap().remove("cache_control");
            }
            trim_markers(body);
            let mut short = false;
            for path in markers(body) {
                let long = body
                    .pointer(&path)
                    .and_then(|v| v.pointer("/cache_control/ttl"))
                    .and_then(Value::as_str)
                    == Some("1h");
                if long && short {
                    return Err(invalid("1h breakpoints must precede 5m breakpoints"));
                }
                if !long {
                    short = true;
                }
            }
        }
    }
    Ok(())
}

pub fn uses_extended_ttl(body: &Value) -> bool {
    markers(body).iter().any(|p| {
        body.pointer(p)
            .and_then(|v| v.pointer("/cache_control/ttl"))
            .and_then(Value::as_str)
            == Some("1h")
    })
}

/// Settings identity for a stateless continuation. History is supplied in full
/// each time; include/store/reasoning and all other settings remain in the hash.
/// This does not cache or store any conversation on the provider.
pub fn continuation_identity(native_request: &Value) -> String {
    let mut settings = native_request.clone();
    if let Some(obj) = settings.as_object_mut() {
        obj.remove("input");
        obj.remove("messages");
        obj.remove("contents");
        obj.remove("stream");
    }
    format!("{:x}", Sha256::digest(settings.to_string().as_bytes()))
}

/// Compare settings and the complete prior message prefix. This does not
/// enable provider-side storage or replace the next request with a delta.
pub fn continuation_matches(previous: &Value, next: &Value) -> bool {
    if continuation_identity(previous) != continuation_identity(next) {
        return false;
    }
    for key in ["input", "messages", "contents"] {
        match (previous.get(key), next.get(key)) {
            (None, None) => {}
            (Some(Value::Array(old)), Some(Value::Array(new))) if new.starts_with(old) => {}
            (Some(old), Some(new)) if old == new => {}
            _ => return false,
        }
    }
    true
}

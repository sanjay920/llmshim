//! The canonical response, rendered as Gemini's `GenerateContentResponse`, and
//! Gemini's error object.
//!
//! Reasoning crosses this wire as thought parts: Gemini signs them with
//! `thoughtSignature`, so a block that originated here is echoed with its own
//! signature and any other block is echoed as an opaque handle whose original
//! typed block is receipted. That is the same rule the Messages wire applies to
//! thinking blocks — an opaque value must never be presented as if it came from
//! this wire.

use super::super::{call_content, Receipts, Result};
use axum::http::StatusCode;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(crate) fn response(
    canonical: &Value,
    usage: &Value,
    finish: &str,
    receipts: &Receipts,
    scope: &str,
) -> Result<Value> {
    let message = &canonical["message"];
    let mut parts = Vec::new();
    for block in message["reasoning"].as_array().into_iter().flatten() {
        // Only text blocks have a thought part to travel in; Gemini carries no
        // container for redacted or encrypted reasoning.
        let Some(text) = block["text"].as_str() else {
            continue;
        };
        let mut part = json!({"thought": true, "text": text});
        part["thoughtSignature"] = json!(thought_signature(block));
        receipts.put(scope, "block", &part, block)?;
        parts.push(part);
    }
    if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
        parts.push(json!({"text": text}));
    } else if let Some(blocks) = message["content"].as_array() {
        for block in blocks {
            if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) {
                parts.push(json!({"text": text}));
            }
        }
    }
    if let Some(refusal) = message["refusal"].as_str() {
        parts.push(json!({"text": refusal}));
    }
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        let parsed = call_content(call)?;
        let mut part = json!({"functionCall": {
            "name": parsed["name"],
            "args": parsed["arguments"],
            "id": parsed["id"],
        }});
        // A signature is echoed only when this wire issued it; a foreign one is
        // not ours to present, and replay finds the typed block by call id
        // through the receipt the shared reader already wrote.
        if call
            .pointer("/thought_signature/origin/wire")
            .and_then(Value::as_str)
            == Some("google-generate-content")
        {
            if let Some(signature) = call
                .pointer("/thought_signature/data")
                .and_then(Value::as_str)
            {
                part["thoughtSignature"] = json!(signature);
            }
        }
        parts.push(part);
    }
    Ok(json!({
        "candidates": [{
            "content": {"role": "model", "parts": parts},
            "finishReason": finish_reason(finish),
            "index": 0,
        }],
        "usageMetadata": usage_metadata(usage),
        "modelVersion": canonical["model"],
        "responseId": canonical["id"],
    }))
}

/// Gemini's own signature when the block came from this wire, and otherwise a
/// handle the caller echoes back verbatim for [`response`]'s receipt to restore.
/// The handle is derived from the block so an unchanged conversation keeps
/// identical bytes and its prefix cache.
fn thought_signature(block: &Value) -> String {
    if block["origin"]["wire"] == "google-generate-content" {
        if let Some(signature) = block["signature"].as_str().filter(|s| !s.is_empty()) {
            return signature.to_owned();
        }
    }
    format!("lsr_{:x}", Sha256::digest(block.to_string().as_bytes()))
}

fn usage_metadata(usage: &Value) -> Value {
    json!({
        "promptTokenCount": usage["input_tokens"],
        "candidatesTokenCount": usage["output_tokens"],
        "totalTokenCount": usage["total_tokens"],
        "cachedContentTokenCount": usage["cache_read_tokens"],
        "cost_usd": usage["cost_usd"],
        "cost_source": usage["cost_source"],
    })
}

fn finish_reason(finish: &str) -> &'static str {
    match finish {
        "length" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        _ => "STOP",
    }
}

pub(crate) fn error(status: StatusCode, message: &str) -> Value {
    json!({"error": {
        "code": status.as_u16(),
        "message": message,
        "status": google_status(status),
    }})
}

fn google_status(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        409 => "ABORTED",
        429 => "RESOURCE_EXHAUSTED",
        499 => "CANCELLED",
        500 => "INTERNAL",
        501 => "UNIMPLEMENTED",
        503 => "UNAVAILABLE",
        504 => "DEADLINE_EXCEEDED",
        _ => "UNKNOWN",
    }
}

/// One chunk per part, then the terminal chunk carrying the finish reason and
/// usage. Gemini's SSE has no `[DONE]` marker: the finish reason ends the stream.
pub(crate) fn frames(response: &Value) -> Vec<(Option<String>, String)> {
    let mut frames = Vec::new();
    let mut opening = json!({"candidates":[{"content":{"role":"model","parts":[]},"index":0}]});
    if let Some(version) = response.get("modelVersion") {
        opening["modelVersion"] = version.clone();
    }
    frames.push((None, opening.to_string()));
    for part in response
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        frames.push((
            None,
            json!({"candidates":[{"content":{"role":"model","parts":[part]},"index":0}]})
                .to_string(),
        ));
    }
    let mut terminal = json!({"candidates":[{
        "content": {"role": "model", "parts": []},
        "finishReason": response
            .pointer("/candidates/0/finishReason")
            .cloned()
            .unwrap_or_else(|| json!("STOP")),
        "index": 0,
    }]});
    for key in [
        "usageMetadata",
        "responseId",
        "modelVersion",
        "x-llmshim-served-model",
    ] {
        if let Some(value) = response.get(key) {
            terminal[key] = value.clone();
        }
    }
    frames.push((None, terminal.to_string()));
    frames
}

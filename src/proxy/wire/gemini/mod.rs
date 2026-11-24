//! Google Generative Language (`generateContent`) as an inbound native wire.
//!
//! Gemini carries the model in the URL and the turns in `contents[].parts`, so
//! [`request`] rewrites that shape into the canonical messages every adapter
//! already consumes. By the time it runs, the shared middleware has read the
//! model off the path and put it in the body, so this reader takes `model` from
//! the body exactly like the Chat and Messages readers do.

mod config;
mod response;

use super::{array, import_call, Receipts, Result};
use crate::proxy::wire::receipts::RestorationBudget;
use config::canonical_config;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};

pub(crate) use response::{error, frames, response};

/// `/v1beta/models/<routing id>:<action>` → the routing id and whether the
/// action streams. A routing id keeps its slashes (`gemini/gemini-3.8-flash`),
/// so it is parsed out of the path rather than matched as one segment.
pub(super) fn path_route(path: &str) -> Option<(&str, bool)> {
    let rest = path.strip_prefix("/v1beta/models/")?;
    let (model, action) = rest.rsplit_once(':')?;
    if model.is_empty() || model.contains(':') {
        return None;
    }
    match action {
        "generateContent" => Some((model, false)),
        "streamGenerateContent" => Some((model, true)),
        _ => None,
    }
}

pub(super) fn request(
    native: &Value,
    receipts: &Receipts,
    scope: &str,
    restoration: &mut RestorationBudget,
) -> Result<Value> {
    let model = native["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .ok_or("model is required")?
        .to_owned();
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = native.get("systemInstruction") {
        let text = instruction_text(system)?;
        if !text.is_empty() {
            messages.push(json!({"role":"system","content":text}));
        }
    }
    // Google pairs a function response with a call by id, and with the oldest
    // unanswered call of the same name when the response carries no id.
    let mut unanswered: BTreeMap<String, VecDeque<String>> = BTreeMap::new();
    for (turn_index, turn) in array(&native["contents"], "contents")?.iter().enumerate() {
        let role = match turn["role"].as_str() {
            None | Some("user") => "user",
            Some("model") => "assistant",
            Some(other) => return Err(format!("unsupported Gemini role: {other}")),
        };
        let mut blocks: Vec<Value> = Vec::new();
        let mut calls: Vec<Value> = Vec::new();
        let mut reasoning: Vec<Value> = Vec::new();
        let mut results: Vec<Value> = Vec::new();
        for (part_index, part) in array(&turn["parts"], "parts")?.iter().enumerate() {
            if part["thought"] == true {
                if let Some(original) = receipts.get_bounded(scope, "block", part, restoration)? {
                    reasoning.push(original);
                }
                continue;
            }
            if let Some(text) = part["text"].as_str() {
                if !text.is_empty() {
                    blocks.push(json!({"type":"text","text":text}));
                }
                continue;
            }
            if let Some(content) = part_content(part) {
                blocks.push(content);
                continue;
            }
            if let Some(call) = part.get("functionCall") {
                let id = match call["id"].as_str().filter(|id| !id.is_empty()) {
                    Some(id) => id.to_owned(),
                    // Google calls this field optional; the canonical history
                    // requires one. Deriving it from the call's place in the
                    // request keeps a replayed conversation stable.
                    None => format!(
                        "call_gc_{:x}",
                        Sha256::digest(
                            format!("{model}/{turn_index}/{part_index}/{}", call["name"])
                                .as_bytes()
                        )
                    ),
                };
                unanswered
                    .entry(call["name"].as_str().unwrap_or_default().to_owned())
                    .or_default()
                    .push_back(id.clone());
                let arguments = call
                    .get("args")
                    .filter(|args| !args.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let call = json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": call["name"], "arguments": arguments.to_string()},
                });
                calls.push(import_call(&call, receipts, scope, restoration)?);
                continue;
            }
            if let Some(result) = part.get("functionResponse") {
                let name = result["name"].as_str().unwrap_or_default();
                let id = match result["id"].as_str().filter(|id| !id.is_empty()) {
                    Some(id) => id.to_owned(),
                    None => unanswered
                        .get_mut(name)
                        .and_then(VecDeque::pop_front)
                        .ok_or("Gemini function response has no matching function call")?,
                };
                let content = result.get("response").cloned().unwrap_or_else(|| json!({}));
                results
                    .push(json!({"role":"tool","tool_call_id":id,"content":content.to_string()}));
                continue;
            }
            return Err("unsupported Gemini content part".into());
        }
        // Results answer the previous model turn, so they precede this turn's
        // own text: the canonical history requires results beside their call.
        messages.append(&mut results);
        if !blocks.is_empty() || !calls.is_empty() || !reasoning.is_empty() {
            let mut message = json!({"role":role,"content":blocks});
            if !calls.is_empty() {
                message["tool_calls"] = Value::Array(calls);
            }
            if !reasoning.is_empty() {
                message["reasoning"] = Value::Array(reasoning);
            }
            messages.push(message);
        }
    }
    Ok(json!({
        "model": model,
        "stream": native["stream"].as_bool().unwrap_or(false),
        "fallback": native["fallback"],
        "messages": messages,
        "provider_config": canonical_config(native)?,
    }))
}

/// `systemInstruction` holds parts, and only text describes the model's
/// instructions; a part of any other kind has no canonical system equivalent.
fn instruction_text(system: &Value) -> Result<String> {
    let mut text = Vec::new();
    for part in array(&system["parts"], "systemInstruction parts")? {
        match part["text"].as_str() {
            Some(part) => text.push(part.to_owned()),
            None => return Err("systemInstruction supports text parts only".into()),
        }
    }
    Ok(text.join("\n\n"))
}

/// `inlineData` carries bytes as base64; the canonical image block is the same
/// bytes as a data URI. `fileData` names a file Google holds, which no canonical
/// image block can express, so it becomes the same text note `to_gemini` uses
/// for a remote image URL.
fn part_content(part: &Value) -> Option<Value> {
    if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data")) {
        let mime = inline["mimeType"]
            .as_str()
            .or_else(|| inline["mime_type"].as_str())?;
        let data = inline["data"].as_str()?;
        return Some(json!({"type":"image_url","image_url":{
            "url": format!("data:{mime};base64,{data}")
        }}));
    }
    let uri = part
        .pointer("/fileData/fileUri")
        .or_else(|| part.pointer("/file_data/file_uri"))?
        .as_str()?;
    Some(json!({"type":"text","text":format!("[Image: {uri}]")}))
}

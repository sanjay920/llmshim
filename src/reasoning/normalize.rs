use super::*;

fn structured_block(
    payload: &Value,
    origin: &ReasoningOrigin,
    field: Option<&str>,
) -> Option<ReasoningBlock> {
    let mut b = ReasoningBlock::text("", origin.clone());
    let kind = payload["type"].as_str().unwrap_or("");
    match kind {
        "reasoning" => {
            b.item_id = payload["id"].as_str().map(str::to_owned);
            if let Some(data) = payload["encrypted_content"].as_str() {
                b.kind = ReasoningKind::Encrypted;
                b.text = None;
                b.data = Some(data.into());
            } else {
                b.text = Some(summary_text(payload));
            }
        }
        "redacted_thinking" => {
            b.kind = ReasoningKind::Redacted;
            b.text = None;
            b.data = Some(payload["data"].as_str()?.into());
        }
        "reasoning.encrypted" => {
            b.kind = ReasoningKind::Encrypted;
            b.text = None;
            b.data = Some(payload["data"].as_str()?.into());
        }
        "thinking" => {
            b.text = Some(payload["thinking"].as_str().unwrap_or("").into());
            b.signature = payload["signature"].as_str().map(str::to_owned);
        }
        "reasoning.text" => {
            b.text = Some(payload["text"].as_str().unwrap_or("").into());
            b.signature = payload["signature"].as_str().map(str::to_owned);
        }
        "reasoning.summary" => {
            b.text = Some(payload["summary"].as_str().unwrap_or("").into());
        }
        _ if payload["thought"] == true => {
            b.text = Some(payload["text"].as_str().unwrap_or("").into());
            b.signature = payload["thoughtSignature"].as_str().map(str::to_owned);
        }
        _ => return None,
    }
    b.payload = Some(payload.clone());
    b.source_field = field.map(str::to_owned);
    Some(b)
}

/// The readable text of a Responses `reasoning` item. OpenAI's hosted models
/// put it in `summary[]`; a server returning the model's own reasoning puts it
/// in `content[]` as `reasoning_text` and leaves the summary empty. The summary
/// is preferred when both exist; the content is the fallback, or the item's
/// completed snapshot would replace every streamed delta with nothing.
fn summary_text(payload: &Value) -> String {
    let joined = |field: &str, kind: Option<&str>| {
        payload[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| kind.is_none_or(|kind| p["type"] == kind))
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    // Every summary part is read, as before; only the content fallback is
    // typed, because a content part can be something other than reasoning.
    let summary = joined("summary", None);
    if summary.is_empty() {
        joined("content", Some("reasoning_text"))
    } else {
        summary
    }
}

pub(super) fn chat_blocks(message: &Value, origin: &ReasoningOrigin) -> Vec<ReasoningBlock> {
    for field in ["reasoning_details", "thinking_blocks"] {
        let blocks: Vec<_> = message[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| structured_block(p, origin, Some(field)))
            .collect();
        if !blocks.is_empty() {
            return blocks;
        }
    }
    let mut blocks = Vec::new();
    let text = message["reasoning_content"]
        .as_str()
        .or_else(|| message["reasoning"].as_str());
    if let Some(text) = text {
        let mut b = ReasoningBlock::text(text, origin.clone());
        b.signature = message["reasoning_signature"].as_str().map(str::to_owned);
        b.source_field = Some(
            if message["reasoning_content"].is_string() {
                "reasoning_content"
            } else {
                "reasoning"
            }
            .into(),
        );
        blocks.push(b);
    }
    if let Some(data) = message["redacted_reasoning_content"].as_str() {
        let mut b = ReasoningBlock::text("", origin.clone());
        b.kind = ReasoningKind::Redacted;
        b.text = None;
        b.data = Some(data.into());
        blocks.push(b);
    }
    blocks
}

fn stamp_tool_signatures(message: &mut Value, origin: &ReasoningOrigin) {
    if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
        for call in calls {
            if let Some(data) = call["thought_signature"].as_str().map(str::to_owned) {
                call["thought_signature"] = json!(ThoughtSignature {
                    data,
                    origin: origin.clone()
                });
            }
        }
    }
}

/// Normalize from the original native response, before lossy text projections
/// can discard ordered blocks, ids, signatures, or encrypted item contents.
pub fn capture_response(target: &ReplayTarget, native: &Value, response: &mut Value) {
    let origin = target.origin();
    if let Some(choices) = response.get_mut("choices").and_then(Value::as_array_mut) {
        for (index, choice) in choices.iter_mut().enumerate() {
            let Some(message) = choice.get_mut("message") else {
                continue;
            };
            let blocks: Vec<ReasoningBlock> = match target.wire {
                WireFormat::AnthropicMessages => native["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| structured_block(p, &origin, None))
                    .collect(),
                WireFormat::OpenAiResponses => native["output"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|p| p["type"] == "reasoning")
                    .filter_map(|p| structured_block(p, &origin, None))
                    .collect(),
                WireFormat::OpenAiChat => {
                    chat_blocks(&native["choices"][index]["message"], &origin)
                }
                WireFormat::GoogleGenerateContent => native["candidates"][index]["content"]
                    ["parts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| structured_block(p, &origin, None))
                    .collect(),
            };
            strip_fields(message);
            if !blocks.is_empty() {
                message["reasoning"] = json!(blocks);
            }
            stamp_tool_signatures(message, &origin);
        }
    }
    crate::providers::anthropic_signature::observe(response, &target.model);
}

fn delta(mut block: ReasoningBlock, index: Value, replace: bool) -> Value {
    // The index is stream framing, never part of the stored ReasoningBlock.
    if block.origin.wire == WireFormat::OpenAiResponses && block.item_id.is_none() {
        block.item_id = index.as_str().map(str::to_owned);
    }
    let mut value = json!(block);
    value["index"] = index;
    if replace {
        value["replace"] = json!(true);
    }
    value
}

/// Transport parsers hand their raw event and ordinary content/tool chunk to
/// this function. Reasoning delta framing and provenance are defined once.
pub fn capture_stream(
    target: &ReplayTarget,
    native: &Value,
    normalized: Option<String>,
) -> crate::error::Result<Option<String>> {
    let origin = target.origin();
    let mut chunk: Value = match normalized {
        Some(data) => serde_json::from_str(&data)?,
        None => json!({
            "object":"chat.completion.chunk","model":if target.provider=="chatgpt" {format!("chatgpt/{}",target.model)}else{target.model.clone()},
            "choices":[{"index":0,"delta":{},"finish_reason":null}]
        }),
    };
    let mut reasoning = Vec::new();
    // Callable records are emitted exclusively by the per-stream tool assembler.
    // Even Chat Completions passthrough must not expose raw provider ids here.
    if let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices {
            if let Some(delta) = choice.get_mut("delta").and_then(Value::as_object_mut) {
                delta.remove("tool_calls");
            }
        }
    }
    let index = native
        .get("index")
        .or_else(|| native.get("output_index"))
        .cloned()
        .unwrap_or(json!(0));
    match target.wire {
        WireFormat::AnthropicMessages => match native["type"].as_str() {
            Some("content_block_start") => {
                if let Some(b) = structured_block(&native["content_block"], &origin, None) {
                    reasoning.push(delta(b, index, true));
                }
            }
            Some("content_block_delta") => {
                let d = &native["delta"];
                let mut b = ReasoningBlock::text("", origin.clone());
                match d["type"].as_str() {
                    Some("thinking_delta") => {
                        b.text = Some(d["thinking"].as_str().unwrap_or("").into());
                        reasoning.push(delta(b, index, false));
                    }
                    Some("signature_delta") => {
                        b.text = None;
                        b.signature = Some(d["signature"].as_str().unwrap_or("").into());
                        reasoning.push(delta(b, index, false));
                    }
                    _ => {}
                }
            }
            _ => {}
        },
        WireFormat::OpenAiResponses => match native["type"].as_str() {
            Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => {
                let mut b =
                    ReasoningBlock::text(native["delta"].as_str().unwrap_or(""), origin.clone());
                b.item_id = native["item_id"].as_str().map(str::to_owned);
                reasoning.push(delta(b, index, false));
            }
            Some("response.output_item.done") if native["item"]["type"] == "reasoning" => {
                if let Some(b) = structured_block(&native["item"], &origin, None) {
                    reasoning.push(delta(b, index, true));
                }
            }
            Some("response.completed" | "response.incomplete") => {
                for (index, item) in native["response"]["output"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    if item["type"] == "reasoning" {
                        if let Some(b) = structured_block(item, &origin, None) {
                            reasoning.push(delta(b, json!(index), true));
                        }
                    }
                }
            }
            _ => {}
        },
        WireFormat::GoogleGenerateContent => {
            for (i, part) in native["candidates"][0]["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if let Some(mut b) = structured_block(part, &origin, None) {
                    // Text streams append; the completed payload is reconstructed
                    // from fragments rather than replaying only the last fragment.
                    b.payload = None;
                    reasoning.push(delta(b, json!(i), false));
                }
            }
        }
        WireFormat::OpenAiChat => {
            if let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut) {
                for (i, choice) in choices.iter_mut().enumerate() {
                    let raw = &native["choices"][i]["delta"];
                    let Some(d) = choice.get_mut("delta") else {
                        continue;
                    };
                    let values: Vec<_> = chat_blocks(raw, &origin)
                        .into_iter()
                        .enumerate()
                        .map(|(i, b)| {
                            let index = b
                                .payload
                                .as_ref()
                                .and_then(|p| p.get("index"))
                                .cloned()
                                .unwrap_or(json!(i));
                            delta(b, index, false)
                        })
                        .collect();
                    strip_fields(d);
                    if !values.is_empty() {
                        d["reasoning"] = json!(values);
                    }
                    stamp_tool_signatures(d, &origin);
                }
            }
            let useful = chunk.get("usage").is_some()
                || chunk["choices"].as_array().is_some_and(|choices| {
                    choices.iter().any(|c| {
                        c["finish_reason"].is_string()
                            || c["delta"].as_object().is_some_and(|d| !d.is_empty())
                    })
                });
            return Ok(useful.then(|| chunk.to_string()));
        }
    }
    // A native event skipped by the ordinary parser can still carry an opaque
    // reasoning block. Conversely, don't invent chunks for keepalives.
    if let Some(d) = chunk.pointer_mut("/choices/0/delta") {
        strip_fields(d);
        if !reasoning.is_empty() {
            d["reasoning"] = json!(reasoning);
        }
        stamp_tool_signatures(d, &origin);
    }
    let useful = chunk
        .pointer("/choices/0/delta")
        .and_then(Value::as_object)
        .is_some_and(|d| !d.is_empty())
        || chunk
            .pointer("/choices/0/finish_reason")
            .is_some_and(Value::is_string)
        || chunk.get("usage").is_some();
    Ok(useful.then(|| chunk.to_string()))
}

/// Visible reasoning text for display. Never includes redacted/encrypted data.
pub fn reasoning_text(message: &Value) -> String {
    message["reasoning"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| {
            if b["replace"] == true {
                return None;
            } // authoritative snapshots are not new display text
            b["text"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| b.get("payload").map(summary_text))
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Assemble normalized reasoning fragments once. Repeated completed snapshots
/// replace their part, and signatures/data are never concatenated across parts.
#[derive(Default, Debug)]
pub struct ReasoningAccumulator {
    parts: BTreeMap<String, Value>,
    order: Vec<String>,
}
impl ReasoningAccumulator {
    pub fn push(&mut self, message: &Value) {
        for (position, fragment) in message["reasoning"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            // A fragment carrying neither key is a whole block already: a buffered
            // answer's blocks lose their stream index when they are assembled. Its
            // position in this message keys it, or every unkeyed block would merge
            // into one and an encrypted block could swallow a readable one.
            let key = match (fragment["item_id"].as_str(), fragment.get("index")) {
                (Some(id), _) => format!("item:{id}"),
                (None, Some(index)) => format!("index:{index}"),
                (None, None) => format!("index:{position}"),
            };
            if !self.parts.contains_key(&key) {
                self.order.push(key.clone());
            }
            if fragment["replace"] == true || !self.parts.contains_key(&key) {
                let mut snapshot = fragment.clone();
                if let Some(previous) = self.parts.get(&key) {
                    snapshot["origin"] = previous["origin"].clone();
                }
                self.parts.insert(key, snapshot);
            } else if let Some(block) = self.parts.get_mut(&key) {
                for field in ["text", "signature", "data"] {
                    if let Some(text) = fragment[field].as_str() {
                        crate::streaming::append_string_fragment(&mut block[field], text);
                    }
                }
                if let Some(payload) = fragment.get("payload") {
                    if let (Some(previous), Some(incoming)) = (
                        block.get_mut("payload").and_then(Value::as_object_mut),
                        payload.as_object(),
                    ) {
                        for (field, value) in incoming {
                            if matches!(
                                field.as_str(),
                                "text" | "signature" | "data" | "summary" | "thinking"
                            ) && value.is_string()
                            {
                                crate::streaming::append_string_fragment(
                                    previous.entry(field.clone()).or_insert(Value::Null),
                                    value.as_str().unwrap(),
                                );
                            } else {
                                previous.insert(field.clone(), value.clone());
                            }
                        }
                    } else {
                        block["payload"] = payload.clone();
                    }
                }
            }
        }
    }
    pub fn blocks(&self) -> Vec<Value> {
        self.order
            .iter()
            .filter_map(|key| self.parts.get(key))
            .cloned()
            .map(|mut b| {
                if let Some(obj) = b.as_object_mut() {
                    obj.remove("index");
                    obj.remove("replace");
                }
                // A streaming Anthropic start payload is partial; serialize assembled
                // fields when reconstructing the native block.
                if b.pointer("/origin/wire").and_then(Value::as_str) == Some("anthropic-messages") {
                    b.as_object_mut().unwrap().remove("payload");
                }
                b
            })
            .collect()
    }
}

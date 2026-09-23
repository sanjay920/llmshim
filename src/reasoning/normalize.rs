use super::*;
use crate::derived_response::{DerivedFootprint, DerivedResponseBudget};
use std::mem::size_of;

fn checked_string_bytes(strings: impl IntoIterator<Item = usize>) -> Option<usize> {
    strings
        .into_iter()
        .try_fold(0_usize, |total, length| total.checked_add(length))
}

fn recognized_structured_block(payload: &Value) -> bool {
    matches!(
        payload["type"].as_str(),
        Some(
            "reasoning"
                | "redacted_thinking"
                | "reasoning.encrypted"
                | "thinking"
                | "reasoning.text"
                | "reasoning.summary"
        )
    ) || payload["thought"] == true
}

pub(super) fn legacy_structured_block_emits(payload: &Value) -> bool {
    if !recognized_structured_block(payload) {
        return false;
    }
    !matches!(
        payload["type"].as_str(),
        Some("redacted_thinking" | "reasoning.encrypted")
    ) || payload["data"].is_string()
}

fn summary_text_length(payload: &Value) -> Option<usize> {
    let joined_length = |field: &str, kind: Option<&str>| {
        let mut total = 0_usize;
        let mut count = 0_usize;
        for text in payload[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|part| kind.is_none_or(|kind| part["type"] == kind))
            .filter_map(|part| part["text"].as_str())
        {
            total = total.checked_add(text.len())?;
            count = count.checked_add(1)?;
        }
        total.checked_add(count.saturating_sub(1))
    };
    let summary = joined_length("summary", None)?;
    if summary == 0 {
        joined_length("content", Some("reasoning_text"))
    } else {
        Some(summary)
    }
}

fn structured_block_footprint(
    payload: &Value,
    target: &ReplayTarget,
    field: Option<&str>,
    include_payload: bool,
) -> Option<DerivedFootprint> {
    if !recognized_structured_block(payload) {
        return None;
    }
    let kind = payload["type"].as_str().unwrap_or("");
    let copied_string_bytes = match kind {
        "reasoning" => payload["encrypted_content"]
            .as_str()
            .map(str::len)
            .or_else(|| summary_text_length(payload)),
        "redacted_thinking" | "reasoning.encrypted" => Some(payload["data"].as_str()?.len()),
        "thinking" => checked_string_bytes([
            payload["thinking"].as_str().unwrap_or("").len(),
            payload["signature"].as_str().map(str::len).unwrap_or(0),
        ]),
        "reasoning.text" => checked_string_bytes([
            payload["text"].as_str().unwrap_or("").len(),
            payload["signature"].as_str().map(str::len).unwrap_or(0),
        ]),
        "reasoning.summary" => Some(payload["summary"].as_str().unwrap_or("").len()),
        _ if payload["thought"] == true => checked_string_bytes([
            payload["text"].as_str().unwrap_or("").len(),
            payload["thoughtSignature"]
                .as_str()
                .map(str::len)
                .unwrap_or(0),
        ]),
        _ => return None,
    }?
    .checked_add(payload["id"].as_str().map(str::len).unwrap_or(0))?
    .checked_add(field.map(str::len).unwrap_or(0))?;
    let mut footprint = DerivedFootprint::record(size_of::<ReasoningBlock>())?
        .checked_add(crate::derived_response::origin_footprint(target)?)?
        .checked_add(DerivedFootprint::strings(copied_string_bytes))?;
    if include_payload {
        footprint =
            footprint.checked_add(crate::derived_response::value_footprint(payload).ok()?)?;
    }
    footprint.checked_multiply(2)
}

fn ensure_origin(
    origin: &mut Option<ReasoningOrigin>,
    target: &ReplayTarget,
    budget: &mut DerivedResponseBudget,
) -> crate::error::Result<()> {
    if origin.is_none() {
        let footprint =
            crate::derived_response::origin_footprint(target).ok_or_else(|| budget.error())?;
        budget.reserve(footprint)?;
        *origin = Some(target.origin());
    }
    Ok(())
}

fn reserve_plain_block(
    target: &ReplayTarget,
    budget: &mut DerivedResponseBudget,
    copied_string_bytes: usize,
) -> crate::error::Result<()> {
    let footprint = DerivedFootprint::record(size_of::<ReasoningBlock>())
        .ok_or_else(|| budget.error())?
        .checked_add(
            crate::derived_response::origin_footprint(target).ok_or_else(|| budget.error())?,
        )
        .and_then(|value| value.checked_add(DerivedFootprint::strings(copied_string_bytes)))
        .and_then(|value| value.checked_multiply(2))
        .ok_or_else(|| budget.error())?;
    budget.reserve(footprint)
}

fn structured_block_with_budget(
    payload: &Value,
    target: &ReplayTarget,
    origin: &mut Option<ReasoningOrigin>,
    field: Option<&str>,
    include_payload: bool,
    budget: &mut DerivedResponseBudget,
) -> crate::error::Result<Option<ReasoningBlock>> {
    if !recognized_structured_block(payload) {
        return Ok(None);
    }
    if matches!(
        payload["type"].as_str(),
        Some("redacted_thinking" | "reasoning.encrypted")
    ) && payload["data"].as_str().is_none()
    {
        return Ok(None);
    }
    let footprint = structured_block_footprint(payload, target, field, include_payload)
        .ok_or_else(|| budget.error())?;
    budget.reserve(footprint)?;
    ensure_origin(origin, target, budget)?;
    let mut b = ReasoningBlock::text("", origin.as_ref().unwrap().clone());
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
            let Some(data) = payload["data"].as_str() else {
                return Ok(None);
            };
            b.data = Some(data.into());
        }
        "reasoning.encrypted" => {
            b.kind = ReasoningKind::Encrypted;
            b.text = None;
            let Some(data) = payload["data"].as_str() else {
                return Ok(None);
            };
            b.data = Some(data.into());
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
        _ => return Ok(None),
    }
    if include_payload {
        b.payload = Some(payload.clone());
    }
    b.source_field = field.map(str::to_owned);
    Ok(Some(b))
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

pub(super) fn chat_blocks(
    message: &Value,
    target: &ReplayTarget,
    origin: &mut Option<ReasoningOrigin>,
    budget: &mut DerivedResponseBudget,
) -> crate::error::Result<Vec<ReasoningBlock>> {
    for field in ["reasoning_details", "thinking_blocks"] {
        let mut blocks = Vec::new();
        for payload in message[field].as_array().into_iter().flatten() {
            if let Some(block) =
                structured_block_with_budget(payload, target, origin, Some(field), true, budget)?
            {
                blocks.push(block);
            }
        }
        if !blocks.is_empty() {
            return Ok(blocks);
        }
    }
    let mut blocks = Vec::new();
    let text = message["reasoning_content"]
        .as_str()
        .or_else(|| message["reasoning"].as_str());
    if let Some(text) = text {
        let copied = checked_string_bytes([
            text.len(),
            message["reasoning_signature"]
                .as_str()
                .map(str::len)
                .unwrap_or(0),
            if message["reasoning_content"].is_string() {
                "reasoning_content".len()
            } else {
                "reasoning".len()
            },
        ])
        .ok_or_else(|| budget.error())?;
        let footprint = DerivedFootprint::record(size_of::<ReasoningBlock>())
            .ok_or_else(|| budget.error())?
            .checked_add(
                crate::derived_response::origin_footprint(target).ok_or_else(|| budget.error())?,
            )
            .and_then(|value| value.checked_add(DerivedFootprint::strings(copied)))
            .and_then(|value| value.checked_multiply(2))
            .ok_or_else(|| budget.error())?;
        budget.reserve(footprint)?;
        ensure_origin(origin, target, budget)?;
        let mut b = ReasoningBlock::text(text, origin.as_ref().unwrap().clone());
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
        let footprint = DerivedFootprint::record(size_of::<ReasoningBlock>())
            .ok_or_else(|| budget.error())?
            .checked_add(
                crate::derived_response::origin_footprint(target).ok_or_else(|| budget.error())?,
            )
            .and_then(|value| value.checked_add(DerivedFootprint::strings(data.len())))
            .and_then(|value| value.checked_multiply(2))
            .ok_or_else(|| budget.error())?;
        budget.reserve(footprint)?;
        ensure_origin(origin, target, budget)?;
        let mut b = ReasoningBlock::text("", origin.as_ref().unwrap().clone());
        b.kind = ReasoningKind::Redacted;
        b.text = None;
        b.data = Some(data.into());
        blocks.push(b);
    }
    Ok(blocks)
}

pub(super) fn legacy_chat_blocks(message: &Value, origin: &ReasoningOrigin) -> Vec<ReasoningBlock> {
    for field in ["reasoning_details", "thinking_blocks"] {
        let mut blocks = Vec::new();
        for payload in message[field].as_array().into_iter().flatten() {
            if !legacy_structured_block_emits(payload) {
                continue;
            }
            let mut block = ReasoningBlock::text("", origin.clone());
            match payload["type"].as_str().unwrap_or("") {
                "reasoning" => {
                    block.item_id = payload["id"].as_str().map(str::to_owned);
                    if let Some(data) = payload["encrypted_content"].as_str() {
                        block.kind = ReasoningKind::Encrypted;
                        block.text = None;
                        block.data = Some(data.into());
                    } else {
                        block.text = Some(summary_text(payload));
                    }
                }
                "redacted_thinking" | "reasoning.encrypted" => {
                    let data = payload["data"]
                        .as_str()
                        .expect("legacy eligibility requires string data");
                    block.kind = if payload["type"] == "redacted_thinking" {
                        ReasoningKind::Redacted
                    } else {
                        ReasoningKind::Encrypted
                    };
                    block.text = None;
                    block.data = Some(data.into());
                }
                "thinking" => {
                    block.text = Some(payload["thinking"].as_str().unwrap_or("").into());
                    block.signature = payload["signature"].as_str().map(str::to_owned);
                }
                "reasoning.text" => {
                    block.text = Some(payload["text"].as_str().unwrap_or("").into());
                    block.signature = payload["signature"].as_str().map(str::to_owned);
                }
                "reasoning.summary" => {
                    block.text = Some(payload["summary"].as_str().unwrap_or("").into());
                }
                _ if payload["thought"] == true => {
                    block.text = Some(payload["text"].as_str().unwrap_or("").into());
                    block.signature = payload["thoughtSignature"].as_str().map(str::to_owned);
                }
                _ => continue,
            }
            block.payload = Some(payload.clone());
            block.source_field = Some(field.into());
            blocks.push(block);
        }
        if !blocks.is_empty() {
            return blocks;
        }
    }
    let mut blocks = Vec::new();
    if let Some(text) = message["reasoning_content"]
        .as_str()
        .or_else(|| message["reasoning"].as_str())
    {
        let mut block = ReasoningBlock::text(text, origin.clone());
        block.signature = message["reasoning_signature"].as_str().map(str::to_owned);
        block.source_field = Some(
            if message["reasoning_content"].is_string() {
                "reasoning_content"
            } else {
                "reasoning"
            }
            .into(),
        );
        blocks.push(block);
    }
    if let Some(data) = message["redacted_reasoning_content"].as_str() {
        let mut block = ReasoningBlock::text("", origin.clone());
        block.kind = ReasoningKind::Redacted;
        block.text = None;
        block.data = Some(data.into());
        blocks.push(block);
    }
    blocks
}

fn stamp_tool_signatures(
    message: &mut Value,
    target: &ReplayTarget,
    origin: &mut Option<ReasoningOrigin>,
    budget: &mut DerivedResponseBudget,
) -> crate::error::Result<()> {
    if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
        for call in calls {
            if let Some(signature) = call.get("thought_signature") {
                let Some(data) = signature.as_str() else {
                    return Err(budget.error());
                };
                let footprint = DerivedFootprint::record(size_of::<ThoughtSignature>())
                    .ok_or_else(|| budget.error())?
                    .checked_add(
                        crate::derived_response::origin_footprint(target)
                            .ok_or_else(|| budget.error())?,
                    )
                    .and_then(|value| value.checked_add(DerivedFootprint::strings(data.len())))
                    .and_then(|value| value.checked_multiply(2))
                    .ok_or_else(|| budget.error())?;
                budget.reserve(footprint)?;
                ensure_origin(origin, target, budget)?;
                call["thought_signature"] = json!(ThoughtSignature {
                    data: data.to_owned(),
                    origin: origin.as_ref().unwrap().clone()
                });
            }
        }
    }
    Ok(())
}

/// Normalize from the original native response, before lossy text projections
/// can discard ordered blocks, ids, signatures, or encrypted item contents.
pub fn capture_response(
    target: &ReplayTarget,
    native: &Value,
    response: &mut Value,
) -> crate::error::Result<()> {
    let mut budget = DerivedResponseBudget::unary();
    capture_response_with_budget(target, native, response, &mut budget)
}

pub(crate) fn capture_response_with_budget(
    target: &ReplayTarget,
    native: &Value,
    response: &mut Value,
    budget: &mut DerivedResponseBudget,
) -> crate::error::Result<()> {
    let mut origin = None;
    if let Some(choices) = response.get_mut("choices").and_then(Value::as_array_mut) {
        for (index, choice) in choices.iter_mut().enumerate() {
            let Some(message) = choice.get_mut("message") else {
                continue;
            };
            let blocks: Vec<ReasoningBlock> = match target.wire {
                WireFormat::AnthropicMessages => {
                    let mut blocks = Vec::new();
                    for payload in native["content"].as_array().into_iter().flatten() {
                        if let Some(block) = structured_block_with_budget(
                            payload,
                            target,
                            &mut origin,
                            None,
                            true,
                            budget,
                        )? {
                            blocks.push(block);
                        }
                    }
                    blocks
                }
                WireFormat::OpenAiResponses => {
                    let mut blocks = Vec::new();
                    for payload in native["output"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|payload| payload["type"] == "reasoning")
                    {
                        if let Some(block) = structured_block_with_budget(
                            payload,
                            target,
                            &mut origin,
                            None,
                            true,
                            budget,
                        )? {
                            blocks.push(block);
                        }
                    }
                    blocks
                }
                WireFormat::OpenAiChat => chat_blocks(
                    &native["choices"][index]["message"],
                    target,
                    &mut origin,
                    budget,
                )?,
                WireFormat::GoogleGenerateContent => {
                    let mut blocks = Vec::new();
                    for payload in native["candidates"][index]["content"]["parts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                    {
                        if let Some(block) = structured_block_with_budget(
                            payload,
                            target,
                            &mut origin,
                            None,
                            true,
                            budget,
                        )? {
                            blocks.push(block);
                        }
                    }
                    blocks
                }
            };
            strip_fields(message);
            if !blocks.is_empty() {
                message["reasoning"] = json!(blocks);
            }
            stamp_tool_signatures(message, target, &mut origin, budget)?;
        }
    }
    crate::providers::anthropic_signature::observe(response, &target.model);
    Ok(())
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
    let mut budget = DerivedResponseBudget::stream();
    capture_stream_with_budget(target, native, normalized, &mut budget)
}

pub(crate) fn capture_stream_with_budget(
    target: &ReplayTarget,
    native: &Value,
    normalized: Option<String>,
    budget: &mut DerivedResponseBudget,
) -> crate::error::Result<Option<String>> {
    let mut origin = None;
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
                if let Some(b) = structured_block_with_budget(
                    &native["content_block"],
                    target,
                    &mut origin,
                    None,
                    true,
                    budget,
                )? {
                    reasoning.push(delta(b, index, true));
                }
            }
            Some("content_block_delta") => {
                let d = &native["delta"];
                match d["type"].as_str() {
                    Some("thinking_delta") => {
                        let text = d["thinking"].as_str().unwrap_or("");
                        reserve_plain_block(target, budget, text.len())?;
                        ensure_origin(&mut origin, target, budget)?;
                        let mut b = ReasoningBlock::text("", origin.as_ref().unwrap().clone());
                        b.text = Some(text.into());
                        reasoning.push(delta(b, index, false));
                    }
                    Some("signature_delta") => {
                        let signature = d["signature"].as_str().unwrap_or("");
                        reserve_plain_block(target, budget, signature.len())?;
                        ensure_origin(&mut origin, target, budget)?;
                        let mut b = ReasoningBlock::text("", origin.as_ref().unwrap().clone());
                        b.text = None;
                        b.signature = Some(signature.into());
                        reasoning.push(delta(b, index, false));
                    }
                    _ => {}
                }
            }
            _ => {}
        },
        WireFormat::OpenAiResponses => match native["type"].as_str() {
            Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => {
                let text = native["delta"].as_str().unwrap_or("");
                let item_id_bytes = native["item_id"].as_str().map(str::len).unwrap_or(0);
                reserve_plain_block(
                    target,
                    budget,
                    text.len()
                        .checked_add(item_id_bytes)
                        .ok_or_else(|| budget.error())?,
                )?;
                ensure_origin(&mut origin, target, budget)?;
                let mut b = ReasoningBlock::text(text, origin.as_ref().unwrap().clone());
                b.item_id = native["item_id"].as_str().map(str::to_owned);
                reasoning.push(delta(b, index, false));
            }
            Some("response.output_item.done") if native["item"]["type"] == "reasoning" => {
                if let Some(b) = structured_block_with_budget(
                    &native["item"],
                    target,
                    &mut origin,
                    None,
                    true,
                    budget,
                )? {
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
                        if let Some(b) = structured_block_with_budget(
                            item,
                            target,
                            &mut origin,
                            None,
                            true,
                            budget,
                        )? {
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
                if let Some(b) =
                    structured_block_with_budget(part, target, &mut origin, None, false, budget)?
                {
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
                    let values: Vec<_> = chat_blocks(raw, target, &mut origin, budget)?
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
                    stamp_tool_signatures(d, target, &mut origin, budget)?;
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
        stamp_tool_signatures(d, target, &mut origin, budget)?;
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
#[derive(Debug)]
pub struct ReasoningAccumulator {
    parts: BTreeMap<String, ReasoningPart>,
    order: Vec<String>,
    budget: crate::stream_retention::RetainedBudget,
    retained: crate::stream_retention::RetainedFootprint,
}

#[derive(Debug)]
struct ReasoningPart {
    value: Value,
    footprint: crate::stream_retention::RetainedFootprint,
}

impl Default for ReasoningAccumulator {
    fn default() -> Self {
        let limits = crate::stream_retention::StreamRetentionLimits::default();
        Self::with_budget(crate::stream_retention::RetainedBudget::new(
            limits.normalizer_bytes,
            limits.normalizer_entries,
        ))
    }
}

impl ReasoningAccumulator {
    pub(crate) fn with_budget(budget: crate::stream_retention::RetainedBudget) -> Self {
        Self {
            parts: BTreeMap::new(),
            order: Vec::new(),
            budget,
            retained: Default::default(),
        }
    }

    pub fn with_retention_limits(
        limits: crate::streaming::StreamRetentionLimits,
    ) -> crate::error::Result<Self> {
        let limits = crate::stream_retention::StreamRetentionLimits::new(
            limits.normalizer_bytes,
            limits.normalizer_entries,
            limits.native_usage_bytes,
            limits.native_usage_entries,
        )?;
        Ok(Self::with_budget(
            crate::stream_retention::RetainedBudget::new(
                limits.normalizer_bytes,
                limits.normalizer_entries,
            ),
        ))
    }

    /// Add normalized reasoning fragments, returning a fixed stream error if
    /// their retained text, opaque data, identities, or metadata exceed the
    /// per-response state budget.
    pub fn push(&mut self, message: &Value) -> crate::error::Result<()> {
        let result = self.push_inner(message);
        if result.is_err() {
            self.clear();
        }
        result
    }

    fn push_inner(&mut self, message: &Value) -> crate::error::Result<()> {
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
            if fragment["replace"] == true || !self.parts.contains_key(&key) {
                let mut snapshot = fragment.clone();
                if let Some(previous) = self.parts.get(&key) {
                    snapshot["origin"] = previous.value["origin"].clone();
                }
                let value_footprint = crate::stream_retention::estimate_value(&snapshot)?;
                if let Some(previous) = self.parts.get(&key) {
                    self.budget.replace(previous.footprint, value_footprint)?;
                    replace_footprint(&mut self.retained, previous.footprint, value_footprint);
                } else {
                    let key_footprint = crate::stream_retention::RetainedFootprint::record(
                        key.capacity().saturating_mul(2),
                    );
                    let added = key_footprint
                        .checked_add(value_footprint)
                        .ok_or_else(crate::stream_retention::retention_error)?;
                    self.budget.reserve(added)?;
                    self.retained = self
                        .retained
                        .checked_add(added)
                        .ok_or_else(crate::stream_retention::retention_error)?;
                    self.order.push(key.clone());
                }
                self.parts.insert(
                    key,
                    ReasoningPart {
                        value: snapshot,
                        footprint: value_footprint,
                    },
                );
            } else if let Some(part) = self.parts.get_mut(&key) {
                let block = &mut part.value;
                if let Some(block_object) = block.as_object_mut() {
                    for field in ["text", "signature", "data"] {
                        if let Some(text) = fragment[field].as_str() {
                            append_retained_object_string(
                                &self.budget,
                                &mut self.retained,
                                &mut part.footprint,
                                block_object,
                                field,
                                text,
                            )?;
                        }
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
                                append_retained_object_string(
                                    &self.budget,
                                    &mut self.retained,
                                    &mut part.footprint,
                                    previous,
                                    field,
                                    value.as_str().unwrap(),
                                )?;
                            } else {
                                replace_retained_value(
                                    &self.budget,
                                    &mut self.retained,
                                    &mut part.footprint,
                                    previous,
                                    field,
                                    value,
                                )?;
                            }
                        }
                    } else {
                        let block_object = block
                            .as_object_mut()
                            .ok_or_else(crate::stream_retention::retention_error)?;
                        replace_retained_value(
                            &self.budget,
                            &mut self.retained,
                            &mut part.footprint,
                            block_object,
                            "payload",
                            payload,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn blocks(&self) -> Vec<Value> {
        self.order
            .iter()
            .filter_map(|key| self.parts.get(key))
            .map(|part| part.value.clone())
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

    pub(crate) fn take_blocks(&mut self) -> Vec<Value> {
        let blocks = self.blocks();
        self.clear();
        blocks
    }

    pub(crate) fn clear(&mut self) {
        self.parts.clear();
        self.order.clear();
        self.budget.release(self.retained);
        self.retained = Default::default();
    }
}

fn append_retained_string(
    budget: &crate::stream_retention::RetainedBudget,
    retained: &mut crate::stream_retention::RetainedFootprint,
    part_footprint: &mut crate::stream_retention::RetainedFootprint,
    destination: &mut Value,
    fragment: &str,
) -> crate::error::Result<()> {
    let previous = crate::stream_retention::estimate_value(destination)?;
    match destination {
        Value::String(assembled) => {
            assembled
                .try_reserve(fragment.len())
                .map_err(|_| crate::stream_retention::retention_error())?;
            assembled.push_str(fragment);
        }
        _ => *destination = Value::String(fragment.to_owned()),
    }
    let replacement = crate::stream_retention::estimate_value(destination)?;
    budget.replace(previous, replacement)?;
    replace_footprint(retained, previous, replacement);
    part_footprint.bytes = part_footprint
        .bytes
        .saturating_sub(previous.bytes)
        .saturating_add(replacement.bytes);
    part_footprint.entries = part_footprint
        .entries
        .saturating_sub(previous.entries)
        .saturating_add(replacement.entries);
    Ok(())
}

fn append_retained_object_string(
    budget: &crate::stream_retention::RetainedBudget,
    retained: &mut crate::stream_retention::RetainedFootprint,
    part_footprint: &mut crate::stream_retention::RetainedFootprint,
    object: &mut serde_json::Map<String, Value>,
    field: &str,
    fragment: &str,
) -> crate::error::Result<()> {
    if let Some(destination) = object.get_mut(field) {
        append_retained_string(budget, retained, part_footprint, destination, fragment)
    } else {
        replace_retained_value(
            budget,
            retained,
            part_footprint,
            object,
            field,
            &Value::String(fragment.to_owned()),
        )
    }
}

fn replace_retained_value(
    budget: &crate::stream_retention::RetainedBudget,
    retained: &mut crate::stream_retention::RetainedFootprint,
    part_footprint: &mut crate::stream_retention::RetainedFootprint,
    object: &mut serde_json::Map<String, Value>,
    field: &str,
    value: &Value,
) -> crate::error::Result<()> {
    let previous = object
        .get(field)
        .map(crate::stream_retention::estimate_value)
        .transpose()?
        .unwrap_or_default();
    let mut replacement = crate::stream_retention::estimate_value(value)?;
    if !object.contains_key(field) {
        replacement = replacement
            .checked_add(crate::stream_retention::RetainedFootprint::record(
                field.len(),
            ))
            .ok_or_else(crate::stream_retention::retention_error)?;
    }
    budget.replace(previous, replacement)?;
    replace_footprint(retained, previous, replacement);
    part_footprint.bytes = part_footprint
        .bytes
        .saturating_sub(previous.bytes)
        .saturating_add(replacement.bytes);
    part_footprint.entries = part_footprint
        .entries
        .saturating_sub(previous.entries)
        .saturating_add(replacement.entries);
    object.insert(field.to_owned(), value.clone());
    Ok(())
}

fn replace_footprint(
    retained: &mut crate::stream_retention::RetainedFootprint,
    previous: crate::stream_retention::RetainedFootprint,
    replacement: crate::stream_retention::RetainedFootprint,
) {
    retained.bytes = retained
        .bytes
        .saturating_sub(previous.bytes)
        .saturating_add(replacement.bytes);
    retained.entries = retained
        .entries
        .saturating_sub(previous.entries)
        .saturating_add(replacement.entries);
}

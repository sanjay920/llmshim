//! Credential-scoped reasoning receipts, never client-asserted provenance.
use super::super::{
    receipts::{RestorationBudget, RestorationLimits},
    Receipts, Result,
};
use serde_json::{json, Value};

/// Accepts canonical history and issued receipts; refuses malformed shapes or excess restoration.
pub(in crate::proxy::wire) fn restore(
    chat: &mut Value,
    native: &Value,
    receipts: &Receipts,
    scope: &str,
    limits: RestorationLimits,
) -> Result<()> {
    let messages = chat
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or("malformed canonical messages: expected an array")?;
    let calls: std::collections::BTreeMap<_, _> = native["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call" | "custom_tool_call")
            )
        })
        .filter_map(|item| item["call_id"].as_str().map(|id| (id, item)))
        .collect();
    let mut budget = RestorationBudget::new(limits);
    for message in messages.iter_mut() {
        for call in message
            .get_mut("tool_calls")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if let Some(item) = call["id"].as_str().and_then(|id| calls.get(id)) {
                let mut descriptor = json!({"type": item["type"], "name": item["name"]});
                if let Some(namespace) = item.get("namespace") {
                    descriptor["namespace"] = namespace.clone();
                }
                call["x-responses-call"] = descriptor;
            }
        }
        if let Some(item) = message
            .as_object_mut()
            .ok_or("malformed canonical message: expected an object")?
            .remove("x-responses-reasoning")
        {
            match receipts.get_bounded(scope, "responses-reasoning", &item, &mut budget)? {
                Some(block) => message["reasoning"] = json!([block]),
                None => message["x-responses-reasoning-dropped"] = json!("unissued_or_expired"),
            }
        }
    }
    Ok(())
}

fn merge_assistant_items(messages: &mut Vec<Value>) {
    let mut index = 0;
    while index + 1 < messages.len() {
        let left = &messages[index];
        let right = &messages[index + 1];
        if left["role"] != "assistant"
            || right["role"] != "assistant"
            || left.get("x-responses-reasoning-dropped").is_some()
            || right.get("x-responses-reasoning-dropped").is_some()
            || ![left, right]
                .iter()
                .any(|item| item["reasoning"].is_array() || item["tool_calls"].is_array())
        {
            index += 1;
            continue;
        }
        let next = messages.remove(index + 1);
        let message = &mut messages[index];
        for field in ["reasoning", "tool_calls"] {
            let mut parts = message[field].as_array().cloned().unwrap_or_default();
            parts.extend(next[field].as_array().into_iter().flatten().cloned());
            if !parts.is_empty() {
                message[field] = json!(parts);
            }
        }
        if message["content"].is_null() {
            message["content"] = next["content"].clone();
        } else if !next["content"].is_null() {
            let mut content = Vec::new();
            for source in [&message["content"], &next["content"]] {
                if let Some(text) = source.as_str() {
                    content.push(json!({"type": "text","text": text}));
                } else {
                    content.extend(source.as_array().into_iter().flatten().cloned());
                }
            }
            message["content"] = json!(content);
        }
    }
}

/// Accepts issued output and canonical reasoning; refuses receipt storage failures.
pub(in crate::proxy::wire) fn retain(
    response: &super::Response,
    message: &Value,
    receipts: &Receipts,
    scope: &str,
) -> Result<()> {
    let items = response
        .output
        .iter()
        .filter(|item| item.kind == "reasoning");
    let blocks = message["reasoning"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| {
            block["payload"]["summary"]
                .as_array()
                .is_some_and(|parts| !parts.is_empty())
                || block["data"].is_string()
                || block["signature"].is_string()
        });
    for (item, block) in items.zip(blocks) {
        receipts.put(scope, "responses-reasoning", &json!(item), block)?;
        let mut summary = item.clone();
        summary.fields.remove("encrypted_content");
        receipts.put(scope, "responses-reasoning", &json!(summary), block)?;
        if item.fields.get("summary") != Some(&json!([])) {
            let mut omitted = item.clone();
            omitted.fields.insert("summary".into(), json!([]));
            receipts.put(scope, "responses-reasoning", &json!(omitted), block)?;
            omitted.fields.remove("encrypted_content");
            receipts.put(scope, "responses-reasoning", &json!(omitted), block)?;
        }
    }
    Ok(())
}

/// Accepts canonical history and a replay target; refuses malformed canonical message shapes.
pub(in crate::proxy::wire) fn replay_metadata(
    chat: &mut Value,
    target: Option<&crate::reasoning::ReplayTarget>,
) -> Result<Value> {
    let tools = chat["provider_config"]["x-responses-loaded-tools"].clone();
    let controls = chat["provider_config"]["x-responses-controls"].clone();
    let messages = chat
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or("malformed canonical messages: expected an array")?;
    let mut dropped = Vec::new();
    for message in messages.iter_mut() {
        if let Some(reason) = message
            .as_object_mut()
            .ok_or("malformed canonical message: expected an object")?
            .remove("x-responses-reasoning-dropped")
        {
            dropped.push(reason);
        }
    }
    for message in messages.iter_mut() {
        if let Some(target) = target {
            let before = message["reasoning"].as_array().map_or(0, Vec::len);
            if crate::reasoning::filter_reasoning_for_target(message, target).is_ok() {
                let after = message["reasoning"].as_array().map_or(0, Vec::len);
                dropped.extend((after..before).map(|_| json!("incompatible_target")));
            }
        }
    }
    messages.retain(|message| {
        message["role"] != "assistant"
            || !message["content"].is_null()
            || message["reasoning"].is_array()
            || message["tool_calls"].is_array()
    });
    if target.is_none_or(|target| target.wire != crate::reasoning::WireFormat::OpenAiResponses) {
        merge_assistant_items(messages);
    }
    let mut metadata = json!({});
    if controls["text"].get("verbosity").is_some()
        && target.is_some_and(|target| !crate::responses_tools::supports_verbosity(target))
    {
        metadata["controls_dropped"] = json!(["text.verbosity"]);
    }
    if tools.is_array() {
        metadata["x-responses-tools"] = tools;
    }
    if controls.is_object() {
        metadata["x-responses-controls"] = controls;
    }
    if !dropped.is_empty() {
        metadata["reasoning_dropped"] = json!(dropped);
    }
    Ok(metadata)
}

/// Accepts issued responses and include selection; omits unrequested encrypted content.
pub(in crate::proxy::wire) fn output_options(
    response: &mut super::Response,
    mut metadata: Value,
    include: bool,
) -> Result<()> {
    let tools = metadata
        .as_object_mut()
        .and_then(|metadata| metadata.remove("x-responses-tools"))
        .unwrap_or(Value::Null);
    let controls = metadata
        .as_object_mut()
        .and_then(|metadata| metadata.remove("x-responses-controls"))
        .unwrap_or(Value::Null);
    if controls["parallel_tool_calls"] == false
        && response
            .output
            .iter()
            .filter(|item| item.kind == "function_call")
            .count()
            > 1
    {
        return Err("parallel_tool_calls is false but the provider returned multiple calls".into());
    }
    if controls["reasoning"]["summary"] == "none" {
        for item in &mut response.output {
            if item.kind == "reasoning" {
                item.fields.insert("summary".into(), json!([]));
            }
        }
    }
    let mut declarations = crate::responses_tools::declarations(&tools);
    for item in &response.output {
        if item.kind == "tool_search_output" {
            if let Some(tools) = item.fields.get("tools") {
                declarations.extend(crate::responses_tools::declarations(tools));
            }
        }
    }
    for item in &mut response.output {
        if item.kind != "function_call" {
            continue;
        }
        if let Some(tool) = declarations.iter().find(|tool| {
            crate::responses_tools::name(
                tool["name"].as_str().unwrap_or(""),
                tool["namespace"].as_str(),
            ) == item
                .fields
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
        }) {
            item.fields.insert("name".into(), tool["name"].clone());
            if let Some(namespace) = tool.get("namespace") {
                item.fields.insert("namespace".into(), namespace.clone());
            }
            if tool["type"] == "custom" {
                let arguments = item
                    .fields
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or("custom tool arguments must be text")?;
                let input = crate::responses_tools::custom_input(arguments)?;
                item.kind = "custom_tool_call".into();
                item.fields.remove("arguments");
                item.fields.insert("input".into(), json!(input));
            }
        }
    }
    response.fields.insert("metadata".into(), metadata);
    if !include {
        for item in &mut response.output {
            item.fields.remove("encrypted_content");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::wire::{request_to_chat_with_limits, Wire};

    #[test]
    fn canonical_shapes_return_named_errors() {
        let receipts = Receipts::new(std::path::PathBuf::from(format!(
            "target/responses-shapes-{}",
            uuid::Uuid::new_v4()
        )));
        let mut valid = json!({"messages": []});
        assert!(restore(
            &mut valid,
            &json!({}),
            &receipts,
            "scope",
            RestorationLimits::default()
        )
        .is_ok());
        assert_eq!(replay_metadata(&mut valid, None).unwrap(), json!({}));
        for (mut chat, message) in [
            (
                Value::Null,
                "malformed canonical messages: expected an array",
            ),
            (json!(7), "malformed canonical messages: expected an array"),
            (json!({}), "malformed canonical messages: expected an array"),
            (
                json!({"messages": {}}),
                "malformed canonical messages: expected an array",
            ),
            (
                json!({"messages": [null]}),
                "malformed canonical message: expected an object",
            ),
        ] {
            assert_eq!(
                restore(
                    &mut chat,
                    &json!({}),
                    &receipts,
                    "scope",
                    RestorationLimits::default()
                )
                .unwrap_err(),
                message
            );
            assert_eq!(replay_metadata(&mut chat, None).unwrap_err(), message);
        }
    }

    #[test]
    fn restored_block_and_envelope_share_the_canonical_limit() {
        let receipts = Receipts::new(std::path::PathBuf::from(format!(
            "target/responses-bounds-{}",
            uuid::Uuid::new_v4()
        )));
        let item = json!({"type": "reasoning","id": "rs_issued","summary": []});
        let block = json!({
            "kind": "text",
            "text": "a".repeat(700),
            "signature": "signature",
            "origin": {
                "provider": "anthropic",
                "model": "claude-sonnet-5-5",
                "family": null,
                "wire": "anthropic-messages",
                "received_at": "2026-10-01T00:00:00Z"
            }
        });
        receipts
            .put("scope", "responses-reasoning", &item, &block)
            .unwrap();
        let native = json!({
            "model": "anthropic/claude-sonnet-5-5",
            "input": [
                item,
                {
                    "role": "user",
                    "content": "continue"
                }
            ]
        });
        let bytes = block.to_string().len();
        let mut limits = RestorationLimits {
            max_serialized_bytes: bytes + 500,
            ..RestorationLimits::default()
        };
        let canonical =
            request_to_chat_with_limits(&native, Wire::Responses, &receipts, "scope", limits)
                .unwrap();
        assert_eq!(canonical["messages"][0]["reasoning"][0], block);
        // The receipt fits alone; the request envelope must still consume nodes.
        let nodes =
            crate::json_bounds::measure_value(&canonical, crate::json_bounds::Limits::INBOUND)
                .unwrap()
                .nodes;
        limits.max_nodes = nodes;
        assert!(
            request_to_chat_with_limits(&native, Wire::Responses, &receipts, "scope", limits)
                .is_ok()
        );
        limits.max_nodes = nodes - 1;
        assert_eq!(
            request_to_chat_with_limits(&native, Wire::Responses, &receipts, "scope", limits)
                .unwrap_err(),
            super::super::super::receipts::REQUEST_LIMIT_ERROR_MESSAGE
        );
    }
}

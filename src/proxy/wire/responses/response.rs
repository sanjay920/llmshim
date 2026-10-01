//! Canonical completions rendered as Responses output items and usage.
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Deserialize, Serialize)]
pub(in crate::proxy::wire) struct Response {
    pub id: String,
    pub output: Vec<OutputItem>,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(in crate::proxy::wire) struct OutputItem {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<Value>>,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

/// Accepts canonical completions and usage; omits empty text and private thinking.
pub(in crate::proxy::wire) fn response(canonical: &Value, usage: &Value, finish: &str) -> Response {
    let message = &canonical["message"];
    let mut content = Vec::new();
    if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
        content.push(json!({
            "type": "output_text",
            "text": text,
            "annotations": [],
        }));
    } else if let Some(parts) = message["content"].as_array() {
        for part in parts {
            if let Some(text) = part["text"].as_str() {
                content.push(json!({
                    "type": "output_text",
                    "text": text,
                    "annotations": [],
                }));
            }
        }
    }
    if let Some(refusal) = message["refusal"].as_str() {
        content.push(json!({
            "type": "refusal",
            "refusal": refusal,
        }));
    }
    let incomplete = finish == "length";
    let status = if incomplete {
        "incomplete"
    } else {
        "completed"
    };
    let response_id = format!(
        "{:x}",
        Sha256::digest(canonical["id"].to_string().as_bytes())
    );
    let mut output = Vec::new();
    if !content.is_empty() {
        output.push(OutputItem {
            id: format!("msg_{response_id}_{}", output.len()),
            kind: "message".into(),
            content: Some(content),
            fields: Map::from_iter([
                ("status".into(), json!(status)),
                ("role".into(), json!("assistant")),
            ]),
        });
    }
    for block in message["reasoning"].as_array().into_iter().flatten() {
        let summary = block["payload"]["summary"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if !summary.is_empty() || block["data"].is_string() || block["signature"].is_string() {
            let mut item = OutputItem {
                id: format!("rs_{response_id}_{}", output.len()),
                kind: "reasoning".into(),
                content: None,
                fields: Map::from_iter([("summary".into(), json!(summary))]),
            };
            if block["origin"]["wire"] == "openai-responses" {
                if let Some(data) = block.get("data") {
                    item.fields.insert("encrypted_content".into(), data.clone());
                }
            }
            output.push(item);
        }
    }
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        output.push(OutputItem {
            id: format!("fc_{response_id}_{}", output.len()),
            kind: "function_call".into(),
            content: None,
            fields: Map::from_iter([
                ("status".into(), json!(status)),
                ("call_id".into(), call["id"].clone()),
                ("name".into(), call["function"]["name"].clone()),
                ("arguments".into(), call["function"]["arguments"].clone()),
            ]),
        });
    }
    let mut remaining = output;
    let mut output = Vec::new();
    for native in message["responses_output"].as_array().into_iter().flatten() {
        let kind = native["type"].as_str().unwrap_or_default();
        if matches!(
            kind,
            "tool_search_call" | "tool_search_output" | "web_search_call"
        ) {
            if let Ok(item) = serde_json::from_value::<OutputItem>(native.clone()) {
                output.push(item);
            }
            continue;
        }
        let call_id = message["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|call| {
                call["wire_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|binding| {
                        binding["wire"] == "openai-responses" && binding["id"] == native["call_id"]
                    })
            })
            .map(|call| &call["id"]);
        let index = remaining.iter().position(|item| {
            if matches!(kind, "function_call" | "custom_tool_call") {
                item.kind == "function_call" && item.fields.get("call_id") == call_id
            } else {
                item.kind == kind
            }
        });
        if let Some(index) = index {
            output.push(remaining.remove(index));
        }
    }
    output.extend(remaining);
    let incomplete_details = if incomplete {
        json!({"reason": "max_output_tokens"})
    } else {
        Value::Null
    };
    Response {
        id: format!("resp_{response_id}"),
        output,
        fields: Map::from_iter([
            ("object".into(), json!("response")),
            ("created_at".into(), canonical["created_at"].clone()),
            ("model".into(), canonical["model"].clone()),
            ("status".into(), json!(status)),
            ("error".into(), Value::Null),
            ("incomplete_details".into(), incomplete_details),
            ("store".into(), json!(false)),
            ("previous_response_id".into(), Value::Null),
            ("usage".into(), response_usage(usage)),
        ]),
    }
}

/// Renders normalized counters in the Responses convention: `input_tokens` counts every input
/// token, cached or not, whatever convention the upstream provider reported in.
fn response_usage(usage: &Value) -> Value {
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let input = count("uncached_input_tokens")
        .saturating_add(count("cache_read_tokens"))
        .saturating_add(count("cache_write_tokens"));
    json!({
        "input_tokens": input,
        "output_tokens": usage["output_tokens"],
        "total_tokens": usage["total_tokens"],
        "input_tokens_details": {
            "cached_tokens": usage["cache_read_tokens"],
        },
        "output_tokens_details": {
            "reasoning_tokens": usage["reasoning_tokens"].as_u64().unwrap_or(0),
        },
        "cost_usd": usage["cost_usd"],
        "cost_source": usage["cost_source"],
    })
}

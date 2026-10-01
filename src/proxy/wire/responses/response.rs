//! Canonical completions rendered as Responses output items and usage.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Hashes the canonical ID so Responses response/item IDs remain stable on idempotent replay.
pub(in crate::proxy::wire) fn response(canonical: &Value, usage: &Value, finish: &str) -> Value {
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
    for block in message["reasoning"].as_array().into_iter().flatten() {
        if let Some(summary) = block["payload"]["summary"]
            .as_array()
            .filter(|summary| !summary.is_empty())
        {
            // A provider's full reasoning text is not a published summary.
            output.push(json!({
                "id": format!("rs_{response_id}_{}", output.len()),
                "type": "reasoning",
                "summary": summary,
            }));
        }
    }
    if !content.is_empty() {
        output.push(json!({
            "id": format!("msg_{response_id}_{}", output.len()),
            "type": "message",
            "status": status,
            "role": "assistant",
            "content": content,
        }));
    }
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        output.push(json!({
            "id": format!("fc_{response_id}_{}", output.len()),
            "type": "function_call",
            "status": status,
            "call_id": call["id"],
            "name": call["function"]["name"],
            "arguments": call["function"]["arguments"],
        }));
    }
    let incomplete_details = if incomplete {
        json!({"reason": "max_output_tokens"})
    } else {
        Value::Null
    };
    json!({
        "id": format!("resp_{response_id}"),
        "object": "response",
        "created_at": canonical["created_at"],
        "model": canonical["model"],
        "status": status,
        "error": null,
        "incomplete_details": incomplete_details,
        "output": output,
        "store": false,
        "previous_response_id": null,
        "usage": response_usage(usage),
    })
}

/// Accepts normalized token/cost counters and defaults absent reasoning tokens to zero.
fn response_usage(usage: &Value) -> Value {
    json!({
        "input_tokens": usage["input_tokens"],
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

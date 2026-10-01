use super::{call_content, gemini, message_key, native_call, Receipts, Result, Wire};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub fn response_from_chat(
    response: &Value,
    wire: Wire,
    receipts: &Receipts,
    scope: &str,
) -> Result<Value> {
    let message = &response["message"];
    let mut usage = response["usage"].clone();
    if !usage.is_object() {
        usage = json!({});
    }
    for key in [
        "input_tokens",
        "uncached_input_tokens",
        "reasoning_tokens",
        "output_tokens",
        "total_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ] {
        if !usage[key].is_u64() {
            usage[key] = json!(0);
        }
    }
    let calls = message["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for call in &calls {
        receipts.put(scope, "call", &call["id"], call)?;
    }
    let finish = response["finish_reason"]
        .as_str()
        .unwrap_or(if !calls.is_empty() {
            "tool_calls"
        } else {
            "stop"
        });
    let mut out = if wire == Wire::Responses {
        super::responses::response(response, &usage, finish)
    } else if wire == Wire::Gemini {
        gemini::response(response, &usage, finish, receipts, scope)?
    } else if wire == Wire::Chat {
        let mut exported = json!({"role":"assistant","content":message["content"]});
        if let Some(refusal) = message.get("refusal") {
            exported["refusal"] = refusal.clone();
        }
        if !calls.is_empty() {
            exported["tool_calls"] = json!(calls.iter().map(native_call).collect::<Vec<_>>());
        }
        if let Some(reasoning) = message.get("reasoning").filter(|r| r.is_array()) {
            exported["reasoning_details"] = reasoning.clone();
            exported["reasoning_content"] = json!(crate::reasoning::reasoning_text(message));
            receipts.put(scope, "reasoning", &message_key(&exported), reasoning)?;
        }
        json!({"id":response["id"],"object":"chat.completion","created":response.get("created").cloned().unwrap_or(json!(chrono::Utc::now().timestamp())),"model":response["model"],"choices":[{"index":0,"message":exported,"finish_reason":finish}],"usage":{"prompt_tokens":usage["uncached_input_tokens"].as_u64().unwrap_or(0).saturating_add(usage["cache_read_tokens"].as_u64().unwrap_or(0)).saturating_add(usage["cache_write_tokens"].as_u64().unwrap_or(0)),"completion_tokens":usage["output_tokens"],"total_tokens":usage["total_tokens"],"cache_read_tokens":usage["cache_read_tokens"],"cache_write_tokens":usage["cache_write_tokens"],"cost_usd":usage["cost_usd"],"cost_source":usage["cost_source"],"prompt_tokens_details":{"cached_tokens":usage["cache_read_tokens"]},"completion_tokens_details":{"reasoning_tokens":usage["reasoning_tokens"]}}})
    } else {
        let mut content = Vec::new();
        for block in message["reasoning"].as_array().into_iter().flatten() {
            let same_wire = block["origin"]["wire"] == "anthropic-messages";
            let handle = format!("lsr_{:x}", Sha256::digest(block.to_string().as_bytes()));
            let exported = if block["kind"] == "text" {
                json!({"type":"thinking","thinking":block["text"].as_str().unwrap_or(""),"signature":if same_wire{block["signature"].as_str().unwrap_or(&handle)}else{&handle}})
            } else {
                json!({"type":"redacted_thinking","data":if same_wire{block["data"].as_str().unwrap_or(&handle)}else{&handle}})
            };
            receipts.put(scope, "block", &exported, block)?;
            content.push(exported);
        }
        if let Some(text) = message["content"].as_str().filter(|s| !s.is_empty()) {
            content.push(json!({"type":"text","text":text}));
        } else if let Some(blocks) = message["content"].as_array() {
            content.extend(blocks.iter().cloned());
        }
        if let Some(refusal) = message["refusal"].as_str() {
            content.push(json!({"type":"text","text":refusal}));
        }
        for call in calls {
            let parsed = call_content(&call)?;
            content.push(json!({"type":"tool_use","id":parsed["id"],"name":parsed["name"],"input":parsed["arguments"]}));
        }
        json!({"id":response["id"],"type":"message","role":"assistant","model":response["model"],"content":content,"stop_reason":match finish{"tool_calls"=>"tool_use","length"=>"max_tokens","content_filter"=>"refusal",_=>"end_turn"},"stop_sequence":null,"usage":{"input_tokens":usage["uncached_input_tokens"],"output_tokens":usage["output_tokens"],"cache_read_input_tokens":usage["cache_read_tokens"],"cache_creation_input_tokens":usage["cache_write_tokens"],"cost_usd":usage["cost_usd"],"cost_source":usage["cost_source"]}})
    };
    let usage_field = if wire == Wire::Gemini {
        "usageMetadata"
    } else {
        "usage"
    };
    out[usage_field]["x-llmshim-usage"] = json!({
        "uncached_input_tokens": usage["uncached_input_tokens"],
        "cache_read_tokens": usage["cache_read_tokens"],
        "cache_write_tokens": usage["cache_write_tokens"],
        "output_tokens": usage["output_tokens"],
        "reasoning_tokens": usage["reasoning_tokens"],
    });
    if let Some(served) = response.get("x-llmshim-served-model") {
        out["x-llmshim-served-model"] = served.clone();
    }
    Ok(out)
}

pub fn stream_frames(response: &Value, wire: Wire) -> Vec<(Option<String>, String)> {
    let mut frames = Vec::new();
    if wire == Wire::Gemini {
        return gemini::frames(response);
    }
    if wire == Wire::Chat {
        let mut chunk = response.clone();
        chunk["object"] = json!("chat.completion.chunk");
        let mut message = chunk["choices"][0]
            .as_object_mut()
            .unwrap()
            .remove("message")
            .unwrap();
        if let Some(calls) = message["tool_calls"].as_array_mut() {
            for (index, call) in calls.iter_mut().enumerate() {
                call["index"] = json!(index);
            }
        }
        chunk["choices"][0]["delta"] = message;
        frames.push((None, chunk.to_string()));
        frames.push((None, "[DONE]".into()));
        return frames;
    }
    let mut start = response.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"]["output_tokens"] = json!(0);
    frames.push((
        Some("message_start".into()),
        json!({"type":"message_start","message":start}).to_string(),
    ));
    for (index, block) in response["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let mut start = block.clone();
        let mut deltas = Vec::new();
        match block["type"].as_str() {
            Some("text") => {
                start["text"] = json!("");
                deltas.push(json!({"type":"text_delta","text":block["text"]}));
            }
            Some("thinking") => {
                start["thinking"] = json!("");
                start["signature"] = json!("");
                deltas.push(json!({"type":"thinking_delta","thinking":block["thinking"]}));
                deltas.push(json!({"type":"signature_delta","signature":block["signature"]}));
            }
            Some("tool_use") => {
                start["input"] = json!({});
                deltas.push(
                    json!({"type":"input_json_delta","partial_json":block["input"].to_string()}),
                );
            }
            _ => {}
        }
        frames.push((
            Some("content_block_start".into()),
            json!({"type":"content_block_start","index":index,"content_block":start}).to_string(),
        ));
        for delta in deltas {
            frames.push((
                Some("content_block_delta".into()),
                json!({"type":"content_block_delta","index":index,"delta":delta}).to_string(),
            ));
        }
        frames.push((
            Some("content_block_stop".into()),
            json!({"type":"content_block_stop","index":index}).to_string(),
        ));
    }
    let mut delta = json!({"type":"message_delta","delta":{"stop_reason":response["stop_reason"],"stop_sequence":null},"usage":response["usage"]});
    if let Some(served) = response.get("x-llmshim-served-model") {
        delta["x-llmshim-served-model"] = served.clone();
    }
    frames.push((Some("message_delta".into()), delta.to_string()));
    frames.push((
        Some("message_stop".into()),
        json!({"type":"message_stop"}).to_string(),
    ));
    frames
}

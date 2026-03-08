use super::types::{ChatRequest, ChatResponse, ResponseMessage, StreamEvent, Usage};
use serde_json::{json, Value};

/// Convert our ChatRequest into the OpenAI-format Value that lib.rs expects.
pub fn request_to_value(req: &ChatRequest) -> Value {
    let mut v = json!({
        "model": req.model,
        "messages": req.messages,
    });

    // Apply provider-agnostic config
    if let Some(cfg) = &req.config {
        if let Some(mt) = cfg.max_tokens {
            v["max_tokens"] = json!(mt);
        }
        if let Some(t) = cfg.temperature {
            v["temperature"] = json!(t);
        }
        if let Some(tp) = cfg.top_p {
            v["top_p"] = json!(tp);
        }
        if let Some(tk) = cfg.top_k {
            v["top_k"] = json!(tk);
        }
        if let Some(stop) = &cfg.stop {
            v["stop"] = json!(stop);
        }
        if let Some(effort) = &cfg.reasoning_effort {
            v["reasoning_effort"] = json!(effort);
        }
    }

    // Merge provider_config as top-level keys (passthrough to provider transform)
    if let Some(pc) = &req.provider_config {
        if let Some(obj) = pc.as_object() {
            for (k, val) in obj {
                v[k.clone()] = val.clone();
            }
        }
    }

    v
}

/// Convert the OpenAI-format Value response from lib.rs into our ChatResponse.
pub fn value_to_response(v: &Value, provider: &str, latency_ms: u64) -> ChatResponse {
    let choice = &v["choices"][0];
    let msg = &choice["message"];

    let content = msg.get("content").cloned().unwrap_or(Value::Null);
    let tool_calls = msg.get("tool_calls").cloned().filter(|v| !v.is_null());
    let reasoning = msg
        .get("reasoning_content")
        .and_then(|r| r.as_str())
        .map(String::from);

    let usage = extract_usage(&v["usage"]);

    ChatResponse {
        id: v
            .get("id")
            .and_then(|id| id.as_str())
            .unwrap_or("")
            .to_string(),
        model: v
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string(),
        provider: provider.to_string(),
        message: ResponseMessage {
            role: "assistant".to_string(),
            content,
            tool_calls,
        },
        reasoning,
        usage,
        latency_ms,
    }
}

/// Extract usage from an OpenAI-format usage object.
pub fn extract_usage(usage: &Value) -> Usage {
    let input = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning = usage
        .get("reasoning_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(input + output);

    Usage {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: reasoning,
        total_tokens: total,
    }
}

/// Parse a single OpenAI-format stream chunk into typed SSE events.
/// Returns one or more events per chunk.
pub fn chunk_to_events(chunk_json: &str) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    let parsed: Value = match serde_json::from_str(chunk_json) {
        Ok(v) => v,
        Err(_) => return events,
    };

    let delta = &parsed["choices"][0]["delta"];

    // Reasoning content
    if let Some(reasoning) = delta.get("reasoning_content").and_then(|r| r.as_str()) {
        if !reasoning.is_empty() {
            events.push(StreamEvent::Reasoning {
                text: reasoning.to_string(),
            });
        }
    }

    // Text content
    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            events.push(StreamEvent::Content {
                text: content.to_string(),
            });
        }
    }

    // Tool calls
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|tc| tc.as_array()) {
        for tc in tool_calls {
            if let (Some(id), Some(name)) = (
                tc.get("id").and_then(|i| i.as_str()),
                tc.pointer("/function/name").and_then(|n| n.as_str()),
            ) {
                let args = tc
                    .pointer("/function/arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("")
                    .to_string();
                events.push(StreamEvent::ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments: args,
                });
            }
        }
    }

    // Finish reason → done event
    if let Some(finish) = parsed["choices"][0]
        .get("finish_reason")
        .and_then(|f| f.as_str())
    {
        // Emit usage if present
        if let Some(usage) = parsed.get("usage") {
            events.push(StreamEvent::Usage(extract_usage(usage)));
        }
        let _ = finish; // finish_reason consumed
        events.push(StreamEvent::Done {});
    }

    events
}

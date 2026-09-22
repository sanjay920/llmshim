use super::types::{ChatRequest, ChatResponse, ResponseMessage, StreamEvent, Usage};
use serde_json::{json, Value};

/// Convert our ChatRequest into the OpenAI-format Value that lib.rs expects.
pub fn request_to_value(req: &ChatRequest) -> Value {
    let mut v = json!({
        "model": req.model,
        "messages": req.messages,
    });
    if let Some(cache) = &req.cache {
        v["x-cache"] = json!(cache);
    }

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
        if let Some(mode) = &cfg.reasoning_mode {
            v["reasoning_mode"] = json!(mode);
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

    if let Some(shim) = &req.shim {
        v["x-shim"] = json!(shim);
    }
    if let Some(format) = &req.response_format {
        v["response_format"] = format.clone();
    }
    v
}

/// Validate canonical history before HTTP/SSE admission. Providers additionally
/// validate their native projection after applying native overrides.
pub(crate) fn validate_request(req: &ChatRequest) -> crate::error::Result<()> {
    let request = request_to_value(req);
    if let Some(messages) = request["messages"].as_array() {
        crate::toolcall::validate_history(messages)?;
    }
    Ok(())
}

/// Release date as a Unix timestamp. The OpenAI list shape types `created` as
/// an integer, so an undated model reports `0` rather than a null.
fn released_at(id: &str) -> i64 {
    crate::catalog::resolve(id)
        .and_then(|info| info.release_date)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|at| at.and_utc().timestamp())
        .unwrap_or(0)
}

/// Build the shared `GET /v1/models` body: the OpenAI list envelope plus
/// llmshim's own array. Proxy and gateway serve the identical shape.
pub(crate) fn models_response(provider_keys: &[&str]) -> super::types::ModelsResponse {
    let available = crate::models::available_models(provider_keys);
    super::types::ModelsResponse {
        object: "list",
        data: available
            .iter()
            .map(|m| super::types::ModelObject {
                id: m.id.to_string(),
                object: "model",
                created: released_at(m.id),
                owned_by: m.provider.to_string(),
            })
            .collect(),
        models: available
            .into_iter()
            .map(|m| super::types::ModelEntry {
                id: m.id.to_string(),
                provider: m.provider.to_string(),
                name: m.name.to_string(),
            })
            .collect(),
    }
}

/// Convert the OpenAI-format Value response from lib.rs into our ChatResponse.
pub fn value_to_response(v: &Value, provider: &str, latency_ms: u64) -> ChatResponse {
    let choice = &v["choices"][0];
    let msg = &choice["message"];

    let content = msg.get("content").cloned().unwrap_or(Value::Null);
    let tool_calls = msg.get("tool_calls").cloned().filter(|v| !v.is_null());
    let reasoning_text = crate::reasoning::reasoning_text(msg);
    let reasoning = (!reasoning_text.is_empty()).then_some(reasoning_text);

    let usage = extract_usage(&v["usage"]);

    ChatResponse {
        finish_reason: choice["finish_reason"].as_str().map(str::to_owned),
        served_model: v["x-llmshim-served-model"].as_str().map(str::to_owned),
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
            refusal: msg["refusal"].as_str().map(str::to_owned),
            role: "assistant".to_string(),
            content,
            tool_calls,
            reasoning: msg.get("reasoning").cloned(),
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
        cache_read_tokens: usage["cache_read_tokens"].as_u64().unwrap_or(0),
        cache_write_tokens: usage["cache_write_tokens"].as_u64().unwrap_or(0),
        cost_usd: crate::cost::stamped(usage),
        cost_source: crate::cost::stamped_source(usage).map(str::to_owned),
    }
}

#[cfg(test)]
#[test]
fn cache_accounting_survives_proxy_projection() {
    let usage = extract_usage(
        &json!({"prompt_tokens": 10, "cache_read_tokens": 7, "cache_write_tokens": 3}),
    );
    let wire = serde_json::to_value(usage).unwrap();
    assert_eq!(wire["cache_read_tokens"], 7);
    assert_eq!(wire["cache_write_tokens"], 3);
    let empty = serde_json::to_value(extract_usage(&json!({}))).unwrap();
    assert_eq!(empty["cache_read_tokens"], 0);
    assert_eq!(empty["cache_write_tokens"], 0);
    let events =
        chunk_to_events(&json!({"choices":[],"usage":{"cache_read_tokens":9}}).to_string());
    assert!(matches!(
        &events[..],
        [StreamEvent::Usage(Usage {
            cache_read_tokens: 9,
            ..
        })]
    ));
}

#[cfg(test)]
#[test]
fn proxy_assistant_message_round_trips_reasoning_without_projection_loss() {
    use crate::provider::Provider;
    let provider = crate::providers::anthropic::Anthropic::new("test".into());
    let native = json!({"stop_reason":"end_turn","content":[{"type":"thinking","thinking":"thought","signature":"opaque+/="},{"type":"text","text":"answer"}]});
    let normalized = provider
        .transform_response("claude-sonnet-4-6", native)
        .unwrap();
    let response = value_to_response(&normalized, "anthropic", 0);
    let message = serde_json::to_value(response.message).unwrap();
    assert_eq!(
        message["reasoning"],
        normalized["choices"][0]["message"]["reasoning"]
    );
    let request: ChatRequest =
        serde_json::from_value(json!({"model":"claude-sonnet-4-6","messages":[message]})).unwrap();
    let outbound = provider
        .transform_request("claude-sonnet-4-6", &request_to_value(&request))
        .unwrap();
    assert_eq!(
        outbound.body["messages"][0]["content"][0]["signature"],
        "opaque+/="
    );
    let signature = json!({"data":"opaque","origin":{"provider":"gemini","model":"gemini-3.8-flash","family":"gemini","wire":"google-generate-content","received_at":"2026-09-16T00:00:00Z"}});
    let events=chunk_to_events(&json!({"choices":[{"delta":{"tool_calls":[{"id":"c1","function":{"name":"read","arguments":"{}"},"thought_signature":signature}]}}]}).to_string());
    assert_eq!(
        serde_json::to_value(&events[0]).unwrap()["thought_signature"],
        signature
    );
}

/// Parse a single OpenAI-format stream chunk into typed SSE events.
/// Returns one or more events per chunk.
pub fn chunk_to_events(chunk_json: &str) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    let parsed: Value = match serde_json::from_str(chunk_json) {
        Ok(v) => v,
        Err(_) => return events,
    };

    let choice = parsed["choices"]
        .as_array()
        .and_then(|choices| {
            choices
                .iter()
                .find(|c| c["index"].as_u64().unwrap_or(0) == 0)
        })
        .unwrap_or(&Value::Null);
    let delta = &choice["delta"];

    // Reasoning content
    if let Some(blocks) = delta.get("reasoning").and_then(Value::as_array) {
        if !blocks.is_empty() {
            events.push(StreamEvent::Reasoning {
                text: crate::reasoning::reasoning_text(delta),
                blocks: blocks.clone(),
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

    if let Some(text) = delta["refusal"].as_str().filter(|text| !text.is_empty()) {
        events.push(StreamEvent::Content { text: text.into() });
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
                    thought_signature: tc.get("thought_signature").cloned(),
                    wire_ids: tc.get("wire_ids").cloned(),
                });
            }
        }
    }

    // Usage-only chunks are valid (notably Chat Completions sends one after
    // its last content delta). Do not require a choice or a finish reason.
    if let Some(usage) = parsed.get("usage").filter(|v| v.is_object()) {
        events.push(StreamEvent::Usage(extract_usage(usage)));
    }

    // Finish reason → done event
    if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        events.push(StreamEvent::Done {
            finish_reason: Some(finish.into()),
            served_model: parsed["x-llmshim-served-model"].as_str().map(str::to_owned),
        });
    }

    events
}

#[cfg(test)]
#[test]
fn proxy_keeps_capability_options_and_refusal_projection() {
    let req: ChatRequest =
        serde_json::from_value(json!({"model":"local/test","messages":[],"x-shim":{"structured_output":"prompt"},"response_format":{"type":"json_schema","json_schema":{"schema":{"type":"integer"}}}})).unwrap();
    let normalized = request_to_value(&req);
    assert_eq!(normalized["x-shim"]["structured_output"], "prompt");
    assert_eq!(
        normalized["response_format"]["json_schema"]["schema"]["type"],
        "integer"
    );
    let mut r = json!({"choices":[{"message":{"role":"assistant","content":null}}]});
    r["choices"][0]["message"]["refusal"] = json!("Cannot comply");
    let projected = value_to_response(&r, "local", 0);
    assert_eq!(
        serde_json::to_value(projected).unwrap()["message"]["refusal"],
        "Cannot comply"
    );
}

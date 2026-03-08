use llmshim::provider::Provider;
use llmshim::providers::xai::Xai;
use serde_json::{json, Value};

fn provider() -> Xai {
    Xai::new("test-key-xai".into())
}

// ============================================================
// transform_request — basic
// ============================================================

#[test]
fn request_url() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert_eq!(result.url, "https://api.x.ai/v1/responses");
}

#[test]
fn request_custom_base_url() {
    let p = Xai::new("k".into()).with_base_url("http://localhost:9090".into());
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert_eq!(result.url, "http://localhost:9090/responses");
}

#[test]
fn request_sets_model() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert_eq!(result.body["model"], "grok-4-1-fast-reasoning");
}

#[test]
fn request_auth_header() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    let auth = result
        .headers
        .iter()
        .find(|(k, _)| k == "Authorization")
        .unwrap();
    assert_eq!(auth.1, "Bearer test-key-xai");
}

#[test]
fn request_messages_become_input() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
        ],
    });
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    let input = result.body["input"].as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["role"], "assistant");
}

// ============================================================
// transform_request — max_tokens
// ============================================================

#[test]
fn request_max_tokens_becomes_max_output_tokens() {
    let p = provider();
    let req =
        json!({"model": "x", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 500});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert_eq!(result.body["max_output_tokens"], 500);
}

#[test]
fn request_max_completion_tokens_becomes_max_output_tokens() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}], "max_completion_tokens": 256});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert_eq!(result.body["max_output_tokens"], 256);
}

// ============================================================
// transform_request — strips unsupported params
// ============================================================

#[test]
fn request_strips_reasoning_effort() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}], "reasoning_effort": "high"});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert!(
        result.body.get("reasoning_effort").is_none(),
        "reasoning_effort should be stripped for xAI"
    );
}

#[test]
fn request_strips_thinking() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}], "thinking": {"type": "adaptive"}});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert!(
        result.body.get("thinking").is_none(),
        "thinking should be stripped for xAI"
    );
}

#[test]
fn request_strips_output_config() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}], "output_config": {"effort": "high"}});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert!(
        result.body.get("output_config").is_none(),
        "output_config should be stripped for xAI"
    );
}

// ============================================================
// transform_request — system/developer → instructions
// ============================================================

#[test]
fn request_system_becomes_instructions() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "hi"},
        ],
    });
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert_eq!(result.body["instructions"], "You are helpful.");
    let input = result.body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "user");
}

#[test]
fn request_developer_becomes_instructions() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "developer", "content": "Be concise."},
            {"role": "user", "content": "hi"},
        ],
    });
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert_eq!(result.body["instructions"], "Be concise.");
}

#[test]
fn request_no_system_no_instructions() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert!(result.body.get("instructions").is_none());
}

// ============================================================
// transform_request — sanitization
// ============================================================

#[test]
fn request_strips_reasoning_content_from_messages() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "assistant", "content": "hello", "reasoning_content": "thinking...", "annotations": []},
            {"role": "user", "content": "hi"},
        ],
    });
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    let input = result.body["input"].as_array().unwrap();
    assert!(input[0].get("reasoning_content").is_none());
    assert!(input[0].get("annotations").is_none());
}

// ============================================================
// transform_request — tool_choice translation
// ============================================================

#[test]
fn tool_choice_anthropic_any_to_required() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}], "tool_choice": {"type": "any"}});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert_eq!(result.body["tool_choice"], "required");
}

#[test]
fn tool_choice_anthropic_tool_to_function() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}], "tool_choice": {"type": "tool", "name": "search"}});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert_eq!(result.body["tool_choice"]["type"], "function");
    assert_eq!(result.body["tool_choice"]["function"]["name"], "search");
}

#[test]
fn tool_choice_string_passthrough() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}], "tool_choice": "auto"});
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert_eq!(result.body["tool_choice"], "auto");
}

#[test]
fn request_rejects_non_object() {
    let p = provider();
    assert!(p.transform_request("x", &json!("string")).is_err());
}

// ============================================================
// transform_response
// ============================================================

#[test]
fn response_text_only() {
    let p = provider();
    let resp = json!({
        "id": "resp_xai",
        "status": "completed",
        "error": null,
        "output": [
            {"type": "message", "status": "completed", "content": [{"type": "output_text", "text": "Hello!"}], "role": "assistant"}
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    });
    let result = p
        .transform_response("grok-4-1-fast-reasoning", resp)
        .unwrap();
    assert_eq!(result["object"], "chat.completion");
    assert_eq!(result["id"], "resp_xai");
    assert_eq!(result["choices"][0]["message"]["content"], "Hello!");
    assert_eq!(result["choices"][0]["finish_reason"], "stop");
    assert_eq!(result["usage"]["prompt_tokens"], 10);
    assert_eq!(result["usage"]["completion_tokens"], 5);
}

#[test]
fn response_with_reasoning_tokens() {
    let p = provider();
    let resp = json!({
        "id": "resp_r",
        "status": "completed",
        "error": null,
        "output": [
            {"type": "reasoning", "summary": []},
            {"type": "message", "status": "completed", "content": [{"type": "output_text", "text": "42"}], "role": "assistant"}
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15, "output_tokens_details": {"reasoning_tokens": 100}},
    });
    let result = p
        .transform_response("grok-4-1-fast-reasoning", resp)
        .unwrap();
    assert_eq!(result["usage"]["reasoning_tokens"], 100);
}

#[test]
fn response_no_reasoning_tokens_field_when_zero() {
    let p = provider();
    let resp = json!({
        "id": "resp_nr",
        "status": "completed",
        "error": null,
        "output": [
            {"type": "message", "status": "completed", "content": [{"type": "output_text", "text": "hi"}], "role": "assistant"}
        ],
        "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7},
    });
    let result = p
        .transform_response("grok-4-1-fast-non-reasoning", resp)
        .unwrap();
    assert!(result["usage"].get("reasoning_tokens").is_none());
}

#[test]
fn response_function_call() {
    let p = provider();
    let resp = json!({
        "id": "resp_fc",
        "status": "completed",
        "error": null,
        "output": [
            {"type": "function_call", "call_id": "call_1", "name": "search", "arguments": "{\"q\":\"rust\"}"}
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    });
    let result = p
        .transform_response("grok-4-1-fast-reasoning", resp)
        .unwrap();
    let tc = &result["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tc["id"], "call_1");
    assert_eq!(tc["function"]["name"], "search");
    let args: Value = serde_json::from_str(tc["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["q"], "rust");
}

#[test]
fn response_incomplete_status() {
    let p = provider();
    let resp = json!({
        "id": "x", "status": "incomplete", "error": null,
        "output": [{"type": "message", "status": "completed", "content": [{"type": "output_text", "text": "partial"}], "role": "assistant"}],
        "usage": {},
    });
    let result = p.transform_response("x", resp).unwrap();
    assert_eq!(result["choices"][0]["finish_reason"], "length");
}

#[test]
fn response_error_null_is_success() {
    let p = provider();
    let resp = json!({
        "id": "x", "status": "completed", "error": null,
        "output": [{"type": "message", "status": "completed", "content": [{"type": "output_text", "text": "ok"}], "role": "assistant"}],
        "usage": {},
    });
    let result = p.transform_response("x", resp).unwrap();
    assert_eq!(result["choices"][0]["message"]["content"], "ok");
}

#[test]
fn response_error_non_null() {
    let p = provider();
    let resp = json!({"error": {"message": "bad request"}});
    assert!(p.transform_response("x", resp).is_err());
}

// ============================================================
// transform_stream_chunk
// ============================================================

#[test]
fn stream_output_text_delta() {
    let p = provider();
    let chunk = json!({"type": "response.output_text.delta", "delta": "Hello"});
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["choices"][0]["delta"]["content"], "Hello");
}

#[test]
fn stream_response_completed() {
    let p = provider();
    let chunk = json!({
        "type": "response.completed",
        "response": {
            "status": "completed",
            "usage": {"input_tokens": 10, "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 50}},
        },
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["choices"][0]["finish_reason"], "stop");
    assert_eq!(parsed["usage"]["prompt_tokens"], 10);
    assert_eq!(parsed["usage"]["reasoning_tokens"], 50);
}

#[test]
fn stream_other_events_skipped() {
    let p = provider();
    for event_type in &[
        "response.created",
        "response.in_progress",
        "response.output_item.added",
    ] {
        let chunk = json!({"type": event_type});
        let result = p
            .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
            .unwrap();
        assert!(result.is_none(), "{} should be skipped", event_type);
    }
}

#[test]
fn stream_empty_returns_none() {
    let p = provider();
    assert!(p.transform_stream_chunk("x", "").unwrap().is_none());
    assert!(p.transform_stream_chunk("x", "  ").unwrap().is_none());
}

#[test]
fn stream_done_returns_none() {
    let p = provider();
    assert!(p.transform_stream_chunk("x", "[DONE]").unwrap().is_none());
}

use llmshim::provider::Provider;
use llmshim::providers::openrouter::OpenRouter;
use serde_json::{json, Value};

fn provider() -> OpenRouter {
    OpenRouter::new("test-key-abc".into())
}

// ============================================================
// transform_request
// ============================================================

#[test]
fn request_url_and_bearer_auth() {
    let p = provider();
    let req = json!({
        "model": "anthropic/claude-sonnet-4.5",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p
        .transform_request("anthropic/claude-sonnet-4.5", &req)
        .unwrap();
    assert_eq!(result.url, "https://openrouter.ai/api/v1/chat/completions");
    let auth = result.headers.iter().find(|(k, _)| k == "Authorization");
    assert_eq!(auth.unwrap().1, "Bearer test-key-abc");
}

#[test]
fn request_preserves_slug_and_messages() {
    // The vendor/model slug (with its internal slash) is passed straight through.
    let p = provider();
    let req = json!({
        "model": "meta-llama/llama-3.1-70b-instruct",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p
        .transform_request("meta-llama/llama-3.1-70b-instruct", &req)
        .unwrap();
    assert_eq!(result.body["model"], "meta-llama/llama-3.1-70b-instruct");
    assert_eq!(result.body["messages"][0]["content"], "hi");
}

#[test]
fn request_forwards_tools_unchanged() {
    // OpenRouter is Chat Completions-native, so nested tools need no translation.
    let p = provider();
    let tools = json!([{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
        }
    }]);
    let req = json!({
        "model": "openai/gpt-5.1",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": tools,
        "tool_choice": "auto",
        "response_format": {"type": "json_object"},
    });
    let result = p.transform_request("openai/gpt-5.1", &req).unwrap();
    assert_eq!(result.body["tools"], tools);
    assert_eq!(result.body["tool_choice"], "auto");
    assert_eq!(result.body["response_format"]["type"], "json_object");
}

#[test]
fn request_maps_reasoning_effort_1to1() {
    let p = provider();
    for effort in ["low", "medium", "high", "xhigh", "max", "none"] {
        let req = json!({
            "model": "x/y",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": effort,
        });
        let result = p.transform_request("x/y", &req).unwrap();
        assert_eq!(
            result.body["reasoning"]["effort"], effort,
            "effort {effort} should map 1:1 (OpenRouter vocab is a superset)"
        );
    }
}

#[test]
fn request_reasoning_mode_pro_bumps_one_tier() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high",
        "reasoning_mode": "pro",
    });
    let result = p.transform_request("x/y", &req).unwrap();
    assert_eq!(result.body["reasoning"]["effort"], "xhigh");
}

#[test]
fn request_native_reasoning_wins_over_effort() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high",
        "x-openrouter": {"reasoning": {"max_tokens": 2000}},
    });
    let result = p.transform_request("x/y", &req).unwrap();
    assert_eq!(result.body["reasoning"]["max_tokens"], 2000);
    // The unified effort mapping is bypassed when native reasoning is supplied.
    assert!(result.body["reasoning"].get("effort").is_none());
}

#[test]
fn request_disables_middle_out_by_default() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p.transform_request("x/y", &req).unwrap();
    assert_eq!(result.body["transforms"], json!([]));
}

#[test]
fn request_transforms_opt_in_preserved() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
        "x-openrouter": {"transforms": ["middle-out"]},
    });
    let result = p.transform_request("x/y", &req).unwrap();
    assert_eq!(result.body["transforms"], json!(["middle-out"]));
}

#[test]
fn request_x_openrouter_body_passthrough() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
        "x-openrouter": {
            "provider": {"order": ["anthropic", "openai"], "allow_fallbacks": false},
            "models": ["anthropic/claude-sonnet-4.5", "openai/gpt-5.1"],
            "route": "fallback"
        },
    });
    let result = p.transform_request("x/y", &req).unwrap();
    assert_eq!(result.body["provider"]["order"][0], "anthropic");
    assert_eq!(result.body["provider"]["allow_fallbacks"], false);
    assert_eq!(result.body["models"][1], "openai/gpt-5.1");
    assert_eq!(result.body["route"], "fallback");
}

#[test]
fn request_x_openrouter_attribution_headers_not_in_body() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
        "x-openrouter": {"http_referer": "https://example.com", "x_title": "MyApp"},
    });
    let result = p.transform_request("x/y", &req).unwrap();
    let referer = result.headers.iter().find(|(k, _)| k == "HTTP-Referer");
    let title = result.headers.iter().find(|(k, _)| k == "X-Title");
    assert_eq!(referer.unwrap().1, "https://example.com");
    assert_eq!(title.unwrap().1, "MyApp");
    // Attribution controls must not leak into the request body.
    assert!(result.body.get("http_referer").is_none());
    assert!(result.body.get("x_title").is_none());
}

#[test]
fn request_sanitizes_foreign_reasoning_fields() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{
            "role": "assistant",
            "content": "hi",
            "reasoning_content": "x",
            "reasoning_signature": "anthropic-sig-should-not-leak",
            "redacted_reasoning_content": "redacted-should-not-leak"
        }]
    });
    let result = p.transform_request("x/y", &req).unwrap();
    let body = serde_json::to_string(&result.body).unwrap();
    assert!(!body.contains("anthropic-sig-should-not-leak"));
    assert!(!body.contains("redacted-should-not-leak"));
    assert!(!body.contains("reasoning_signature"));
}

#[test]
fn request_preserves_vision_and_tool_messages() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]},
            {"role": "assistant", "content": "", "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "f", "arguments": "{}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "result"}
        ]
    });
    let result = p.transform_request("x/y", &req).unwrap();
    let msgs = result.body["messages"].as_array().unwrap();
    // Image block preserved in OpenAI form.
    assert_eq!(msgs[0]["content"][1]["type"], "image_url");
    // tool_calls stay in Chat Completions shape (no function_call splitting).
    assert_eq!(msgs[1]["tool_calls"][0]["id"], "call_1");
    // role:"tool" stays as-is.
    assert_eq!(msgs[2]["role"], "tool");
    assert_eq!(msgs[2]["tool_call_id"], "call_1");
}

// ============================================================
// transform_response
// ============================================================

#[test]
fn response_normalizes_reasoning_to_reasoning_content() {
    let p = provider();
    let resp = json!({
        "id": "gen_1",
        "model": "anthropic/claude-sonnet-4.5",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "42", "reasoning": "let me think..."},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
    });
    let result = p
        .transform_response("anthropic/claude-sonnet-4.5", resp)
        .unwrap();
    let msg = &result["choices"][0]["message"];
    assert_eq!(msg["reasoning_content"], "let me think...");
    assert_eq!(msg["content"], "42");
    assert_eq!(result["usage"]["total_tokens"], 7);
}

#[test]
fn response_passes_tool_calls_through() {
    let p = provider();
    let resp = json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_9", "type": "function",
                "function": {"name": "search", "arguments": "{\"q\":\"x\"}"}
            }]},
            "finish_reason": "tool_calls"
        }]
    });
    let result = p.transform_response("x/y", resp).unwrap();
    let tc = &result["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tc["id"], "call_9");
    assert_eq!(tc["function"]["name"], "search");
    assert_eq!(result["choices"][0]["finish_reason"], "tool_calls");
}

#[test]
fn response_error_object_becomes_error() {
    let p = provider();
    let resp = json!({"error": {"code": 402, "message": "Insufficient credits"}});
    let result = p.transform_response("x/y", resp);
    assert!(result.is_err());
}

// ============================================================
// transform_stream_chunk
// ============================================================

#[test]
fn stream_normalizes_reasoning_delta() {
    let p = provider();
    let chunk = json!({
        "choices": [{"index": 0, "delta": {"reasoning": "thinking"}, "finish_reason": null}]
    });
    let result = p
        .transform_stream_chunk("x/y", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        parsed["choices"][0]["delta"]["reasoning_content"],
        "thinking"
    );
}

#[test]
fn stream_passes_content_delta() {
    let p = provider();
    let chunk = json!({
        "choices": [{"index": 0, "delta": {"content": "Hello"}, "finish_reason": null}]
    });
    let result = p
        .transform_stream_chunk("x/y", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["choices"][0]["delta"]["content"], "Hello");
}

#[test]
fn stream_skips_unparseable() {
    let p = provider();
    let result = p.transform_stream_chunk("x/y", "not json").unwrap();
    assert!(result.is_none());
}

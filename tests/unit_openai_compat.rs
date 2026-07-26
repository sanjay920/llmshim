use llmshim::provider::Provider;
use llmshim::providers::openai_compat::OpenAiCompatible;
use serde_json::{json, Value};

fn vllm() -> OpenAiCompatible {
    OpenAiCompatible::new("vllm", "http://localhost:8000/v1", None)
}

fn sglang_with_key() -> OpenAiCompatible {
    OpenAiCompatible::new(
        "sglang",
        "http://localhost:30000/v1",
        Some("secret".to_string()),
    )
}

// ============================================================
// transform_request
// ============================================================

#[test]
fn request_url_from_base_and_no_auth_when_keyless() {
    let p = vllm();
    let req = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("m", &req).unwrap();
    assert_eq!(result.url, "http://localhost:8000/v1/chat/completions");
    // Self-hosted with no key: no Authorization header.
    assert!(result.headers.iter().all(|(k, _)| k != "Authorization"));
}

#[test]
fn request_sends_bearer_when_key_set() {
    let p = sglang_with_key();
    let req = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("m", &req).unwrap();
    let auth = result.headers.iter().find(|(k, _)| k == "Authorization");
    assert_eq!(auth.unwrap().1, "Bearer secret");
}

#[test]
fn request_trailing_slash_base_url_is_normalized() {
    let p = OpenAiCompatible::new("vllm", "http://localhost:8000/v1/", None);
    let req = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("m", &req).unwrap();
    assert_eq!(result.url, "http://localhost:8000/v1/chat/completions");
}

#[test]
fn request_preserves_hf_slug_model() {
    // The served model name (an HF path with a slash) passes through as `model`.
    let p = vllm();
    let req = json!({"model": "meta-llama/Llama-3.1-8B-Instruct", "messages": [{"role": "user", "content": "hi"}]});
    let result = p
        .transform_request("meta-llama/Llama-3.1-8B-Instruct", &req)
        .unwrap();
    assert_eq!(result.body["model"], "meta-llama/Llama-3.1-8B-Instruct");
}

#[test]
fn request_forwards_standard_params_and_reasoning_effort() {
    let p = vllm();
    let tools = json!([{"type": "function", "function": {"name": "f", "parameters": {}}}]);
    let req = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": tools,
        "response_format": {"type": "json_object"},
        "reasoning_effort": "high",
        "temperature": 0.3,
    });
    let result = p.transform_request("m", &req).unwrap();
    assert_eq!(result.body["tools"], tools);
    assert_eq!(result.body["response_format"]["type"], "json_object");
    assert_eq!(result.body["reasoning_effort"], "high");
    assert_eq!(result.body["temperature"], 0.3);
}

#[test]
fn request_x_namespace_params_copied_to_body() {
    let p = vllm();
    let req = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "x-vllm": {
            "top_k": 20,
            "guided_json": {"type": "object"},
            "chat_template_kwargs": {"enable_thinking": true}
        },
    });
    let result = p.transform_request("m", &req).unwrap();
    assert_eq!(result.body["top_k"], 20);
    assert_eq!(result.body["guided_json"]["type"], "object");
    assert_eq!(result.body["chat_template_kwargs"]["enable_thinking"], true);
}

#[test]
fn request_namespace_is_provider_specific() {
    // An x-sglang block is ignored by the vllm provider (wrong namespace).
    let p = vllm();
    let req = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "x-sglang": {"separate_reasoning": false},
    });
    let result = p.transform_request("m", &req).unwrap();
    assert!(result.body.get("separate_reasoning").is_none());
}

#[test]
fn request_sanitizes_foreign_reasoning_fields() {
    let p = vllm();
    let req = json!({
        "model": "m",
        "messages": [{
            "role": "assistant", "content": "hi",
            "reasoning_signature": "anthropic-sig-should-not-leak",
            "redacted_reasoning_content": "redacted-should-not-leak"
        }]
    });
    let result = p.transform_request("m", &req).unwrap();
    let body = serde_json::to_string(&result.body).unwrap();
    assert!(!body.contains("anthropic-sig-should-not-leak"));
    assert!(!body.contains("redacted-should-not-leak"));
}

#[test]
fn request_preserves_image_url_vision() {
    let p = vllm();
    let req = json!({
        "model": "m",
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]}]
    });
    let result = p.transform_request("m", &req).unwrap();
    let content = &result.body["messages"][0]["content"];
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
}

// ============================================================
// transform_response / stream
// ============================================================

#[test]
fn response_normalizes_reasoning_field_to_reasoning_content() {
    // vLLM's newer `reasoning` field -> llmshim's reasoning_content.
    let p = vllm();
    let resp = json!({
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "42", "reasoning": "think"}, "finish_reason": "stop"}]
    });
    let result = p.transform_response("m", resp).unwrap();
    assert_eq!(
        result["choices"][0]["message"]["reasoning_content"],
        "think"
    );
}

#[test]
fn response_keeps_native_reasoning_content() {
    // SGLang already returns reasoning_content — leave it untouched.
    let p = sglang_with_key();
    let resp = json!({
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "42", "reasoning_content": "native"}, "finish_reason": "stop"}]
    });
    let result = p.transform_response("m", resp).unwrap();
    assert_eq!(
        result["choices"][0]["message"]["reasoning_content"],
        "native"
    );
}

#[test]
fn response_passes_tool_calls_and_errors() {
    let p = vllm();
    let resp = json!({
        "choices": [{"index": 0, "message": {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}]}, "finish_reason": "tool_calls"}]
    });
    let result = p.transform_response("m", resp).unwrap();
    assert_eq!(result["choices"][0]["message"]["tool_calls"][0]["id"], "c1");

    let err_resp = json!({"error": {"code": 404, "message": "model not found"}});
    assert!(p.transform_response("m", err_resp).is_err());
}

#[test]
fn stream_normalizes_reasoning_delta() {
    let p = vllm();
    let chunk =
        json!({"choices": [{"index": 0, "delta": {"reasoning": "th"}, "finish_reason": null}]});
    let out = p
        .transform_stream_chunk("m", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["choices"][0]["delta"]["reasoning_content"], "th");
}

#[test]
fn name_reflects_configured_provider() {
    assert_eq!(vllm().name(), "vllm");
    assert_eq!(sglang_with_key().name(), "sglang");
}

use llmshim::provider::Provider;
use llmshim::providers::anthropic::Anthropic;
use serde_json::{json, Value};

fn provider() -> Anthropic {
    Anthropic::new("test-key-abc".into())
}

// ============================================================
// transform_request — basic
// ============================================================

#[test]
fn request_sets_model_and_url() {
    let p = provider();
    let req = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    assert_eq!(result.body["model"], "claude-sonnet-4-6");
    assert_eq!(result.url, "https://api.anthropic.com/v1/messages");
}

#[test]
fn request_custom_base_url() {
    let p = Anthropic::new("k".into()).with_base_url("http://localhost:9090".into());
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("x", &req).unwrap();
    assert_eq!(result.url, "http://localhost:9090/messages");
}

#[test]
fn request_headers() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("x", &req).unwrap();
    let api_key = result
        .headers
        .iter()
        .find(|(k, _)| k == "x-api-key")
        .unwrap();
    assert_eq!(api_key.1, "test-key-abc");
    let version = result
        .headers
        .iter()
        .find(|(k, _)| k == "anthropic-version")
        .unwrap();
    assert_eq!(version.1, "2023-06-01");
}

// ============================================================
// transform_request — max_tokens handling
// ============================================================

#[test]
fn request_default_max_tokens() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p.transform_request("x", &req).unwrap();
    assert_eq!(result.body["max_tokens"], 8192);
}

#[test]
fn request_explicit_max_tokens() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 256,
    });
    let result = p.transform_request("x", &req).unwrap();
    assert_eq!(result.body["max_tokens"], 256);
}

#[test]
fn request_max_completion_tokens_fallback() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "max_completion_tokens": 512,
    });
    let result = p.transform_request("x", &req).unwrap();
    assert_eq!(result.body["max_tokens"], 512);
}

// ============================================================
// transform_request — system message extraction
// ============================================================

#[test]
fn request_extracts_system_message() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "hi"},
        ],
    });
    let result = p.transform_request("x", &req).unwrap();
    assert_eq!(result.body["system"], "You are helpful.");
    let messages = result.body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
}

#[test]
fn request_extracts_developer_role_as_system() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "developer", "content": "Be concise."},
            {"role": "user", "content": "hi"},
        ],
    });
    let result = p.transform_request("x", &req).unwrap();
    assert_eq!(result.body["system"], "Be concise.");
}

#[test]
fn request_merges_multiple_system_messages() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "system", "content": "Part one."},
            {"role": "system", "content": "Part two."},
            {"role": "user", "content": "hi"},
        ],
    });
    let result = p.transform_request("x", &req).unwrap();
    assert_eq!(result.body["system"], "Part one.\n\nPart two.");
}

#[test]
fn request_no_system_message() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p.transform_request("x", &req).unwrap();
    assert!(result.body.get("system").is_none());
}

// ============================================================
// transform_request — standard params
// ============================================================

#[test]
fn request_passes_standard_params() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.5,
        "top_p": 0.9,
        "top_k": 40,
        "stop": ["END"],
        "stream": true,
    });
    let result = p.transform_request("x", &req).unwrap();
    assert_eq!(result.body["temperature"], 0.5);
    assert_eq!(result.body["top_p"], 0.9);
    assert_eq!(result.body["top_k"], 40);
    assert_eq!(result.body["stop"], json!(["END"]));
    assert_eq!(result.body["stream"], true);
}

#[test]
fn request_ignores_openai_only_params() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "frequency_penalty": 0.5,
        "presence_penalty": 0.3,
        "logprobs": true,
        "n": 2,
    });
    let result = p.transform_request("x", &req).unwrap();
    // These should NOT appear in the Anthropic request body
    assert!(result.body.get("frequency_penalty").is_none());
    assert!(result.body.get("presence_penalty").is_none());
    assert!(result.body.get("logprobs").is_none());
    assert!(result.body.get("n").is_none());
}

// ============================================================
// transform_request — tool transformations
// ============================================================

#[test]
fn request_transforms_tools() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "weather?"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get current weather",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "city": {"type": "string"}
                    },
                    "required": ["city"]
                }
            }
        }]
    });
    let result = p.transform_request("x", &req).unwrap();
    let tools = result.body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "get_weather");
    assert_eq!(tools[0]["description"], "Get current weather");
    assert!(tools[0]["input_schema"].is_object());
    assert_eq!(
        tools[0]["input_schema"]["properties"]["city"]["type"],
        "string"
    );
}

#[test]
fn request_transforms_tool_calls_in_messages() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "weather in paris?"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_123",
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"city\":\"paris\"}"
                    }
                }]
            },
            {
                "role": "tool",
                "tool_call_id": "call_123",
                "content": "Sunny, 22C"
            },
            {"role": "user", "content": "thanks!"}
        ],
    });
    let result = p.transform_request("x", &req).unwrap();
    let messages = result.body["messages"].as_array().unwrap();

    // Message 0: user (unchanged)
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "weather in paris?");

    // Message 1: assistant with tool_use content blocks (not tool_calls)
    assert_eq!(messages[1]["role"], "assistant");
    assert!(messages[1].get("tool_calls").is_none());
    let content = messages[1]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "tool_use");
    assert_eq!(content[0]["id"], "call_123");
    assert_eq!(content[0]["name"], "get_weather");
    assert_eq!(content[0]["input"]["city"], "paris");

    // Message 2: tool result (role becomes "user")
    assert_eq!(messages[2]["role"], "user");
    let tool_content = messages[2]["content"].as_array().unwrap();
    assert_eq!(tool_content[0]["type"], "tool_result");
    assert_eq!(tool_content[0]["tool_use_id"], "call_123");
}

#[test]
fn request_transforms_assistant_with_text_and_tool_calls() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{
            "role": "assistant",
            "content": "Let me check that for you.",
            "tool_calls": [{
                "id": "call_456",
                "type": "function",
                "function": {
                    "name": "search",
                    "arguments": "{\"q\":\"rust\"}"
                }
            }]
        }],
    });
    let result = p.transform_request("x", &req).unwrap();
    let messages = result.body["messages"].as_array().unwrap();
    let content = messages[0]["content"].as_array().unwrap();
    // Should have text block + tool_use block
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "Let me check that for you.");
    assert_eq!(content[1]["type"], "tool_use");
}

#[test]
fn request_transforms_function_role_to_user() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "function", "content": "result data", "name": "my_func"},
        ],
    });
    let result = p.transform_request("x", &req).unwrap();
    let messages = result.body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
}

// ============================================================
// transform_request — x-anthropic extensions
// ============================================================

#[test]
fn request_applies_x_anthropic_extensions() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "x-anthropic": {
            "metadata": {"user_id": "user-123"},
        }
    });
    let result = p.transform_request("x", &req).unwrap();
    assert_eq!(result.body["metadata"]["user_id"], "user-123");
}

// ============================================================
// transform_request — error handling
// ============================================================

#[test]
fn request_rejects_non_object() {
    let p = provider();
    assert!(p.transform_request("x", &json!("string")).is_err());
}

#[test]
fn request_rejects_missing_messages() {
    let p = provider();
    let req = json!({"model": "x"});
    assert!(p.transform_request("x", &req).is_err());
}

// ============================================================
// transform_response — text responses
// ============================================================

#[test]
fn response_text_only() {
    let p = provider();
    let resp = json!({
        "id": "msg_123",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "Hello!"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let result = p.transform_response("claude-sonnet-4-6", resp).unwrap();

    assert_eq!(result["id"], "msg_123");
    assert_eq!(result["object"], "chat.completion");
    assert_eq!(result["model"], "claude-sonnet-4-6");
    assert_eq!(result["choices"][0]["message"]["role"], "assistant");
    assert_eq!(result["choices"][0]["message"]["content"], "Hello!");
    assert_eq!(result["choices"][0]["finish_reason"], "stop");
    assert_eq!(result["usage"]["prompt_tokens"], 10);
    assert_eq!(result["usage"]["completion_tokens"], 5);
    assert_eq!(result["usage"]["total_tokens"], 15);
}

#[test]
fn response_multiple_text_blocks() {
    let p = provider();
    let resp = json!({
        "id": "msg_456",
        "content": [
            {"type": "text", "text": "Hello "},
            {"type": "text", "text": "World!"},
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3}
    });
    let result = p.transform_response("x", resp).unwrap();
    assert_eq!(result["choices"][0]["message"]["content"], "Hello World!");
}

// ============================================================
// transform_response — stop reasons
// ============================================================

#[test]
fn response_stop_reason_end_turn() {
    let p = provider();
    let resp = json!({"content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn", "usage": {}});
    let result = p.transform_response("x", resp).unwrap();
    assert_eq!(result["choices"][0]["finish_reason"], "stop");
}

#[test]
fn response_stop_reason_max_tokens() {
    let p = provider();
    let resp = json!({"content": [{"type": "text", "text": "ok"}], "stop_reason": "max_tokens", "usage": {}});
    let result = p.transform_response("x", resp).unwrap();
    assert_eq!(result["choices"][0]["finish_reason"], "length");
}

#[test]
fn response_stop_reason_tool_use() {
    let p = provider();
    let resp = json!({"content": [{"type": "text", "text": "ok"}], "stop_reason": "tool_use", "usage": {}});
    let result = p.transform_response("x", resp).unwrap();
    assert_eq!(result["choices"][0]["finish_reason"], "tool_calls");
}

// ============================================================
// transform_response — tool use responses
// ============================================================

#[test]
fn response_tool_use() {
    let p = provider();
    let resp = json!({
        "id": "msg_789",
        "content": [
            {"type": "text", "text": "Let me check."},
            {
                "type": "tool_use",
                "id": "tu_123",
                "name": "get_weather",
                "input": {"city": "paris"}
            }
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 20, "output_tokens": 15}
    });
    let result = p.transform_response("x", resp).unwrap();
    let msg = &result["choices"][0]["message"];
    assert_eq!(msg["content"], "Let me check.");
    let tool_calls = msg["tool_calls"].as_array().unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["id"], "tu_123");
    assert_eq!(tool_calls[0]["type"], "function");
    assert_eq!(tool_calls[0]["function"]["name"], "get_weather");

    // Arguments should be a JSON string
    let args: Value =
        serde_json::from_str(tool_calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "paris");
}

#[test]
fn response_tool_use_only_no_text() {
    let p = provider();
    let resp = json!({
        "id": "msg_x",
        "content": [{
            "type": "tool_use",
            "id": "tu_1",
            "name": "search",
            "input": {"q": "rust"}
        }],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 5, "output_tokens": 5}
    });
    let result = p.transform_response("x", resp).unwrap();
    // content should be null when no text blocks
    assert!(result["choices"][0]["message"]["content"].is_null());
    assert_eq!(
        result["choices"][0]["message"]["tool_calls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

// ============================================================
// transform_response — error handling
// ============================================================

#[test]
fn response_api_error() {
    let p = provider();
    let resp = json!({
        "type": "error",
        "error": {"type": "not_found_error", "message": "model not found"}
    });
    let err = p.transform_response("x", resp).unwrap_err();
    let msg = format!("{}", err);
    assert!(msg.contains("model not found"));
}

#[test]
fn response_empty_content() {
    let p = provider();
    let resp = json!({"content": [], "stop_reason": "end_turn", "usage": {}});
    let result = p.transform_response("x", resp).unwrap();
    assert!(result["choices"][0]["message"]["content"].is_null());
}

// ============================================================
// transform_stream_chunk
// ============================================================

#[test]
fn stream_message_start() {
    let p = provider();
    let chunk = json!({
        "type": "message_start",
        "message": {"id": "msg_stream_1", "role": "assistant"}
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["id"], "msg_stream_1");
    assert_eq!(parsed["object"], "chat.completion.chunk");
    assert_eq!(parsed["choices"][0]["delta"]["role"], "assistant");
}

#[test]
fn stream_text_delta() {
    let p = provider();
    let chunk = json!({
        "type": "content_block_delta",
        "delta": {"type": "text_delta", "text": "Hello"}
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["choices"][0]["delta"]["content"], "Hello");
    assert!(parsed["choices"][0]["finish_reason"].is_null());
}

#[test]
fn stream_tool_use_start() {
    let p = provider();
    let chunk = json!({
        "type": "content_block_start",
        "content_block": {"type": "tool_use", "id": "tu_s1", "name": "get_weather"}
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    let tc = &parsed["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc["id"], "tu_s1");
    assert_eq!(tc["function"]["name"], "get_weather");
}

#[test]
fn stream_tool_json_delta() {
    let p = provider();
    let chunk = json!({
        "type": "content_block_delta",
        "delta": {"type": "input_json_delta", "partial_json": "{\"city\":"}
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        parsed["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{\"city\":"
    );
}

#[test]
fn stream_message_delta_stop() {
    let p = provider();
    let chunk = json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn"},
        "usage": {"input_tokens": 10, "output_tokens": 20}
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["choices"][0]["finish_reason"], "stop");
    assert_eq!(parsed["usage"]["completion_tokens"], 20);
}

#[test]
fn stream_message_delta_tool_use_stop() {
    let p = provider();
    let chunk = json!({
        "type": "message_delta",
        "delta": {"stop_reason": "tool_use"},
        "usage": {"input_tokens": 5, "output_tokens": 10}
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["choices"][0]["finish_reason"], "tool_calls");
}

#[test]
fn stream_ping_skipped() {
    let p = provider();
    let chunk = json!({"type": "ping"});
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn stream_message_stop_skipped() {
    let p = provider();
    let chunk = json!({"type": "message_stop"});
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn stream_content_block_start_text_skipped() {
    let p = provider();
    let chunk = json!({
        "type": "content_block_start",
        "content_block": {"type": "text", "text": ""}
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn stream_empty_input() {
    let p = provider();
    assert!(p.transform_stream_chunk("x", "").unwrap().is_none());
    assert!(p.transform_stream_chunk("x", "  ").unwrap().is_none());
}

#[test]
fn stream_invalid_json_errors() {
    let p = provider();
    assert!(p.transform_stream_chunk("x", "not json").is_err());
}

#[test]
fn stream_unknown_event_type_skipped() {
    let p = provider();
    let chunk = json!({"type": "content_block_stop"});
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap();
    assert!(result.is_none());
}

// ============================================================
// transform_request — reasoning_effort translation
// ============================================================

#[test]
fn reasoning_effort_high_on_claude_4_6_uses_adaptive() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high",
    });
    let result = p
        .transform_request("claude-sonnet-4-6-20250514", &req)
        .unwrap();
    assert_eq!(result.body["thinking"]["type"], "adaptive");
    assert_eq!(result.body["output_config"]["effort"], "high");
    // effort should NOT be at top level
    assert!(result.body.get("effort").is_none());
}

#[test]
fn reasoning_effort_low_on_claude_4_6() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "low",
    });
    let result = p
        .transform_request("claude-sonnet-4-6-20250514", &req)
        .unwrap();
    assert_eq!(result.body["thinking"]["type"], "adaptive");
    assert_eq!(result.body["output_config"]["effort"], "low");
}

#[test]
fn reasoning_effort_medium_on_claude_4_6() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "medium",
    });
    let result = p
        .transform_request("claude-sonnet-4-6-20250514", &req)
        .unwrap();
    assert_eq!(result.body["thinking"]["type"], "adaptive");
    assert_eq!(result.body["output_config"]["effort"], "medium");
}

#[test]
fn reasoning_effort_high_on_pre_4_6_uses_enabled_with_budget() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 4096,
        "reasoning_effort": "high",
    });
    let result = p
        .transform_request("claude-sonnet-4-20250514", &req)
        .unwrap();
    assert_eq!(result.body["thinking"]["type"], "enabled");
    let budget = result.body["thinking"]["budget_tokens"].as_u64().unwrap();
    assert_eq!(budget, 4095); // max_tokens - 1
}

#[test]
fn reasoning_effort_low_on_pre_4_6_uses_small_budget() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 8192,
        "reasoning_effort": "low",
    });
    let result = p
        .transform_request("claude-sonnet-4-20250514", &req)
        .unwrap();
    assert_eq!(result.body["thinking"]["type"], "enabled");
    let budget = result.body["thinking"]["budget_tokens"].as_u64().unwrap();
    // low = max(1024, max_tokens/4) = max(1024, 2048) = 2048
    assert_eq!(budget, 2048);
}

#[test]
fn reasoning_effort_enforces_min_budget() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 2048,
        "reasoning_effort": "low",
    });
    let result = p
        .transform_request("claude-sonnet-4-20250514", &req)
        .unwrap();
    let budget = result.body["thinking"]["budget_tokens"].as_u64().unwrap();
    assert!(
        budget >= 1024,
        "Budget must be at least 1024, got {}",
        budget
    );
}

#[test]
fn reasoning_effort_strips_temperature() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.5,
        "reasoning_effort": "high",
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    // Thinking requires temperature=1, so it must be removed
    assert!(result.body.get("temperature").is_none());
}

#[test]
fn reasoning_effort_strips_top_k() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "top_k": 40,
        "reasoning_effort": "medium",
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    assert!(result.body.get("top_k").is_none());
}

#[test]
fn reasoning_effort_ignored_on_non_thinking_model() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high",
    });
    // claude-3-5-sonnet doesn't support thinking
    let result = p
        .transform_request("claude-3-5-sonnet-20241022", &req)
        .unwrap();
    assert!(result.body.get("thinking").is_none());
}

// ============================================================
// transform_request — direct thinking parameter
// ============================================================

#[test]
fn direct_thinking_enabled_passthrough() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "enabled", "budget_tokens": 5000},
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    assert_eq!(result.body["thinking"]["type"], "enabled");
    assert_eq!(result.body["thinking"]["budget_tokens"], 5000);
    // Temperature should be stripped when thinking is enabled
    assert!(result.body.get("temperature").is_none());
}

#[test]
fn direct_thinking_via_x_anthropic() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "x-anthropic": {
            "thinking": {"type": "enabled", "budget_tokens": 3000}
        },
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    assert_eq!(result.body["thinking"]["type"], "enabled");
    assert_eq!(result.body["thinking"]["budget_tokens"], 3000);
}

#[test]
fn direct_thinking_disabled_keeps_temperature() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "disabled"},
        "temperature": 0.5,
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    assert_eq!(result.body["thinking"]["type"], "disabled");
    assert_eq!(result.body["temperature"], 0.5);
}

#[test]
fn direct_output_config_effort_passthrough() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "low"},
    });
    let result = p
        .transform_request("claude-opus-4-6-20250514", &req)
        .unwrap();
    assert_eq!(result.body["thinking"]["type"], "adaptive");
    assert_eq!(result.body["output_config"]["effort"], "low");
}

// ============================================================
// transform_request — tool_choice translation
// ============================================================

#[test]
fn tool_choice_openai_string_auto_to_anthropic() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": "auto",
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    assert_eq!(result.body["tool_choice"]["type"], "auto");
}

#[test]
fn tool_choice_openai_string_required_to_anthropic_any() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": "required",
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    assert_eq!(result.body["tool_choice"]["type"], "any");
}

#[test]
fn tool_choice_openai_string_none_to_anthropic() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": "none",
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    assert_eq!(result.body["tool_choice"]["type"], "none");
}

#[test]
fn tool_choice_openai_function_object_to_anthropic_tool() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    assert_eq!(result.body["tool_choice"]["type"], "tool");
    assert_eq!(result.body["tool_choice"]["name"], "get_weather");
}

#[test]
fn tool_choice_anthropic_native_passthrough() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": {"type": "any"},
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    assert_eq!(result.body["tool_choice"]["type"], "any");
}

// ============================================================
// transform_response — thinking blocks
// ============================================================

#[test]
fn response_with_thinking_block() {
    let p = provider();
    let resp = json!({
        "id": "msg_think_1",
        "content": [
            {
                "type": "thinking",
                "thinking": "Let me reason step by step...\n1. First...\n2. Then...",
                "signature": "abc123signature"
            },
            {"type": "text", "text": "The answer is 42."}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 50, "output_tokens": 100}
    });
    let result = p.transform_response("claude-sonnet-4-6", resp).unwrap();
    let msg = &result["choices"][0]["message"];
    assert_eq!(msg["content"], "The answer is 42.");
    assert_eq!(
        msg["reasoning_content"],
        "Let me reason step by step...\n1. First...\n2. Then..."
    );
    assert_eq!(msg["role"], "assistant");
}

#[test]
fn response_without_thinking_has_no_reasoning_content() {
    let p = provider();
    let resp = json!({
        "id": "msg_nothink",
        "content": [{"type": "text", "text": "Hello!"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 2}
    });
    let result = p.transform_response("x", resp).unwrap();
    assert!(result["choices"][0]["message"]
        .get("reasoning_content")
        .is_none());
}

#[test]
fn response_thinking_only_no_text() {
    let p = provider();
    let resp = json!({
        "id": "msg_thinkonly",
        "content": [
            {"type": "thinking", "thinking": "Reasoning here...", "signature": "sig"}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 50}
    });
    let result = p.transform_response("x", resp).unwrap();
    let msg = &result["choices"][0]["message"];
    assert!(msg["content"].is_null());
    assert_eq!(msg["reasoning_content"], "Reasoning here...");
}

// ============================================================
// transform_stream_chunk — thinking events
// ============================================================

#[test]
fn stream_thinking_delta() {
    let p = provider();
    let chunk = json!({
        "type": "content_block_delta",
        "delta": {"type": "thinking_delta", "thinking": "Step 1: analyze..."}
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        parsed["choices"][0]["delta"]["reasoning_content"],
        "Step 1: analyze..."
    );
    assert!(parsed["choices"][0]["finish_reason"].is_null());
}

#[test]
fn stream_signature_delta_skipped() {
    let p = provider();
    let chunk = json!({
        "type": "content_block_delta",
        "delta": {"type": "signature_delta", "signature": "EqoBCkgIAxgC..."}
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap();
    assert!(result.is_none(), "signature_delta should be skipped");
}

// ============================================================
// model detection helpers
// ============================================================

#[test]
fn supports_thinking_models() {
    let p = provider();
    // Models that support thinking should get reasoning_effort translated
    let thinking_models = [
        "claude-3-7-sonnet-20250219",
        "claude-sonnet-4-6",
        "claude-opus-4-20250514",
        "claude-haiku-4-20250514",
        "claude-sonnet-4-6-20250514",
        "claude-opus-4-6-20250514",
    ];
    for model in thinking_models {
        let req = json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high",
        });
        let result = p.transform_request(model, &req).unwrap();
        assert!(
            result.body.get("thinking").is_some(),
            "Expected thinking to be set for {}",
            model
        );
    }
}

#[test]
fn non_thinking_models_skip_reasoning() {
    let p = provider();
    let non_thinking = [
        "claude-3-5-sonnet-20241022",
        "claude-3-opus-20240229",
        "claude-3-haiku-20240307",
    ];
    for model in non_thinking {
        let req = json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high",
        });
        let result = p.transform_request(model, &req).unwrap();
        assert!(
            result.body.get("thinking").is_none(),
            "Expected no thinking for {}",
            model
        );
    }
}

// ============================================================
// 1M context window beta header
// ============================================================

#[test]
fn context_1m_header_on_by_default_for_supported_models() {
    let p = provider();
    let supported = [
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-sonnet-4-20250514",
        "claude-opus-4-20250514",
    ];
    for model in supported {
        let req = json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let result = p.transform_request(model, &req).unwrap();
        let has_beta = result
            .headers
            .iter()
            .any(|(k, v)| k == "anthropic-beta" && v == "context-1m-2025-08-07");
        assert!(has_beta, "Expected context-1m beta header for {}", model);
    }
}

#[test]
fn context_1m_header_not_on_for_haiku() {
    let p = provider();
    let req = json!({
        "model": "claude-haiku-4-5-20251001",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p
        .transform_request("claude-haiku-4-5-20251001", &req)
        .unwrap();
    let has_beta = result
        .headers
        .iter()
        .any(|(k, v)| k == "anthropic-beta" && v == "context-1m-2025-08-07");
    assert!(!has_beta, "Haiku should not get 1M context header");
}

#[test]
fn context_1m_header_can_be_disabled() {
    let p = provider();
    let req = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "x-anthropic": {"disable_1m_context": true},
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    let has_beta = result
        .headers
        .iter()
        .any(|(k, v)| k == "anthropic-beta" && v == "context-1m-2025-08-07");
    assert!(
        !has_beta,
        "Should be disabled when x-anthropic.disable_1m_context=true"
    );
}

#[test]
fn context_1m_header_enabled_when_disable_flag_false() {
    let p = provider();
    let req = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "x-anthropic": {"disable_1m_context": false},
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    let has_beta = result
        .headers
        .iter()
        .any(|(k, v)| k == "anthropic-beta" && v == "context-1m-2025-08-07");
    assert!(has_beta, "Should be enabled when disable_1m_context=false");
}

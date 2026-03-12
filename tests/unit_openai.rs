use llmshim::provider::Provider;
use llmshim::providers::openai::OpenAi;
use serde_json::{json, Value};

fn provider() -> OpenAi {
    OpenAi::new("test-key-123".into())
}

// ============================================================
// transform_request — Responses API format
// ============================================================

#[test]
fn request_url_is_responses_api() {
    let p = provider();
    let req = json!({"model": "gpt-5.4", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.url, "https://api.openai.com/v1/responses");
}

#[test]
fn request_custom_base_url() {
    let p = OpenAi::new("key".into()).with_base_url("http://localhost:8080".into());
    let req = json!({"model": "gpt-5.4", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.url, "http://localhost:8080/responses");
}

#[test]
fn request_sets_model() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["model"], "gpt-5.4");
}

#[test]
fn request_auth_header() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    let auth = result
        .headers
        .iter()
        .find(|(k, _)| k == "Authorization")
        .unwrap();
    assert_eq!(auth.1, "Bearer test-key-123");
}

#[test]
fn request_messages_become_input() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
            {"role": "user", "content": "bye"},
        ],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    let input = result.body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["role"], "assistant");
}

#[test]
fn request_max_tokens_becomes_max_output_tokens() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 500,
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["max_output_tokens"], 500);
}

#[test]
fn request_max_completion_tokens_becomes_max_output_tokens() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "max_completion_tokens": 256,
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["max_output_tokens"], 256);
}

// ============================================================
// transform_request — reasoning config
// ============================================================

#[test]
fn request_default_reasoning_high_with_summary() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["reasoning"]["effort"], "high");
    assert_eq!(result.body["reasoning"]["summary"], "auto");
}

#[test]
fn request_reasoning_effort_passthrough() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "low",
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["reasoning"]["effort"], "low");
    assert_eq!(result.body["reasoning"]["summary"], "auto");
}

#[test]
fn request_output_config_effort_translated() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"effort": "medium"},
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["reasoning"]["effort"], "medium");
}

#[test]
fn request_reasoning_effort_takes_precedence_over_output_config() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high",
        "output_config": {"effort": "low"},
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["reasoning"]["effort"], "high");
}

// ============================================================
// transform_request — system/developer → instructions
// ============================================================

#[test]
fn request_system_message_becomes_instructions() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "hi"},
        ],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["instructions"], "You are helpful.");
    let input = result.body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1); // system message extracted
    assert_eq!(input[0]["role"], "user");
}

#[test]
fn request_developer_message_becomes_instructions() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "developer", "content": "Be concise."},
            {"role": "user", "content": "hi"},
        ],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["instructions"], "Be concise.");
}

#[test]
fn request_multiple_system_messages_merged() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "system", "content": "Part one."},
            {"role": "system", "content": "Part two."},
            {"role": "user", "content": "hi"},
        ],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["instructions"], "Part one.\n\nPart two.");
}

#[test]
fn request_no_system_no_instructions() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
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
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello", "reasoning_content": "thinking..."},
            {"role": "user", "content": "bye"},
        ],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    let input = result.body["input"].as_array().unwrap();
    assert!(input[1].get("reasoning_content").is_none());
    assert_eq!(input[1]["content"], "hello");
}

#[test]
fn request_strips_anthropic_thinking_param() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "adaptive"},
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert!(result.body.get("thinking").is_none());
}

// ============================================================
// transform_request — tools
// ============================================================

#[test]
fn request_tools_chat_completions_format_flattened() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "weather?"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        }],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    let tool = &result.body["tools"][0];
    // Responses API flat format: name/description/parameters at top level, no "function" wrapper.
    assert_eq!(tool["name"], "get_weather");
    assert_eq!(tool["description"], "Get weather");
    assert_eq!(tool["type"], "function");
    assert!(tool.get("function").is_none());
    assert!(tool["parameters"]["properties"]["city"]["type"] == "string");
}

#[test]
fn request_tools_already_flat_passthrough() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "weather?"}],
        "tools": [{
            "type": "function",
            "name": "get_weather",
            "description": "Get weather",
            "parameters": {"type": "object"},
        }],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["tools"][0]["name"], "get_weather");
}

#[test]
fn request_tools_multiple_flattened() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {"type": "function", "function": {"name": "tool_a", "description": "A", "parameters": {}}},
            {"type": "function", "function": {"name": "tool_b", "description": "B", "parameters": {}}},
        ],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["tools"][0]["name"], "tool_a");
    assert_eq!(result.body["tools"][1]["name"], "tool_b");
}

#[test]
fn request_tool_choice_anthropic_any_to_required() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": {"type": "any"},
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["tool_choice"], "required");
}

#[test]
fn request_tool_choice_anthropic_tool_to_function() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": {"type": "tool", "name": "search"},
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["tool_choice"]["type"], "function");
    assert_eq!(result.body["tool_choice"]["function"]["name"], "search");
}

// ============================================================
// transform_request — tool call/result message translation
// ============================================================

#[test]
fn request_assistant_tool_calls_become_function_call_items() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "weather?"},
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_123",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                }]
            },
            {"role": "tool", "tool_call_id": "call_123", "content": "72F sunny"},
            {"role": "assistant", "content": "It's 72F and sunny in Paris."},
        ],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    let input = result.body["input"].as_array().unwrap();

    // user message
    assert_eq!(input[0]["role"], "user");
    // assistant with empty content is dropped, but function_call item emitted
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["call_id"], "call_123");
    assert_eq!(input[1]["name"], "get_weather");
    assert_eq!(input[1]["arguments"], "{\"city\":\"Paris\"}");
    // tool result → function_call_output
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["call_id"], "call_123");
    assert_eq!(input[2]["output"], "72F sunny");
    // final assistant message
    assert_eq!(input[3]["role"], "assistant");
    assert_eq!(input[3]["content"], "It's 72F and sunny in Paris.");
}

#[test]
fn request_tool_result_message_becomes_function_call_output() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "tool", "tool_call_id": "call_abc", "content": "result data"},
        ],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    let input = result.body["input"].as_array().unwrap();
    assert_eq!(input[1]["type"], "function_call_output");
    assert_eq!(input[1]["call_id"], "call_abc");
    assert_eq!(input[1]["output"], "result data");
    assert!(input[1].get("role").is_none());
}

#[test]
fn request_rejects_non_object() {
    let p = provider();
    assert!(p.transform_request("gpt-5.4", &json!("string")).is_err());
}

// ============================================================
// transform_response — Responses API format
// ============================================================

#[test]
fn response_text_only() {
    let p = provider();
    let resp = json!({
        "id": "resp_123",
        "status": "completed",
        "output": [
            {"type": "message", "status": "completed", "content": [{"type": "output_text", "text": "Hello!"}], "role": "assistant"}
        ],
        "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7},
    });
    let result = p.transform_response("gpt-5.4", resp).unwrap();
    assert_eq!(result["object"], "chat.completion");
    assert_eq!(result["id"], "resp_123");
    assert_eq!(result["choices"][0]["message"]["content"], "Hello!");
    assert_eq!(result["choices"][0]["message"]["role"], "assistant");
    assert_eq!(result["choices"][0]["finish_reason"], "stop");
    assert_eq!(result["usage"]["prompt_tokens"], 5);
    assert_eq!(result["usage"]["completion_tokens"], 2);
}

#[test]
fn response_with_reasoning_summary() {
    let p = provider();
    let resp = json!({
        "id": "resp_456",
        "status": "completed",
        "output": [
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "Thinking step by step..."}]},
            {"type": "message", "status": "completed", "content": [{"type": "output_text", "text": "42"}], "role": "assistant"}
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    });
    let result = p.transform_response("gpt-5.4", resp).unwrap();
    assert_eq!(result["choices"][0]["message"]["content"], "42");
    assert_eq!(
        result["choices"][0]["message"]["reasoning_content"],
        "Thinking step by step..."
    );
}

#[test]
fn response_empty_reasoning_summary() {
    let p = provider();
    let resp = json!({
        "id": "resp_789",
        "status": "completed",
        "output": [
            {"type": "reasoning", "summary": []},
            {"type": "message", "status": "completed", "content": [{"type": "output_text", "text": "8"}], "role": "assistant"}
        ],
        "usage": {"input_tokens": 5, "output_tokens": 1, "total_tokens": 6},
    });
    let result = p.transform_response("gpt-5.4", resp).unwrap();
    assert_eq!(result["choices"][0]["message"]["content"], "8");
    assert!(result["choices"][0]["message"]
        .get("reasoning_content")
        .is_none());
}

#[test]
fn response_incomplete_status() {
    let p = provider();
    let resp = json!({
        "id": "resp_x",
        "status": "incomplete",
        "output": [
            {"type": "message", "status": "completed", "content": [{"type": "output_text", "text": "partial"}], "role": "assistant"}
        ],
        "usage": {},
    });
    let result = p.transform_response("gpt-5.4", resp).unwrap();
    assert_eq!(result["choices"][0]["finish_reason"], "length");
}

#[test]
fn response_function_call() {
    let p = provider();
    let resp = json!({
        "id": "resp_fc",
        "status": "completed",
        "output": [
            {
                "type": "function_call",
                "call_id": "call_abc",
                "name": "get_weather",
                "arguments": "{\"city\":\"Paris\"}"
            }
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    });
    let result = p.transform_response("gpt-5.4", resp).unwrap();
    let tc = &result["choices"][0]["message"]["tool_calls"];
    assert_eq!(tc[0]["id"], "call_abc");
    assert_eq!(tc[0]["function"]["name"], "get_weather");
    let args: Value =
        serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Paris");
}

#[test]
fn response_error() {
    let p = provider();
    let resp = json!({"error": {"message": "bad request"}});
    assert!(p.transform_response("gpt-5.4", resp).is_err());
}

// ============================================================
// transform_stream_chunk — Responses API events
// ============================================================

#[test]
fn stream_reasoning_summary_delta() {
    let p = provider();
    let chunk = json!({
        "type": "response.reasoning_summary_text.delta",
        "delta": "Thinking about...",
    });
    let result = p
        .transform_stream_chunk("gpt-5.4", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["object"], "chat.completion.chunk");
    assert_eq!(
        parsed["choices"][0]["delta"]["reasoning_content"],
        "Thinking about..."
    );
}

#[test]
fn stream_output_text_delta() {
    let p = provider();
    let chunk = json!({
        "type": "response.output_text.delta",
        "delta": "Hello",
    });
    let result = p
        .transform_stream_chunk("gpt-5.4", &serde_json::to_string(&chunk).unwrap())
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
            "usage": {"input_tokens": 10, "output_tokens": 5},
        },
    });
    let result = p
        .transform_stream_chunk("gpt-5.4", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["choices"][0]["finish_reason"], "stop");
    assert_eq!(parsed["usage"]["prompt_tokens"], 10);
    assert_eq!(parsed["usage"]["completion_tokens"], 5);
}

#[test]
fn stream_function_call_output_item_added() {
    let p = provider();
    let chunk = json!({
        "type": "response.output_item.added",
        "output_index": 1,
        "item": {
            "type": "function_call",
            "call_id": "call_xyz",
            "name": "get_weather",
        },
    });
    let result = p
        .transform_stream_chunk("gpt-5.4", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    let tc = &parsed["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc["id"], "call_xyz");
    assert_eq!(tc["function"]["name"], "get_weather");
    assert_eq!(tc["index"], 1);
}

#[test]
fn stream_function_call_non_function_item_skipped() {
    let p = provider();
    let chunk = json!({
        "type": "response.output_item.added",
        "output_index": 0,
        "item": {"type": "message"},
    });
    let result = p
        .transform_stream_chunk("gpt-5.4", &serde_json::to_string(&chunk).unwrap())
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn stream_function_call_arguments_delta() {
    let p = provider();
    let chunk = json!({
        "type": "response.function_call_arguments.delta",
        "output_index": 0,
        "delta": "{\"city\":",
    });
    let result = p
        .transform_stream_chunk("gpt-5.4", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    let tc = &parsed["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc["function"]["arguments"], "{\"city\":");
    assert_eq!(tc["index"], 0);
}

#[test]
fn stream_other_events_skipped() {
    let p = provider();
    for event_type in &[
        "response.created",
        "response.in_progress",
        "response.output_item.added",
        "response.content_part.added",
    ] {
        let chunk = json!({"type": event_type});
        let result = p
            .transform_stream_chunk("gpt-5.4", &serde_json::to_string(&chunk).unwrap())
            .unwrap();
        assert!(result.is_none(), "{} should be skipped", event_type);
    }
}

#[test]
fn stream_empty_returns_none() {
    let p = provider();
    assert!(p.transform_stream_chunk("gpt-5.4", "").unwrap().is_none());
    assert!(p.transform_stream_chunk("gpt-5.4", "  ").unwrap().is_none());
}

#[test]
fn stream_done_returns_none() {
    let p = provider();
    assert!(p
        .transform_stream_chunk("gpt-5.4", "[DONE]")
        .unwrap()
        .is_none());
}

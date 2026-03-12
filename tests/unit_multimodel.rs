/// Tests for multi-model conversation support (Cursor-style provider switching).
/// Verifies that conversation history from one provider can be sent to another
/// without breaking, and that provider-specific fields are sanitized.
use llmshim::provider::Provider;
use llmshim::providers::anthropic::Anthropic;
use llmshim::providers::openai::OpenAi;
use serde_json::json;

fn openai() -> OpenAi {
    OpenAi::new("test-key".into())
}

fn anthropic() -> Anthropic {
    Anthropic::new("test-key".into())
}

// ============================================================
// Basic multi-model conversation
// ============================================================

#[test]
fn openai_response_sent_to_anthropic() {
    let a = anthropic();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "What is Rust?"},
            {"role": "assistant", "content": "Rust is a systems programming language."},
            {"role": "user", "content": "How does it compare to Go?"},
        ],
        "max_tokens": 100,
    });
    let result = a.transform_request("claude-sonnet-4-6", &req).unwrap();
    let messages = result.body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"],
        "Rust is a systems programming language."
    );
}

#[test]
fn anthropic_response_sent_to_openai() {
    let o = openai();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "What is Rust?"},
            {"role": "assistant", "content": "Rust is a systems programming language."},
            {"role": "user", "content": "How does it compare to Go?"},
        ],
    });
    let result = o.transform_request("gpt-5.4", &req).unwrap();
    // Responses API uses "input" not "messages"
    let input = result.body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!(
        input[1]["content"],
        "Rust is a systems programming language."
    );
}

// ============================================================
// reasoning_content sanitization
// ============================================================

#[test]
fn reasoning_content_stripped_when_sent_to_openai() {
    let o = openai();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "Solve this math problem."},
            {
                "role": "assistant",
                "content": "The answer is 42.",
                "reasoning_content": "Let me think step by step..."
            },
            {"role": "user", "content": "Are you sure?"},
        ],
    });
    let result = o.transform_request("gpt-5.4", &req).unwrap();
    let input = result.body["input"].as_array().unwrap();
    assert!(input[1].get("reasoning_content").is_none());
    assert_eq!(input[1]["content"], "The answer is 42.");
}

#[test]
fn reasoning_content_stripped_when_sent_to_anthropic() {
    let a = anthropic();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "Solve this."},
            {
                "role": "assistant",
                "content": "The answer is 42.",
                "reasoning_content": "Step by step reasoning..."
            },
            {"role": "user", "content": "Explain more."},
        ],
        "max_tokens": 100,
    });
    let result = a.transform_request("claude-sonnet-4-6", &req).unwrap();
    let messages = result.body["messages"].as_array().unwrap();
    assert!(messages[1].get("reasoning_content").is_none());
    assert_eq!(messages[1]["content"], "The answer is 42.");
}

// ============================================================
// Tool calls across providers
// ============================================================

#[test]
fn openai_tool_call_history_sent_to_anthropic() {
    let a = anthropic();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "What's the weather in Paris?"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                }]
            },
            {"role": "tool", "tool_call_id": "call_abc", "content": "Sunny, 22C"},
            {"role": "user", "content": "Thanks!"},
        ],
        "max_tokens": 100,
    });
    let result = a.transform_request("claude-sonnet-4-6", &req).unwrap();
    let messages = result.body["messages"].as_array().unwrap();
    // tool_calls → tool_use content blocks
    let content = messages[1]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "tool_use");
    assert_eq!(content[0]["name"], "get_weather");
    // tool result → user with tool_result
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
}

#[test]
fn anthropic_tool_response_format_works_for_openai() {
    let a = anthropic();
    let resp = json!({
        "id": "msg_123",
        "content": [{"type": "tool_use", "id": "tu_456", "name": "search", "input": {"query": "rust"}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let normalized = a.transform_response("claude-sonnet-4-6", resp).unwrap();

    let o = openai();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "search for rust"},
            normalized["choices"][0]["message"],
            {"role": "tool", "tool_call_id": "tu_456", "content": "Results: ..."},
            {"role": "user", "content": "summarize"},
        ],
    });
    let result = o.transform_request("gpt-5.4", &req).unwrap();
    let input = result.body["input"].as_array().unwrap();
    // Assistant tool_calls are translated to Responses API function_call items.
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["name"], "search");
    assert_eq!(input[1]["call_id"], "tu_456");
    // Tool result is translated to function_call_output.
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["call_id"], "tu_456");
}

// ============================================================
// System/developer role across providers
// ============================================================

#[test]
fn developer_role_from_openai_handled_by_anthropic() {
    let a = anthropic();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "developer", "content": "You are a helpful assistant."},
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "Hello!"},
            {"role": "user", "content": "Follow up"},
        ],
        "max_tokens": 100,
    });
    let result = a.transform_request("claude-sonnet-4-6", &req).unwrap();
    assert_eq!(result.body["system"], "You are a helpful assistant.");
    let messages = result.body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
}

#[test]
fn system_role_becomes_instructions_for_openai() {
    let o = openai();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "system", "content": "Be concise."},
            {"role": "user", "content": "What is 2+2?"},
            {"role": "assistant", "content": "4"},
            {"role": "user", "content": "And 3+3?"},
        ],
    });
    let result = o.transform_request("gpt-5.4", &req).unwrap();
    // System message → instructions in Responses API
    assert_eq!(result.body["instructions"], "Be concise.");
    let input = result.body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3); // system extracted out
    assert_eq!(input[0]["role"], "user");
}

// ============================================================
// Multi-hop: A → B → A
// ============================================================

#[test]
fn round_trip_openai_anthropic_openai() {
    let o = openai();
    let a = anthropic();

    // Simulate Anthropic response with thinking
    let anthropic_resp = json!({
        "id": "msg_789",
        "content": [
            {"type": "thinking", "thinking": "Let me think of a joke...", "signature": "sig"},
            {"type": "text", "text": "Why did the crab never share? Because he's shellfish!"}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 30, "output_tokens": 20}
    });
    let normalized_resp = a
        .transform_response("claude-sonnet-4-6", anthropic_resp)
        .unwrap();
    assert_eq!(
        normalized_resp["choices"][0]["message"]["reasoning_content"],
        "Let me think of a joke..."
    );

    // Send to OpenAI with reasoning_content in history — should be stripped
    let req3 = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "Tell me a joke"},
            normalized_resp["choices"][0]["message"],
            {"role": "user", "content": "That was funny!"},
        ],
    });
    let result3 = o.transform_request("gpt-5.4", &req3).unwrap();
    let input = result3.body["input"].as_array().unwrap();
    assert!(input[1].get("reasoning_content").is_none());
    assert_eq!(
        input[1]["content"],
        "Why did the crab never share? Because he's shellfish!"
    );
}

#[test]
fn round_trip_anthropic_openai_anthropic() {
    let o = openai();
    let a = anthropic();

    // Turn 2: Send to OpenAI
    let req2 = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "Hello from Claude!"},
            {"role": "user", "content": "Now in Spanish"},
        ],
    });
    let result2 = o.transform_request("gpt-5.4", &req2).unwrap();
    assert_eq!(result2.body["input"].as_array().unwrap().len(), 3);

    // Turn 3: Back to Anthropic
    let req3 = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "Hello from Claude!"},
            {"role": "user", "content": "Now in Spanish"},
            {"role": "assistant", "content": "Hola desde Claude!"},
            {"role": "user", "content": "And in French?"},
        ],
        "max_tokens": 100,
    });
    let result3 = a.transform_request("claude-sonnet-4-6", &req3).unwrap();
    let messages = result3.body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[3]["content"], "Hola desde Claude!");
}

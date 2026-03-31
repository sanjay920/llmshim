use llmshim::provider::Provider;
use llmshim::providers::gemini::Gemini;
use serde_json::{json, Value};

fn provider() -> Gemini {
    Gemini::new("test-key".into())
}

// ============================================================
// transform_request — basic structure
// ============================================================

#[test]
fn request_url_non_streaming() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    assert!(result
        .url
        .contains("/models/gemini-3-flash-preview:generateContent"));
    assert!(result.url.contains("key=test-key"));
    assert!(!result.url.contains("alt=sse"));
}

#[test]
fn request_url_streaming() {
    let p = provider();
    let req =
        json!({"model": "x", "messages": [{"role": "user", "content": "hi"}], "stream": true});
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    assert!(result.url.contains(":streamGenerateContent"));
    assert!(result.url.contains("alt=sse"));
}

#[test]
fn request_basic_message_format() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hello"}],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"][0]["text"], "hello");
}

#[test]
fn request_multi_turn() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello!"},
            {"role": "user", "content": "how are you?"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[1]["role"], "model"); // assistant → model
    assert_eq!(contents[1]["parts"][0]["text"], "hello!");
    assert_eq!(contents[2]["role"], "user");
}

// ============================================================
// transform_request — system message
// ============================================================

#[test]
fn request_system_message_extracted() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "hi"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    assert_eq!(
        result.body["systemInstruction"]["parts"][0]["text"],
        "You are helpful."
    );
    let contents = result.body["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 1); // system not in contents
    assert_eq!(contents[0]["role"], "user");
}

#[test]
fn request_developer_role_as_system() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "developer", "content": "Be concise."},
            {"role": "user", "content": "hi"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    assert_eq!(
        result.body["systemInstruction"]["parts"][0]["text"],
        "Be concise."
    );
}

#[test]
fn request_no_system_message() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    assert!(result.body.get("systemInstruction").is_none());
}

// ============================================================
// transform_request — generation config
// ============================================================

#[test]
fn request_generation_config() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.5,
        "top_p": 0.9,
        "top_k": 40,
        "max_tokens": 100,
        "stop": ["END"],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let gc = &result.body["generationConfig"];
    assert_eq!(gc["temperature"], 0.5);
    assert_eq!(gc["topP"], 0.9);
    assert_eq!(gc["topK"], 40);
    assert_eq!(gc["maxOutputTokens"], 100);
    assert_eq!(gc["stopSequences"], json!(["END"]));
}

#[test]
fn request_max_completion_tokens_fallback() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "max_completion_tokens": 256,
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    assert_eq!(result.body["generationConfig"]["maxOutputTokens"], 256);
}

// ============================================================
// transform_request — reasoning/thinking
// ============================================================

#[test]
fn reasoning_effort_to_thinking_level() {
    let p = provider();
    for (effort, expected) in [
        ("low", "low"),
        ("medium", "medium"),
        ("high", "high"),
        ("minimal", "low"),
    ] {
        let req = json!({
            "model": "x",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": effort,
        });
        let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
        assert_eq!(
            result.body["generationConfig"]["thinkingConfig"]["thinkingLevel"], expected,
            "reasoning_effort '{}' should map to '{}'",
            effort, expected
        );
    }
}

#[test]
fn output_config_effort_to_thinking_level() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "output_config": {"effort": "medium"},
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    assert_eq!(
        result.body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "medium"
    );
}

#[test]
fn x_gemini_thinking_config_passthrough() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "x-gemini": {"thinkingConfig": {"thinkingLevel": "minimal"}},
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    assert_eq!(
        result.body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "minimal"
    );
}

// ============================================================
// transform_request — tools
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
                "description": "Get weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        }]
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let decls = &result.body["tools"][0]["functionDeclarations"];
    assert_eq!(decls[0]["name"], "get_weather");
    assert_eq!(decls[0]["description"], "Get weather");
}

#[test]
fn request_tool_choice_auto() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": "auto",
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    assert_eq!(
        result.body["toolConfig"]["functionCallingConfig"]["mode"],
        "AUTO"
    );
}

#[test]
fn request_tool_choice_required() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "tool_choice": "required",
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    assert_eq!(
        result.body["toolConfig"]["functionCallingConfig"]["mode"],
        "ANY"
    );
}

#[test]
fn request_tool_calls_in_history() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "weather?"},
            {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                    "thought_signature": "test-sig"
                }]
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "{\"temp\": 22}"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();

    // Assistant message should have functionCall
    assert_eq!(contents[1]["role"], "model");
    assert_eq!(
        contents[1]["parts"][0]["functionCall"]["name"],
        "get_weather"
    );
    assert_eq!(
        contents[1]["parts"][0]["functionCall"]["args"]["city"],
        "Paris"
    );

    // Tool result should be functionResponse with resolved function name
    assert_eq!(contents[2]["role"], "user");
    assert!(contents[2]["parts"][0].get("functionResponse").is_some());
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"]["name"],
        "get_weather"
    );
    // JSON object response passed through directly
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"]["response"]["temp"],
        22
    );
}

#[test]
fn request_tool_result_array_wrapped_as_object() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "data?"},
            {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_0",
                    "type": "function",
                    "function": {"name": "list_items", "arguments": "{}"},
                    "thought_signature": "test-sig"
                }]
            },
            {"role": "tool", "tool_call_id": "call_0", "content": "[\"a\", \"b\", \"c\"]"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();
    let fr = &contents[2]["parts"][0]["functionResponse"];
    assert_eq!(fr["name"], "list_items");
    // Array should be wrapped in {"result": [...]}
    assert!(fr["response"]["result"].is_array());
    assert_eq!(fr["response"]["result"][0], "a");
}

#[test]
fn request_tool_result_plain_string_wrapped() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "hi"},
            {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_0",
                    "type": "function",
                    "function": {"name": "echo", "arguments": "{}"},
                    "thought_signature": "test-sig"
                }]
            },
            {"role": "tool", "tool_call_id": "call_0", "content": "hello world"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let fr = &result.body["contents"].as_array().unwrap()[2]["parts"][0]["functionResponse"];
    assert_eq!(fr["name"], "echo");
    assert_eq!(fr["response"]["result"], "hello world");
}

// ============================================================
// transform_response
// ============================================================

#[test]
fn response_text_only() {
    let p = provider();
    let resp = json!({
        "candidates": [{"content": {"parts": [{"text": "Hello!"}], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2, "totalTokenCount": 7},
        "responseId": "resp_123",
    });
    let result = p
        .transform_response("gemini-3-flash-preview", resp)
        .unwrap();
    assert_eq!(result["object"], "chat.completion");
    assert_eq!(result["id"], "resp_123");
    assert_eq!(result["choices"][0]["message"]["content"], "Hello!");
    assert_eq!(result["choices"][0]["finish_reason"], "stop");
    assert_eq!(result["usage"]["prompt_tokens"], 5);
    assert_eq!(result["usage"]["completion_tokens"], 2);
    assert_eq!(result["usage"]["total_tokens"], 7);
}

#[test]
fn response_with_thought_signature_text_preserved() {
    let p = provider();
    let resp = json!({
        "candidates": [{"content": {"parts": [
            {"text": "Hello!", "thoughtSignature": "abc123=="}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2, "totalTokenCount": 7},
    });
    let result = p
        .transform_response("gemini-3-flash-preview", resp)
        .unwrap();
    assert_eq!(result["choices"][0]["message"]["content"], "Hello!");
}

#[test]
fn response_function_call() {
    let p = provider();
    let resp = json!({
        "candidates": [{"content": {"parts": [
            {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15},
    });
    let result = p
        .transform_response("gemini-3-flash-preview", resp)
        .unwrap();
    let tc = &result["choices"][0]["message"]["tool_calls"];
    assert_eq!(tc[0]["function"]["name"], "get_weather");
    let args: Value =
        serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Paris");
}

#[test]
fn response_max_tokens_finish() {
    let p = provider();
    let resp = json!({
        "candidates": [{"content": {"parts": [{"text": "partial..."}]}, "finishReason": "MAX_TOKENS"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_response("gemini-3-flash-preview", resp)
        .unwrap();
    assert_eq!(result["choices"][0]["finish_reason"], "length");
}

#[test]
fn response_safety_finish() {
    let p = provider();
    let resp = json!({
        "candidates": [{"content": {"parts": [{"text": ""}]}, "finishReason": "SAFETY"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_response("gemini-3-flash-preview", resp)
        .unwrap();
    assert_eq!(result["choices"][0]["finish_reason"], "content_filter");
}

#[test]
fn response_error() {
    let p = provider();
    let resp = json!({"error": {"code": 404, "message": "model not found"}});
    let err = p.transform_response("x", resp).unwrap_err();
    let msg = format!("{}", err);
    assert!(msg.contains("model not found"));
}

// ============================================================
// transform_stream_chunk
// ============================================================

#[test]
fn stream_text_chunk() {
    let p = provider();
    let chunk = json!({
        "candidates": [{"content": {"parts": [{"text": "Hello"}], "role": "model"}, "index": 0}],
        "usageMetadata": {},
    });
    let result = p
        .transform_stream_chunk(
            "gemini-3-flash-preview",
            &serde_json::to_string(&chunk).unwrap(),
        )
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["object"], "chat.completion.chunk");
    assert_eq!(parsed["choices"][0]["delta"]["content"], "Hello");
}

#[test]
fn stream_thought_signature_only_skipped() {
    let p = provider();
    // Chunk with empty text + thoughtSignature should be skipped
    let chunk = json!({
        "candidates": [{"content": {"parts": [
            {"text": "", "thoughtSignature": "abc123=="}
        ], "role": "model"}, "finishReason": "STOP", "index": 0}],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 3, "totalTokenCount": 8},
    });
    let result = p
        .transform_stream_chunk(
            "gemini-3-flash-preview",
            &serde_json::to_string(&chunk).unwrap(),
        )
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    // Should still emit because of finishReason
    assert_eq!(parsed["choices"][0]["finish_reason"], "stop");
}

#[test]
fn stream_empty_skipped() {
    let p = provider();
    assert!(p.transform_stream_chunk("x", "").unwrap().is_none());
    assert!(p.transform_stream_chunk("x", "  ").unwrap().is_none());
}

#[test]
fn stream_function_call() {
    let p = provider();
    let chunk = json!({
        "candidates": [{"content": {"parts": [
            {"functionCall": {"name": "search", "args": {"q": "rust"}}}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        parsed["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "search"
    );
}

// ============================================================
// Cross-provider sanitization
// ============================================================

#[test]
fn strips_openai_annotations_from_history() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello!", "annotations": [], "refusal": null, "reasoning_content": "thought..."},
            {"role": "user", "content": "bye"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();
    let model_msg = &contents[1];
    // Should only have text parts, no foreign fields
    assert_eq!(model_msg["parts"][0]["text"], "hello!");
    // These shouldn't leak through as Gemini doesn't understand them
    assert!(model_msg.get("annotations").is_none());
    assert!(model_msg.get("refusal").is_none());
    assert!(model_msg.get("reasoning_content").is_none());
}

// ============================================================
// includeThoughts always set
// ============================================================

#[test]
fn request_always_includes_thoughts() {
    let p = provider();
    let req = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    assert_eq!(
        result.body["generationConfig"]["thinkingConfig"]["includeThoughts"], true,
        "includeThoughts should always be true"
    );
}

#[test]
fn request_includes_thoughts_with_thinking_level() {
    let p = provider();
    let req = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high",
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let tc = &result.body["generationConfig"]["thinkingConfig"];
    assert_eq!(tc["includeThoughts"], true);
    assert_eq!(tc["thinkingLevel"], "high");
}

// ============================================================
// Response with thought parts
// ============================================================

#[test]
fn response_with_thought_parts() {
    let p = provider();
    let resp = json!({
        "candidates": [{"content": {"parts": [
            {"text": "Let me think about this...", "thought": true},
            {"text": "The answer is 42."}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 10, "totalTokenCount": 15},
    });
    let result = p
        .transform_response("gemini-3-flash-preview", resp)
        .unwrap();
    assert_eq!(
        result["choices"][0]["message"]["content"],
        "The answer is 42."
    );
    assert_eq!(
        result["choices"][0]["message"]["reasoning_content"],
        "Let me think about this..."
    );
}

#[test]
fn response_thought_only_no_text() {
    let p = provider();
    let resp = json!({
        "candidates": [{"content": {"parts": [
            {"text": "Just thinking...", "thought": true}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_response("gemini-3-flash-preview", resp)
        .unwrap();
    assert!(result["choices"][0]["message"]["content"].is_null());
    assert_eq!(
        result["choices"][0]["message"]["reasoning_content"],
        "Just thinking..."
    );
}

#[test]
fn response_no_thought_no_reasoning_content() {
    let p = provider();
    let resp = json!({
        "candidates": [{"content": {"parts": [
            {"text": "Hello!"}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_response("gemini-3-flash-preview", resp)
        .unwrap();
    assert_eq!(result["choices"][0]["message"]["content"], "Hello!");
    assert!(result["choices"][0]["message"]
        .get("reasoning_content")
        .is_none());
}

// ============================================================
// Stream with thought parts
// ============================================================

#[test]
fn stream_thought_part() {
    let p = provider();
    let chunk = json!({
        "candidates": [{"content": {"parts": [
            {"text": "Thinking...", "thought": true}
        ], "role": "model"}, "index": 0}],
        "usageMetadata": {},
    });
    let result = p
        .transform_stream_chunk(
            "gemini-3-flash-preview",
            &serde_json::to_string(&chunk).unwrap(),
        )
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        parsed["choices"][0]["delta"]["reasoning_content"],
        "Thinking..."
    );
    assert!(parsed["choices"][0]["delta"].get("content").is_none());
}

#[test]
fn stream_text_and_thought_in_same_chunk() {
    let p = provider();
    let chunk = json!({
        "candidates": [{"content": {"parts": [
            {"text": "Reasoning here", "thought": true},
            {"text": "Answer here"}
        ], "role": "model"}, "index": 0}],
        "usageMetadata": {},
    });
    let result = p
        .transform_stream_chunk(
            "gemini-3-flash-preview",
            &serde_json::to_string(&chunk).unwrap(),
        )
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        parsed["choices"][0]["delta"]["reasoning_content"],
        "Reasoning here"
    );
    assert_eq!(parsed["choices"][0]["delta"]["content"], "Answer here");
}

// ============================================================
// thought_signature roundtrip
// ============================================================

#[test]
fn response_preserves_thought_signature_on_tool_calls() {
    let p = provider();
    let gemini_response = json!({
        "candidates": [{
            "content": {
                "parts": [
                    {
                        "functionCall": {"name": "get_quote", "args": {"symbols": ["AAPL"]}},
                        "thoughtSignature": "abc123-sig"
                    }
                ],
                "role": "model"
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15}
    });
    let result = p
        .transform_response("gemini-3-flash-preview", gemini_response)
        .unwrap();
    let tc = &result["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tc["function"]["name"], "get_quote");
    assert_eq!(tc["thought_signature"], "abc123-sig");
}

#[test]
fn request_echoes_thought_signature_in_function_call_parts() {
    let p = provider();
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [
            {"role": "user", "content": "Get AAPL quote"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_0",
                    "type": "function",
                    "function": {"name": "get_quote", "arguments": "{\"symbols\":[\"AAPL\"]}"},
                    "thought_signature": "abc123-sig"
                }]
            },
            {"role": "tool", "tool_call_id": "call_0", "name": "get_quote", "content": "{\"price\": 150}"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();
    // The model (assistant) message should have functionCall with thoughtSignature
    let model_parts = contents[1]["parts"].as_array().unwrap();
    let fc_part = model_parts
        .iter()
        .find(|p| p.get("functionCall").is_some())
        .unwrap();
    assert_eq!(fc_part["functionCall"]["name"], "get_quote");
    assert_eq!(fc_part["thoughtSignature"], "abc123-sig");
}

#[test]
fn request_without_thought_signature_strips_function_calls() {
    // functionCall without thought_signature should be stripped by enforce_gemini_turn_order
    let p = provider();
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [
            {"role": "user", "content": "Get quote"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_0",
                    "type": "function",
                    "function": {"name": "get_quote", "arguments": "{}"}
                }]
            },
            {"role": "tool", "tool_call_id": "call_0", "name": "get_quote", "content": "{}"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();
    // The functionCall+functionResponse pair should be stripped (missing thought_signature)
    // Only the user turn should remain
    for turn in contents {
        let parts = turn["parts"].as_array().unwrap();
        for part in parts {
            assert!(
                part.get("functionCall").is_none(),
                "functionCall without thought_signature should be stripped"
            );
            assert!(
                part.get("functionResponse").is_none(),
                "orphaned functionResponse should be stripped"
            );
        }
    }
}

#[test]
fn stream_preserves_thought_signature_on_tool_calls() {
    let p = provider();
    let chunk = json!({
        "candidates": [{
            "content": {
                "parts": [{
                    "functionCall": {"name": "get_quote", "args": {"symbols": ["AAPL"]}},
                    "thoughtSignature": "stream-sig-xyz"
                }],
                "role": "model"
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5}
    });
    let result = p
        .transform_stream_chunk(
            "gemini-3-flash-preview",
            &serde_json::to_string(&chunk).unwrap(),
        )
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    let tc = &parsed["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc["function"]["name"], "get_quote");
    assert_eq!(tc["thought_signature"], "stream-sig-xyz");
}

// ============================================================
// Streaming: parallel tool calls, empty args, no-name skipped
// ============================================================

#[test]
fn stream_parallel_tool_calls_each_get_unique_index() {
    // Gemini sends each parallel tool call as a separate SSE chunk.
    // Each chunk arrives with its own parts containing functionCall.
    // The llmshim transform_stream_chunk assigns incrementing indices.
    let p = provider();

    // First tool call chunk
    let chunk1 = json!({
        "candidates": [{"content": {"parts": [
            {"functionCall": {"name": "get_quote", "args": {"symbol": "AAPL"}}, "thoughtSignature": "sig1"}
        ], "role": "model"}}],
        "usageMetadata": {},
    });
    let result1 = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk1).unwrap())
        .unwrap()
        .unwrap();
    let parsed1: Value = serde_json::from_str(&result1).unwrap();
    let tc1 = &parsed1["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc1["function"]["name"], "get_quote");
    assert_eq!(tc1["index"], 0);
    assert_eq!(tc1["thought_signature"], "sig1");

    // Second tool call chunk (same SSE stream, would arrive separately)
    let chunk2 = json!({
        "candidates": [{"content": {"parts": [
            {"functionCall": {"name": "get_news", "args": {"count": 5}}, "thoughtSignature": "sig2"}
        ], "role": "model"}}],
        "usageMetadata": {},
    });
    let result2 = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk2).unwrap())
        .unwrap()
        .unwrap();
    let parsed2: Value = serde_json::from_str(&result2).unwrap();
    let tc2 = &parsed2["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc2["function"]["name"], "get_news");
    // Each chunk is independently transformed — index starts at 0 within the chunk.
    // The ragents runner handles deduplication across chunks.
    assert_eq!(tc2["index"], 0);
    assert_eq!(tc2["thought_signature"], "sig2");
}

#[test]
fn stream_multiple_tool_calls_in_single_chunk() {
    // Gemini can also send multiple functionCall parts in a single chunk
    let p = provider();
    let chunk = json!({
        "candidates": [{"content": {"parts": [
            {"functionCall": {"name": "get_quote", "args": {"symbol": "AAPL"}}, "thoughtSignature": "sig1"},
            {"functionCall": {"name": "get_news", "args": {"count": 5}}, "thoughtSignature": "sig2"}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    let tcs = parsed["choices"][0]["delta"]["tool_calls"].as_array().unwrap();
    assert_eq!(tcs.len(), 2);
    assert_eq!(tcs[0]["function"]["name"], "get_quote");
    assert_eq!(tcs[0]["index"], 0);
    assert_eq!(tcs[0]["thought_signature"], "sig1");
    assert_eq!(tcs[1]["function"]["name"], "get_news");
    assert_eq!(tcs[1]["index"], 1);
    assert_eq!(tcs[1]["thought_signature"], "sig2");
}

#[test]
fn stream_empty_args_produces_empty_json_object() {
    let p = provider();
    // functionCall with no args field at all
    let chunk = json!({
        "candidates": [{"content": {"parts": [
            {"functionCall": {"name": "list_positions"}}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    let args = parsed["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(args, "{}", "Missing args should produce empty JSON object string");
}

#[test]
fn stream_null_args_produces_empty_json_object() {
    let p = provider();
    // functionCall with null args
    let chunk = json!({
        "candidates": [{"content": {"parts": [
            {"functionCall": {"name": "list_positions", "args": null}}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    let args = parsed["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(args, "{}", "Null args should produce empty JSON object string");
}

#[test]
fn stream_no_name_function_call_skipped() {
    let p = provider();
    // functionCall with empty name should be skipped entirely
    let chunk = json!({
        "candidates": [{"content": {"parts": [
            {"functionCall": {"name": "", "args": {"x": 1}}}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    // Should not have tool_calls since the only functionCall had empty name
    assert!(
        parsed["choices"][0]["delta"].get("tool_calls").is_none(),
        "functionCall with empty name should be skipped"
    );
}

#[test]
fn stream_no_name_function_call_skipped_but_valid_ones_kept() {
    let p = provider();
    // Mix of empty-name and valid functionCalls
    let chunk = json!({
        "candidates": [{"content": {"parts": [
            {"functionCall": {"name": "", "args": {}}},
            {"functionCall": {"name": "get_quote", "args": {"symbol": "AAPL"}}, "thoughtSignature": "sig"}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_stream_chunk("x", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    let tcs = parsed["choices"][0]["delta"]["tool_calls"].as_array().unwrap();
    assert_eq!(tcs.len(), 1, "Only valid functionCalls should be included");
    assert_eq!(tcs[0]["function"]["name"], "get_quote");
}

#[test]
fn response_empty_args_produces_empty_json_object() {
    // Non-streaming: functionCall with no args
    let p = provider();
    let resp = json!({
        "candidates": [{"content": {"parts": [
            {"functionCall": {"name": "list_positions"}}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_response("gemini-3-flash-preview", resp)
        .unwrap();
    let args = result["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(args, "{}", "Missing args should produce empty JSON object string");
}

#[test]
fn response_no_name_function_call_skipped() {
    let p = provider();
    let resp = json!({
        "candidates": [{"content": {"parts": [
            {"functionCall": {"name": "", "args": {"x": 1}}}
        ], "role": "model"}, "finishReason": "STOP"}],
        "usageMetadata": {},
    });
    let result = p
        .transform_response("gemini-3-flash-preview", resp)
        .unwrap();
    // No tool_calls should be present
    assert!(
        result["choices"][0]["message"].get("tool_calls").is_none(),
        "functionCall with empty name should be skipped"
    );
}

// ============================================================
// Session history: turn ordering enforcement
// ============================================================

#[test]
fn session_history_orphaned_tool_calls_stripped() {
    // Simulate session history where assistant made tool calls but the session
    // was cut short — no tool results followed. Gemini should not see the functionCall.
    let p = provider();
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [
            {"role": "user", "content": "Check the news"},
            {
                "role": "assistant",
                "content": "Let me check.",
                "tool_calls": [{
                    "id": "call_old",
                    "type": "function",
                    "function": {"name": "get_news", "arguments": "{}"}
                }]
            },
            // No tool result — session was interrupted
            // New run starts with a new user message
            {"role": "user", "content": "Check the news again"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();

    // The model turn should NOT have functionCall parts (orphaned)
    for turn in contents {
        if turn["role"] == "model" {
            let parts = turn["parts"].as_array().unwrap();
            for part in parts {
                assert!(
                    part.get("functionCall").is_none(),
                    "Orphaned functionCall should be stripped"
                );
            }
        }
    }
}

#[test]
fn session_history_valid_tool_roundtrip_preserved() {
    // Valid history: assistant tool call with thought_signature followed by tool result
    let p = provider();
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [
            {"role": "user", "content": "Get a quote"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_quote", "arguments": "{\"symbols\":[\"AAPL\"]}"},
                    "thought_signature": "valid-sig"
                }]
            },
            {"role": "tool", "tool_call_id": "call_1", "name": "get_quote", "content": "{\"price\":150}"},
            {"role": "assistant", "content": "AAPL is at $150."},
            {"role": "user", "content": "Thanks"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();

    // The model turn with functionCall should be preserved (followed by functionResponse)
    let model_with_fc = contents.iter().any(|t| {
        t["role"] == "model"
            && t["parts"]
                .as_array()
                .map(|p| p.iter().any(|part| part.get("functionCall").is_some()))
                .unwrap_or(false)
    });
    assert!(model_with_fc, "Valid functionCall should be preserved");

    // And there should be a functionResponse
    let has_fr = contents.iter().any(|t| {
        t["parts"]
            .as_array()
            .map(|p| p.iter().any(|part| part.get("functionResponse").is_some()))
            .unwrap_or(false)
    });
    assert!(has_fr, "functionResponse should be present");
}

#[test]
fn session_history_consecutive_roles_merged() {
    // If session has two consecutive user messages, they should be merged
    let p = provider();
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [
            {"role": "user", "content": "First message"},
            {"role": "user", "content": "Second message"},
            {"role": "assistant", "content": "Reply"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();

    // Should have exactly 2 turns: user (merged) + model
    assert_eq!(contents.len(), 2, "Consecutive user turns should be merged");
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[1]["role"], "model");

    // Merged user turn should have 2 text parts
    let user_parts = contents[0]["parts"].as_array().unwrap();
    assert_eq!(user_parts.len(), 2);
}

#[test]
fn session_history_compacted_starts_with_model_tool_call() {
    // Reproduces the news-producer bug: session history starts with
    // assistant(tool_calls) after system message extraction, meaning
    // the first content turn is model(functionCall) — invalid for Gemini.
    let p = provider();
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [
            {"role": "system", "content": "You are a news agent."},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_0",
                    "type": "function",
                    "function": {"name": "read_news_board", "arguments": "{}"}
                }]
            },
            {"role": "tool", "tool_call_id": "call_0", "name": "read_news_board", "content": "{\"items\":[]}"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_0",
                    "type": "function",
                    "function": {"name": "get_news", "arguments": "{\"count\":10}"}
                }]
            },
            {"role": "tool", "tool_call_id": "call_0", "name": "get_news", "content": "{\"headlines\":[]}"},
            {"role": "assistant", "content": "No new headlines."},
            {"role": "user", "content": "Check news again"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();

    // All orphaned functionCall/functionResponse pairs must be stripped.
    // No functionCall should appear without a preceding user turn.
    // No functionResponse should appear without a preceding functionCall.
    for (i, turn) in contents.iter().enumerate() {
        let parts = turn["parts"].as_array().unwrap();

        let has_fc = parts.iter().any(|p| p.get("functionCall").is_some());
        let has_fr = parts.iter().any(|p| p.get("functionResponse").is_some());

        if has_fc {
            assert!(i > 0, "functionCall at position 0 is invalid");
            assert_eq!(
                contents[i - 1]["role"],
                "user",
                "functionCall at position {i} must follow a user turn"
            );
            // Must be followed by functionResponse
            assert!(
                contents.get(i + 1).is_some_and(|next| {
                    next["parts"]
                        .as_array()
                        .map(|p| p.iter().any(|part| part.get("functionResponse").is_some()))
                        .unwrap_or(false)
                }),
                "functionCall at position {i} must be followed by functionResponse"
            );
        }

        if has_fr {
            assert!(i > 0, "functionResponse at position 0 is invalid");
            assert!(
                contents[i - 1]["parts"]
                    .as_array()
                    .map(|p| p.iter().any(|part| part.get("functionCall").is_some()))
                    .unwrap_or(false),
                "functionResponse at position {i} must follow a functionCall"
            );
        }
    }
}

#[test]
fn session_history_empty_assistant_turns_removed() {
    // Empty assistant messages (no content, no tool_calls) should be removed
    let p = provider();
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [
            {"role": "user", "content": "Hello"},
            {"role": "assistant", "content": ""},
            {"role": "user", "content": "Hello again"},
            {"role": "assistant", "content": "Hi there"},
        ],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let contents = result.body["contents"].as_array().unwrap();

    // The empty assistant turn should be removed, resulting in merged user turns
    for turn in contents {
        if turn["role"] == "model" {
            let parts = turn["parts"].as_array().unwrap();
            let has_real_content = parts.iter().any(|p| {
                p.get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(true)
            });
            assert!(has_real_content, "Empty model turns should be removed");
        }
    }
}

// ============================================================
// Full tool call roundtrip: Gemini response -> OpenAI format -> back to Gemini request
// Simulates what ragents does: receives tool calls via streaming, accumulates them,
// sends results back. thought_signature MUST survive the full roundtrip.
// ============================================================

#[test]
fn full_roundtrip_thought_signature_preserved() {
    let p = provider();

    // Step 1: Gemini returns a functionCall with thoughtSignature (via streaming)
    let stream_chunk = json!({
        "candidates": [{
            "content": {
                "parts": [{
                    "functionCall": {"name": "get_quote", "args": {"symbol": "AAPL"}},
                    "thoughtSignature": "roundtrip-sig-abc"
                }],
                "role": "model"
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5}
    });
    let chunk_result = p
        .transform_stream_chunk("gemini-3-flash-preview", &serde_json::to_string(&stream_chunk).unwrap())
        .unwrap()
        .unwrap();
    let chunk_parsed: Value = serde_json::from_str(&chunk_result).unwrap();

    // Extract tool call from the transformed chunk (what ragents would accumulate)
    let tc = &chunk_parsed["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc["thought_signature"], "roundtrip-sig-abc");

    // Step 2: Simulate ragents building the assistant message with accumulated tool calls
    // This is what ragents/src/runner.rs does after streaming accumulation
    let assistant_message = json!({
        "role": "assistant",
        "content": "",
        "tool_calls": [{
            "id": tc["id"].as_str().unwrap(),
            "type": "function",
            "function": {
                "name": tc["function"]["name"].as_str().unwrap(),
                "arguments": tc["function"]["arguments"].as_str().unwrap(),
            },
            "thought_signature": tc["thought_signature"].clone()
        }]
    });

    // Step 3: Simulate ragents adding the tool result
    let tool_result = json!({
        "role": "tool",
        "tool_call_id": tc["id"].as_str().unwrap(),
        "name": "get_quote",
        "content": "{\"price\": 150.0}"
    });

    // Step 4: Send the full conversation back to Gemini
    let follow_up_request = json!({
        "model": "gemini-3-flash-preview",
        "messages": [
            {"role": "user", "content": "Get me an AAPL quote"},
            assistant_message,
            tool_result,
            {"role": "user", "content": "Now what about TSLA?"},
        ]
    });
    let result = p.transform_request("gemini-3-flash-preview", &follow_up_request).unwrap();
    let contents = result.body["contents"].as_array().unwrap();

    // The model turn with functionCall should be preserved (has thought_signature)
    let model_fc_turn = contents.iter().find(|t| {
        t["role"] == "model"
            && t["parts"]
                .as_array()
                .map(|p| p.iter().any(|part| part.get("functionCall").is_some()))
                .unwrap_or(false)
    });
    assert!(
        model_fc_turn.is_some(),
        "Model turn with functionCall should be preserved when thought_signature is present"
    );

    // Verify thoughtSignature is echoed back
    let fc_part = model_fc_turn.unwrap()["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p.get("functionCall").is_some())
        .unwrap();
    assert_eq!(
        fc_part["thoughtSignature"], "roundtrip-sig-abc",
        "thoughtSignature must be echoed back in the Gemini request"
    );

    // And there should be a functionResponse following it
    let fr_turn = contents.iter().find(|t| {
        t["parts"]
            .as_array()
            .map(|p| p.iter().any(|part| part.get("functionResponse").is_some()))
            .unwrap_or(false)
    });
    assert!(
        fr_turn.is_some(),
        "functionResponse should be present after the functionCall"
    );
}

#[test]
fn full_roundtrip_without_thought_signature_gets_stripped() {
    // When ragents does NOT preserve thought_signature (the bug scenario),
    // enforce_gemini_turn_order strips the functionCall pair, breaking Gemini
    let p = provider();

    let request = json!({
        "model": "gemini-3-flash-preview",
        "messages": [
            {"role": "user", "content": "Get me a quote"},
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_0",
                    "type": "function",
                    "function": {"name": "get_quote", "arguments": "{\"symbol\":\"AAPL\"}"}
                    // NOTE: no thought_signature — this is the bug case
                }]
            },
            {"role": "tool", "tool_call_id": "call_0", "name": "get_quote", "content": "{\"price\":150}"},
            {"role": "user", "content": "Now TSLA?"},
        ]
    });
    let result = p.transform_request("gemini-3-flash-preview", &request).unwrap();
    let contents = result.body["contents"].as_array().unwrap();

    // Without thought_signature, the functionCall pair gets stripped
    for turn in contents {
        let parts = turn["parts"].as_array().unwrap();
        for part in parts {
            assert!(
                part.get("functionCall").is_none(),
                "functionCall without thought_signature should be stripped by enforce_gemini_turn_order"
            );
            assert!(
                part.get("functionResponse").is_none(),
                "orphaned functionResponse should be stripped"
            );
        }
    }
}

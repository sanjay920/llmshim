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
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
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

    // Tool result should be functionResponse
    assert_eq!(contents[2]["role"], "user");
    assert!(contents[2]["parts"][0].get("functionResponse").is_some());
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"]["name"],
        "call_1"
    );
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

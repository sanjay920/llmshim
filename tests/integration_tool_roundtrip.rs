/// Integration tests for tool call roundtrips — the full cycle of:
/// 1. Send request with tools → provider returns function call
/// 2. Execute tool (fake) → send results back → provider gives final answer
///
/// Tests both Gemini (thought_signature requirement) and xAI (Responses API format).
///
/// Run: cargo test --test integration_tool_roundtrip -- --ignored --test-threads=1
use serde_json::{json, Value};

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

fn weather_tools() -> Value {
    json!([{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get current weather for a city",
            "parameters": {
                "type": "object",
                "properties": {
                    "city": {"type": "string", "description": "City name"}
                },
                "required": ["city"]
            }
        }
    }])
}

/// Simulate a full tool roundtrip: call LLM → get tool call → send result → get answer.
async fn tool_roundtrip(model: &str) -> Result<String, String> {
    let router = router();

    // Step 1: Send initial request with tools
    let req1 = json!({
        "model": model,
        "messages": [{"role": "user", "content": "What is the weather in Tokyo? Use the get_weather tool."}],
        "max_tokens": 500,
        "tools": weather_tools(),
        "tool_choice": "required",
    });

    let resp1 = llmshim::completion(&router, &req1)
        .await
        .map_err(|e| format!("Step 1 failed: {e}"))?;

    let msg1 = &resp1["choices"][0]["message"];
    let tool_calls = msg1
        .get("tool_calls")
        .and_then(|tc| tc.as_array())
        .ok_or_else(|| format!("No tool_calls in response: {msg1}"))?;

    assert!(!tool_calls.is_empty(), "Expected at least one tool call");
    let tc = &tool_calls[0];
    let func_name = tc["function"]["name"].as_str().unwrap_or("");
    assert_eq!(func_name, "get_weather", "Expected get_weather tool call");

    // Step 2: Build follow-up with tool result
    let mut messages = vec![
        json!({"role": "user", "content": "What is the weather in Tokyo? Use the get_weather tool."}),
        msg1.clone(),
        json!({
            "role": "tool",
            "tool_call_id": tc["id"].as_str().unwrap_or("call_0"),
            "name": "get_weather",
            "content": "{\"temperature\": 22, \"condition\": \"sunny\", \"city\": \"Tokyo\"}"
        }),
    ];

    // For Gemini, the message content might be null — that's fine
    if let Some(obj) = messages[1].as_object_mut() {
        if obj.get("content").map(|c| c.is_null()).unwrap_or(true) {
            obj.remove("content");
        }
    }

    let req2 = json!({
        "model": model,
        "messages": messages,
        "max_tokens": 500,
        "tools": weather_tools(),
    });

    let resp2 = llmshim::completion(&router, &req2)
        .await
        .map_err(|e| format!("Step 2 failed (tool result roundtrip): {e}"))?;

    let msg2 = &resp2["choices"][0]["message"];
    let answer = msg2["content"].as_str().unwrap_or("").to_string();

    Ok(answer)
}

#[tokio::test]
#[ignore]
async fn gemini_tool_roundtrip_with_thought_signature() {
    let result = tool_roundtrip("gemini/gemini-3-flash-preview").await;
    match result {
        Ok(answer) => {
            println!("Gemini tool roundtrip OK: {answer}");
            assert!(
                !answer.is_empty(),
                "Expected non-empty answer after tool result"
            );
        }
        Err(e) => panic!("{e}"),
    }
}

#[tokio::test]
#[ignore]
async fn xai_tool_roundtrip() {
    let result = tool_roundtrip("xai/grok-4-1-fast-reasoning").await;
    match result {
        Ok(answer) => {
            println!("xAI tool roundtrip OK: {answer}");
            assert!(
                !answer.is_empty(),
                "Expected non-empty answer after tool result"
            );
        }
        Err(e) => panic!("{e}"),
    }
}

#[tokio::test]
#[ignore]
async fn openai_tool_roundtrip() {
    let result = tool_roundtrip("openai/gpt-5.4").await;
    match result {
        Ok(answer) => {
            println!("OpenAI tool roundtrip OK: {answer}");
            assert!(
                !answer.is_empty(),
                "Expected non-empty answer after tool result"
            );
        }
        Err(e) => panic!("{e}"),
    }
}

#[tokio::test]
#[ignore]
async fn anthropic_tool_roundtrip() {
    let result = tool_roundtrip("anthropic/claude-sonnet-4-6").await;
    match result {
        Ok(answer) => {
            println!("Anthropic tool roundtrip OK: {answer}");
            assert!(
                !answer.is_empty(),
                "Expected non-empty answer after tool result"
            );
        }
        Err(e) => panic!("{e}"),
    }
}

/// Simulate accumulated session history — multiple turns of tool use.
/// This is the exact scenario that broke xAI (Bug 2).
#[tokio::test]
#[ignore]
async fn xai_accumulated_session_history() {
    let router = router();

    // Simulate what a session would look like after a prior run:
    // user → assistant (tool call) → tool result → assistant (answer) → user (new turn)
    let accumulated = json!({
        "model": "xai/grok-4-1-fast-reasoning",
        "messages": [
            {"role": "user", "content": "What is the weather in Tokyo?"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_prev_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}"}
                }]
            },
            {"role": "tool", "tool_call_id": "call_prev_1", "content": "{\"temp\": 22, \"condition\": \"sunny\"}"},
            {"role": "assistant", "content": "It's 22°C and sunny in Tokyo."},
            {"role": "user", "content": "Now what about Paris?"},
        ],
        "max_tokens": 500,
        "tools": weather_tools(),
    });

    let result = llmshim::completion(&router, &accumulated).await;
    match result {
        Ok(resp) => {
            let msg = &resp["choices"][0]["message"];
            println!(
                "xAI session history OK: content={}, tool_calls={}",
                msg.get("content").unwrap_or(&json!(null)),
                msg.get("tool_calls").is_some()
            );
        }
        Err(e) => panic!("xAI rejected accumulated session history: {e}"),
    }
}

/// Integration tests for thinking/reasoning features hitting real APIs.
/// Run with: cargo test --test integration_thinking -- --ignored
use futures::StreamExt;
use serde_json::{json, Value};

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

fn skip_if_missing_keys() -> bool {
    std::env::var("OPENAI_API_KEY").is_err() || std::env::var("ANTHROPIC_API_KEY").is_err()
}

// ============================================================
// Anthropic thinking — basic completion
// ============================================================

#[tokio::test]
#[ignore]
async fn anthropic_thinking_via_reasoning_effort() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();

    let req = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "What is 15 * 37?"}],
        "max_tokens": 4000,
        "reasoning_effort": "high",
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();

    // Should be in OpenAI format
    assert_eq!(resp["object"], "chat.completion");
    assert_eq!(resp["choices"][0]["finish_reason"], "stop");

    // Should have text content with the answer
    let content = resp["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(content.contains("555"), "Expected 555, got: {}", content);

    // Should have reasoning_content from thinking blocks
    let reasoning = resp["choices"][0]["message"].get("reasoning_content");
    assert!(
        reasoning.is_some(),
        "Expected reasoning_content to be present"
    );
    let reasoning_text = reasoning.unwrap().as_str().unwrap();
    assert!(
        !reasoning_text.is_empty(),
        "reasoning_content should not be empty"
    );
    println!("Reasoning: {}", reasoning_text);
    println!("Answer: {}", content);
}

#[tokio::test]
#[ignore]
async fn anthropic_thinking_via_direct_param() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();

    let req = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "What is the square root of 144?"}],
        "max_tokens": 4000,
        "thinking": {"type": "enabled", "budget_tokens": 2000},
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();

    assert_eq!(resp["object"], "chat.completion");
    let content = resp["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(content.contains("12"), "Expected 12, got: {}", content);

    let reasoning = resp["choices"][0]["message"]["reasoning_content"]
        .as_str()
        .expect("Expected reasoning_content");
    assert!(!reasoning.is_empty());
    println!("Thinking: {}", reasoning);
}

// ============================================================
// Anthropic thinking — streaming
// ============================================================

#[tokio::test]
#[ignore]
async fn anthropic_thinking_stream() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();

    let req = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "What is 8 * 9?"}],
        "max_tokens": 4000,
        "reasoning_effort": "high",
    });

    let mut stream = llmshim::stream(&router, &req).await.unwrap();
    let mut full_text = String::new();
    let mut full_reasoning = String::new();
    let mut got_finish = false;
    let mut chunk_count = 0;

    while let Some(chunk) = stream.next().await {
        let data = chunk.unwrap();
        let parsed: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["object"], "chat.completion.chunk");

        if let Some(text) = parsed
            .pointer("/choices/0/delta/content")
            .and_then(|c| c.as_str())
        {
            full_text.push_str(text);
        }
        if let Some(reasoning) = parsed
            .pointer("/choices/0/delta/reasoning_content")
            .and_then(|c| c.as_str())
        {
            full_reasoning.push_str(reasoning);
        }
        if parsed["choices"][0]["finish_reason"].as_str().is_some() {
            got_finish = true;
        }
        chunk_count += 1;
    }

    println!(
        "Streamed reasoning ({} chars): {}...",
        full_reasoning.len(),
        &full_reasoning[..full_reasoning.len().min(100)]
    );
    println!("Streamed answer: {}", full_text);

    assert!(
        chunk_count > 3,
        "Should have multiple chunks, got {}",
        chunk_count
    );
    assert!(
        !full_reasoning.is_empty(),
        "Should have received reasoning chunks"
    );
    assert!(
        full_text.contains("72"),
        "Expected 72 in answer, got: {}",
        full_text
    );
    assert!(got_finish, "Should have received finish_reason");
}

// ============================================================
// Cross-provider: thinking response → OpenAI
// ============================================================

#[tokio::test]
#[ignore]
async fn thinking_response_then_switch_to_openai() {
    if skip_if_missing_keys() {
        return;
    }
    let router = router();

    // Turn 1: Anthropic with thinking (use enabled+budget to guarantee thinking)
    let req1 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "How many r's are in 'strawberry'? Think carefully step by step."}],
        "max_tokens": 4000,
        "thinking": {"type": "enabled", "budget_tokens": 2000},
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    println!(
        "Claude (with thinking): content={}, has_reasoning={}",
        msg1["content"],
        msg1.get("reasoning_content").is_some()
    );

    // Verify thinking was present
    assert!(
        msg1.get("reasoning_content").is_some(),
        "Should have reasoning_content"
    );

    // Turn 2: Send to OpenAI with Claude's thinking response in history
    // reasoning_content should be stripped so OpenAI doesn't choke
    let req2 = json!({
        "model": "openai/gpt-5.4",
        "messages": [
            {"role": "user", "content": "How many r's are in 'strawberry'?"},
            msg1,  // has reasoning_content that needs to be stripped
            {"role": "user", "content": "Now double that count. Just give the number."},
        ],
        "max_tokens": 200,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let content2 = resp2["choices"][0]["message"]["content"].as_str().unwrap();
    println!("OpenAI follow-up: {}", content2);
    assert!(
        content2.contains('6'),
        "Expected 6 (3*2), got: {}",
        content2
    );
}

// ============================================================
// Cross-provider: OpenAI → Anthropic with thinking
// ============================================================

#[tokio::test]
#[ignore]
async fn openai_then_anthropic_with_thinking() {
    if skip_if_missing_keys() {
        return;
    }
    let router = router();

    // Turn 1: OpenAI
    let req1 = json!({
        "model": "openai/gpt-5.4",
        "messages": [{"role": "user", "content": "What is 10 + 5? Just the number."}],
        "max_tokens": 100,
        "temperature": 0.0,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    println!("OpenAI: {}", msg1["content"]);

    // Turn 2: Anthropic with thinking, using OpenAI's response
    let req2 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": "What is 10 + 5? Just the number."},
            msg1,  // from OpenAI, has annotations/refusal that need stripping
            {"role": "user", "content": "Now multiply that by 3. Think about it carefully."},
        ],
        "max_tokens": 4000,
        "thinking": {"type": "enabled", "budget_tokens": 2000},
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let content2 = resp2["choices"][0]["message"]["content"].as_str().unwrap();
    let reasoning = resp2["choices"][0]["message"].get("reasoning_content");
    println!("Claude answer: {}", content2);
    println!(
        "Claude reasoning: {}",
        reasoning.and_then(|r| r.as_str()).unwrap_or("none")
    );

    assert!(content2.contains("45"), "Expected 45, got: {}", content2);
    assert!(reasoning.is_some(), "Should have reasoning from thinking");
}

// ============================================================
// Three-hop with thinking in the middle
// ============================================================

#[tokio::test]
#[ignore]
async fn three_hop_with_thinking_in_middle() {
    if skip_if_missing_keys() {
        return;
    }
    let router = router();

    // Turn 1: OpenAI
    let req1 = json!({
        "model": "openai/gpt-5.4",
        "messages": [{"role": "user", "content": "Pick a number between 1 and 5. Reply with just the number."}],
        "max_tokens": 100,
        "temperature": 1.0,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    let num1: String = msg1["content"]
        .as_str()
        .unwrap()
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    println!("Turn 1 (OpenAI): {}", num1);

    // Turn 2: Anthropic WITH thinking — square the number
    let req2 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": "Pick a number between 1 and 5. Reply with just the number."},
            msg1,
            {"role": "user", "content": "Square that number. Reply with just the result."},
        ],
        "max_tokens": 4000,
        "thinking": {"type": "enabled", "budget_tokens": 2000},
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let msg2 = resp2["choices"][0]["message"].clone();
    println!(
        "Turn 2 (Claude+thinking): content={}, reasoning={}",
        msg2["content"],
        msg2.get("reasoning_content").is_some()
    );

    // Verify thinking happened
    assert!(msg2.get("reasoning_content").is_some());

    // Turn 3: Back to OpenAI — should work despite reasoning_content in history
    let req3 = json!({
        "model": "openai/gpt-5.4",
        "messages": [
            {"role": "user", "content": "Pick a number between 1 and 5. Reply with just the number."},
            msg1,
            {"role": "user", "content": "Square that number. Reply with just the result."},
            msg2,  // has reasoning_content
            {"role": "user", "content": "Is that number a perfect square? Reply 'yes' or 'no'."},
        ],
        "max_tokens": 100,
        "temperature": 0.0,
    });
    let resp3 = llmshim::completion(&router, &req3).await.unwrap();
    let content3 = resp3["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    println!("Turn 3 (OpenAI): {}", content3);
    assert!(
        content3.contains("yes"),
        "A squared number is always a perfect square, got: {}",
        content3
    );
}

// ============================================================
// Reasoning round-trip (issue #34) — lossless thinking + tool-use continuation
// ============================================================

#[tokio::test]
#[ignore]
async fn anthropic_reasoning_roundtrip_accepted_live() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();
    let model = "anthropic/claude-sonnet-4-6";
    let tools = json!([{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get the current weather for a city",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }
        }
    }]);
    let user_turn = json!({"role": "user", "content": "What's the weather in Tokyo? Use the get_weather tool."});

    // Turn 1: thinking on + a tool available → expect thinking + a tool call.
    let req1 = json!({
        "model": model,
        "messages": [user_turn],
        "max_tokens": 4000,
        "reasoning_effort": "high",
        "tools": tools,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = &resp1["choices"][0]["message"];

    let tool_calls = msg1
        .get("tool_calls")
        .and_then(|t| t.as_array())
        .filter(|a| !a.is_empty());
    assert!(tool_calls.is_some(), "expected a tool call, got: {resp1}");
    assert!(
        msg1.get("reasoning_signature").is_some(),
        "expected reasoning_signature to be surfaced, got: {msg1}"
    );
    let call_id = tool_calls.unwrap()[0]["id"].as_str().unwrap().to_string();

    // Turn 2: echo the assistant message VERBATIM (reasoning_content +
    // reasoning_signature + tool_calls) and add the tool result. The shim must
    // reconstruct a valid thinking block first — the API must accept it (200).
    let req2 = json!({
        "model": model,
        "messages": [
            user_turn,
            msg1.clone(),
            {"role": "tool", "tool_call_id": call_id, "content": "72F and sunny"}
        ],
        "max_tokens": 4000,
        "reasoning_effort": "high",
        "tools": tools,
    });
    let resp2 = llmshim::completion(&router, &req2)
        .await
        .expect("reconstructed thinking block must be accepted by the API");
    assert_eq!(resp2["object"], "chat.completion");
    let content = resp2["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(!content.is_empty(), "expected a final answer, got: {resp2}");
    println!("Round-trip accepted. Final answer: {content}");
}

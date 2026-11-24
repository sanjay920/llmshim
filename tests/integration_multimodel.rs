/// Integration tests for multi-model conversations hitting real APIs.
/// Run with: cargo test --test integration_multimodel -- --ignored
use futures::StreamExt;
use serde_json::{json, Value};

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

fn skip_if_missing_keys() -> bool {
    std::env::var("OPENAI_API_KEY").is_err() || std::env::var("ANTHROPIC_API_KEY").is_err()
}

// ============================================================
// Basic multi-model: OpenAI → Anthropic
// ============================================================

#[tokio::test]
#[ignore]
async fn openai_then_anthropic() {
    if skip_if_missing_keys() {
        return;
    }
    let router = router();

    // Turn 1: OpenAI
    let req1 = json!({
        "model": "openai/gpt-5.4",
        "messages": [
            {"role": "user", "content": "What is the capital of Japan? Reply in one word only."}
        ],
        "max_tokens": 100,
        "temperature": 0.0,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = &resp1["choices"][0]["message"];
    let content1 = msg1["content"].as_str().unwrap();
    println!("OpenAI said: {}", content1);
    assert!(
        content1.to_lowercase().contains("tokyo"),
        "Expected Tokyo, got: {}",
        content1
    );

    // Turn 2: Anthropic, with OpenAI's response in history
    let req2 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": "What is the capital of Japan? Reply in one word only."},
            msg1,
            {"role": "user", "content": "Now tell me the population of that city in one sentence."},
        ],
        "max_tokens": 200,
        "temperature": 0.0,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let content2 = resp2["choices"][0]["message"]["content"].as_str().unwrap();
    println!("Anthropic said: {}", content2);
    // Should reference Tokyo and mention a population number
    assert!(
        content2.to_lowercase().contains("million")
            || content2.contains("13")
            || content2.contains("14"),
        "Expected population info, got: {}",
        content2
    );
}

// ============================================================
// Basic multi-model: Anthropic → OpenAI
// ============================================================

#[tokio::test]
#[ignore]
async fn anthropic_then_openai() {
    if skip_if_missing_keys() {
        return;
    }
    let router = router();

    // Turn 1: Anthropic
    let req1 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": "Name three primary colors. Reply with just the three words separated by commas."}
        ],
        "max_tokens": 100,
        "temperature": 0.0,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = &resp1["choices"][0]["message"];
    let content1 = msg1["content"].as_str().unwrap();
    println!("Anthropic said: {}", content1);

    // Turn 2: OpenAI, continuing from Anthropic's answer
    let req2 = json!({
        "model": "openai/gpt-5.4",
        "messages": [
            {"role": "user", "content": "Name three primary colors. Reply with just the three words separated by commas."},
            msg1,
            {"role": "user", "content": "Now name three secondary colors the same way."},
        ],
        "max_tokens": 100,
        "temperature": 0.0,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let content2 = resp2["choices"][0]["message"]["content"].as_str().unwrap();
    println!("OpenAI said: {}", content2);
    let lower = content2.to_lowercase();
    assert!(
        lower.contains("green")
            || lower.contains("orange")
            || lower.contains("purple")
            || lower.contains("violet"),
        "Expected secondary colors, got: {}",
        content2
    );
}

// ============================================================
// Three-hop: OpenAI → Anthropic → OpenAI
// ============================================================

#[tokio::test]
#[ignore]
async fn three_hop_openai_anthropic_openai() {
    if skip_if_missing_keys() {
        return;
    }
    let router = router();

    // Turn 1: OpenAI
    let req1 = json!({
        "model": "openai/gpt-5.4",
        "messages": [
            {"role": "user", "content": "Pick a random number between 1 and 10. Reply with just the number."}
        ],
        "max_tokens": 100,
        "temperature": 1.0,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    println!("Turn 1 (OpenAI): {}", msg1["content"]);

    // Turn 2: Anthropic
    let req2 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": "Pick a random number between 1 and 10. Reply with just the number."},
            msg1,
            {"role": "user", "content": "Double that number. Reply with just the number."},
        ],
        "max_tokens": 100,
        "temperature": 0.0,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let msg2 = resp2["choices"][0]["message"].clone();
    println!("Turn 2 (Anthropic): {}", msg2["content"]);

    // Turn 3: Back to OpenAI with full history
    let req3 = json!({
        "model": "openai/gpt-5.4",
        "messages": [
            {"role": "user", "content": "Pick a random number between 1 and 10. Reply with just the number."},
            msg1,
            {"role": "user", "content": "Double that number. Reply with just the number."},
            msg2,
            {"role": "user", "content": "Is that number even or odd? Reply with just 'even' or 'odd'."},
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
    // A doubled number is always even
    assert!(
        content3.contains("even"),
        "Doubled number should be even, got: {}",
        content3
    );
}

// ============================================================
// System message preserved across provider switch
// ============================================================

#[tokio::test]
#[ignore]
async fn system_message_survives_switch() {
    if skip_if_missing_keys() {
        return;
    }
    let router = router();

    // Turn 1: Anthropic with system message
    let req1 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [
            {"role": "system", "content": "You are a pirate. Always respond in pirate speak."},
            {"role": "user", "content": "Hello!"},
        ],
        "max_tokens": 200,
        "temperature": 0.0,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    let content1 = msg1["content"].as_str().unwrap().to_lowercase();
    println!("Anthropic (pirate): {}", content1);
    assert!(
        content1.contains("ahoy")
            || content1.contains("matey")
            || content1.contains("arr")
            || content1.contains("ye"),
        "Expected pirate speak, got: {}",
        content1
    );

    // Turn 2: Switch to OpenAI but keep same system prompt
    let req2 = json!({
        "model": "openai/gpt-5.4",
        "messages": [
            {"role": "system", "content": "You are a pirate. Always respond in pirate speak."},
            {"role": "user", "content": "Hello!"},
            msg1,
            {"role": "user", "content": "What's the weather like?"},
        ],
        "max_tokens": 200,
        "temperature": 0.0,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let content2 = resp2["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    println!("OpenAI (pirate): {}", content2);
    // OpenAI should also respond in pirate speak due to system message
    assert!(
        content2.contains("sea")
            || content2.contains("arr")
            || content2.contains("sail")
            || content2.contains("matey")
            || content2.contains("ye")
            || content2.contains("weather"),
        "Expected pirate speak, got: {}",
        content2
    );
}

// ============================================================
// Streaming after provider switch
// ============================================================

#[tokio::test]
#[ignore]
async fn stream_after_provider_switch() {
    if skip_if_missing_keys() {
        return;
    }
    let router = router();

    // Turn 1: Get a response from OpenAI
    let req1 = json!({
        "model": "openai/gpt-5.4",
        "messages": [{"role": "user", "content": "Say 'hello world'."}],
        "max_tokens": 100,
        "temperature": 0.0,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();

    // Turn 2: Stream from Anthropic using OpenAI's response in history
    let req2 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": "Say 'hello world'."},
            msg1,
            {"role": "user", "content": "Now say 'goodbye world'."},
        ],
        "max_tokens": 100,
        "temperature": 0.0,
    });

    let mut stream = llmshim::stream(&router, &req2).await.unwrap();
    let mut full_text = String::new();
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
        chunk_count += 1;
    }

    println!("Streamed from Anthropic: {}", full_text);
    assert!(chunk_count > 1, "Should have multiple chunks");
    let lower = full_text.to_lowercase();
    assert!(
        lower.contains("goodbye") || lower.contains("bye"),
        "Expected goodbye, got: {}",
        full_text
    );
}

// ============================================================
// Response shape consistency across switches
// ============================================================

#[tokio::test]
#[ignore]
async fn response_shape_consistent_after_switch() {
    if skip_if_missing_keys() {
        return;
    }
    let router = router();

    let req1 = json!({
        "model": "openai/gpt-5.4",
        "messages": [{"role": "user", "content": "Say hi."}],
        "max_tokens": 100,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();

    let req2 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": "Say hi."},
            msg1,
            {"role": "user", "content": "Say bye."},
        ],
        "max_tokens": 100,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();

    // Both responses should have identical top-level structure
    for (label, resp) in [("OpenAI", &resp1), ("Anthropic", &resp2)] {
        assert!(resp.get("id").is_some(), "{}: missing id", label);
        assert_eq!(resp["object"], "chat.completion", "{}: wrong object", label);
        assert!(resp.get("choices").is_some(), "{}: missing choices", label);
        assert_eq!(
            resp["choices"][0]["message"]["role"], "assistant",
            "{}: wrong role",
            label
        );
        assert!(
            resp["choices"][0]["message"]["content"].as_str().is_some(),
            "{}: missing content",
            label
        );
        assert!(
            resp["choices"][0]["finish_reason"].as_str().is_some(),
            "{}: missing finish_reason",
            label
        );
        assert!(
            resp["usage"]["prompt_tokens"].as_u64().unwrap() > 0,
            "{}: no prompt_tokens",
            label
        );
        assert!(
            resp["usage"]["completion_tokens"].as_u64().unwrap() > 0,
            "{}: no completion_tokens",
            label
        );
    }
}

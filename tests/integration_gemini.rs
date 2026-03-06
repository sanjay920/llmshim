/// Integration tests for Gemini provider hitting the real API.
/// Run with: cargo test --test integration_gemini -- --ignored
use futures::StreamExt;
use serde_json::{json, Value};

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

fn skip_if_no_key() -> bool {
    std::env::var("GEMINI_API_KEY").is_err()
}

fn skip_if_missing_all() -> bool {
    std::env::var("GEMINI_API_KEY").is_err()
        || std::env::var("OPENAI_API_KEY").is_err()
        || std::env::var("ANTHROPIC_API_KEY").is_err()
}

const FLASH: &str = "gemini/gemini-3-flash-preview";
const PRO: &str = "gemini/gemini-3.1-pro-preview";
const LITE: &str = "gemini/gemini-3.1-flash-lite-preview";

// ============================================================
// Basic completions — all 3 models
// ============================================================

#[tokio::test]
#[ignore]
async fn gemini_flash_completion() {
    if skip_if_no_key() {
        return;
    }
    let router = router();
    let req = json!({
        "model": FLASH,
        "messages": [{"role": "user", "content": "Reply with only the word 'pong'."}],
        "max_tokens": 200,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(resp["object"], "chat.completion");
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    println!("Flash: {}", content);
    assert!(content.contains("pong"), "Expected pong, got: {}", content);
    assert!(resp["usage"]["prompt_tokens"].as_u64().unwrap() > 0);
    assert!(resp["usage"]["completion_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
#[ignore]
async fn gemini_pro_completion() {
    if skip_if_no_key() {
        return;
    }
    let router = router();
    let req = json!({
        "model": PRO,
        "messages": [{"role": "user", "content": "Reply with only the word 'pong'."}],
        "max_tokens": 200,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(resp["object"], "chat.completion");
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    println!("Pro: {}", content);
    assert!(content.contains("pong"), "Expected pong, got: {}", content);
}

#[tokio::test]
#[ignore]
async fn gemini_flash_lite_completion() {
    if skip_if_no_key() {
        return;
    }
    let router = router();
    let req = json!({
        "model": LITE,
        "messages": [{"role": "user", "content": "Reply with only the word 'pong'."}],
        "max_tokens": 200,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(resp["object"], "chat.completion");
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    println!("Flash-Lite: {}", content);
    assert!(content.contains("pong"), "Expected pong, got: {}", content);
}

// ============================================================
// System message
// ============================================================

#[tokio::test]
#[ignore]
async fn gemini_system_message() {
    if skip_if_no_key() {
        return;
    }
    let router = router();
    let req = json!({
        "model": FLASH,
        "messages": [
            {"role": "system", "content": "Always respond in exactly one word."},
            {"role": "user", "content": "What color is the sky?"},
        ],
        "max_tokens": 200,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = resp["choices"][0]["message"]["content"].as_str().unwrap();
    println!("System msg test: {}", content);
    assert!(
        content.split_whitespace().count() <= 3,
        "Expected ~1 word, got: {}",
        content
    );
}

// ============================================================
// Streaming
// ============================================================

#[tokio::test]
#[ignore]
async fn gemini_stream() {
    if skip_if_no_key() {
        return;
    }
    let router = router();
    let req = json!({
        "model": FLASH,
        "messages": [{"role": "user", "content": "Count from 1 to 3."}],
        "max_tokens": 200,
    });
    let mut stream = llmshim::stream(&router, &req).await.unwrap();
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

    println!("Streamed ({} chunks): {}", chunk_count, full_text);
    assert!(chunk_count >= 1, "Should have at least 1 chunk");
    assert!(
        full_text.contains('1') && full_text.contains('2') && full_text.contains('3'),
        "Expected 1,2,3 in: {}",
        full_text
    );
}

// ============================================================
// Auto-inferred provider
// ============================================================

#[tokio::test]
#[ignore]
async fn gemini_inferred_provider() {
    if skip_if_no_key() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [{"role": "user", "content": "Say ok."}],
        "max_tokens": 200,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(resp["object"], "chat.completion");
}

// ============================================================
// Reasoning effort → thinkingLevel
// ============================================================

#[tokio::test]
#[ignore]
async fn gemini_reasoning_effort() {
    if skip_if_no_key() {
        return;
    }
    let router = router();
    let req = json!({
        "model": FLASH,
        "messages": [{"role": "user", "content": "What is 15 * 37?"}],
        "max_tokens": 500,
        "reasoning_effort": "high",
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = resp["choices"][0]["message"]["content"].as_str().unwrap();
    println!("Reasoning test: {}", content);
    assert!(content.contains("555"), "Expected 555, got: {}", content);
}

// ============================================================
// Cross-provider: Gemini ↔ OpenAI ↔ Anthropic
// ============================================================

#[tokio::test]
#[ignore]
async fn gemini_then_openai() {
    if skip_if_missing_all() {
        return;
    }
    let router = router();

    // Turn 1: Gemini
    let req1 = json!({
        "model": FLASH,
        "messages": [{"role": "user", "content": "What is the capital of France? One word only."}],
        "max_tokens": 200,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    println!("Gemini: {}", msg1["content"]);

    // Turn 2: OpenAI with Gemini history
    let req2 = json!({
        "model": "openai/gpt-5.4",
        "messages": [
            {"role": "user", "content": "What is the capital of France? One word only."},
            msg1,
            {"role": "user", "content": "What country is that city in? One word only."},
        ],
        "max_tokens": 100,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let content2 = resp2["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    println!("OpenAI: {}", content2);
    assert!(
        content2.contains("france"),
        "Expected France, got: {}",
        content2
    );
}

#[tokio::test]
#[ignore]
async fn openai_then_gemini() {
    if skip_if_missing_all() {
        return;
    }
    let router = router();

    // Turn 1: OpenAI
    let req1 = json!({
        "model": "openai/gpt-5.4",
        "messages": [{"role": "user", "content": "Say 'hello world'. Nothing else."}],
        "max_tokens": 100,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    println!("OpenAI: {}", msg1["content"]);

    // Turn 2: Gemini with OpenAI history (has annotations/refusal)
    let req2 = json!({
        "model": FLASH,
        "messages": [
            {"role": "user", "content": "Say 'hello world'. Nothing else."},
            msg1,
            {"role": "user", "content": "Now say 'goodbye world'. Nothing else."},
        ],
        "max_tokens": 200,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let content2 = resp2["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    println!("Gemini: {}", content2);
    assert!(
        content2.contains("goodbye"),
        "Expected goodbye, got: {}",
        content2
    );
}

#[tokio::test]
#[ignore]
async fn anthropic_then_gemini() {
    if skip_if_missing_all() {
        return;
    }
    let router = router();

    // Turn 1: Anthropic
    let req1 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "What is 2+2? Just the number."}],
        "max_tokens": 100,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    println!("Anthropic: {}", msg1["content"]);

    // Turn 2: Gemini
    let req2 = json!({
        "model": FLASH,
        "messages": [
            {"role": "user", "content": "What is 2+2? Just the number."},
            msg1,
            {"role": "user", "content": "Double that. Just the number."},
        ],
        "max_tokens": 200,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let content2 = resp2["choices"][0]["message"]["content"].as_str().unwrap();
    println!("Gemini: {}", content2);
    assert!(content2.contains('8'), "Expected 8, got: {}", content2);
}

// ============================================================
// Three-provider hop: OpenAI → Gemini → Anthropic
// ============================================================

#[tokio::test]
#[ignore]
async fn three_provider_hop() {
    if skip_if_missing_all() {
        return;
    }
    let router = router();

    // Turn 1: OpenAI
    let req1 = json!({
        "model": "openai/gpt-5.4",
        "messages": [{"role": "user", "content": "Pick a number between 1 and 5. Just the number."}],
        "max_tokens": 100,
        "temperature": 1.0,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    println!("Turn 1 (OpenAI): {}", msg1["content"]);

    // Turn 2: Gemini
    let req2 = json!({
        "model": FLASH,
        "messages": [
            {"role": "user", "content": "Pick a number between 1 and 5. Just the number."},
            msg1,
            {"role": "user", "content": "Add 10 to that number. Just the number."},
        ],
        "max_tokens": 200,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let msg2 = resp2["choices"][0]["message"].clone();
    println!("Turn 2 (Gemini): {}", msg2["content"]);

    // Turn 3: Anthropic
    let req3 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": "Pick a number between 1 and 5. Just the number."},
            msg1,
            {"role": "user", "content": "Add 10 to that number. Just the number."},
            msg2,
            {"role": "user", "content": "Is that number between 11 and 15? Reply 'yes' or 'no'."},
        ],
        "max_tokens": 100,
    });
    let resp3 = llmshim::completion(&router, &req3).await.unwrap();
    let content3 = resp3["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    println!("Turn 3 (Anthropic): {}", content3);
    assert!(content3.contains("yes"), "Expected yes, got: {}", content3);
}

// ============================================================
// Response shape consistency
// ============================================================

#[tokio::test]
#[ignore]
async fn all_three_providers_same_shape() {
    if skip_if_missing_all() {
        return;
    }
    let router = router();

    let models = ["openai/gpt-5.4", "anthropic/claude-sonnet-4-6", FLASH];

    for model in models {
        let req = json!({
            "model": model,
            "messages": [{"role": "user", "content": "Say hello."}],
            "max_tokens": 200,
        });
        let resp = llmshim::completion(&router, &req).await.unwrap();

        assert!(resp.get("id").is_some(), "{}: missing id", model);
        assert_eq!(resp["object"], "chat.completion", "{}: wrong object", model);
        assert_eq!(
            resp["choices"][0]["message"]["role"], "assistant",
            "{}: wrong role",
            model
        );
        assert!(
            resp["choices"][0]["message"]["content"].as_str().is_some(),
            "{}: missing content",
            model
        );
        assert!(
            resp["choices"][0]["finish_reason"].as_str().is_some(),
            "{}: missing finish_reason",
            model
        );
        assert!(
            resp["usage"]["prompt_tokens"].as_u64().unwrap() > 0,
            "{}: no prompt_tokens",
            model
        );
        assert!(
            resp["usage"]["completion_tokens"].as_u64().unwrap() > 0,
            "{}: no completion_tokens",
            model
        );
    }
}

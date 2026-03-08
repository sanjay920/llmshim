/// Integration tests for long context (>200K tokens) requiring the 1M beta header.
/// Run with: cargo test --test integration_long_context -- --ignored
use serde_json::{json, Value};

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

/// Generate a payload that exceeds the default 200K token context window.
/// Uses ~250K tokens worth of structured text (~1M chars).
fn make_large_payload(model: &str) -> Value {
    let mut lines = Vec::with_capacity(25000);
    for i in 0..25000 {
        lines.push(format!(
            "Data point {}: measurement={:.4}, status=active, category=alpha",
            i,
            i as f64 * 3.14159
        ));
    }
    let big_text = lines.join("\n");

    json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": format!(
                "{}\n\nWhat is the measurement value on data point 24999? Reply with just the number.",
                big_text
            )
        }],
        "max_tokens": 100,
    })
}

// ============================================================
// Anthropic — 1M context beta (enabled by default)
// ============================================================

#[tokio::test]
#[ignore]
async fn anthropic_large_context_succeeds() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();
    let request = make_large_payload("anthropic/claude-sonnet-4-6");

    println!("Sending ~250K token request to Claude Sonnet 4.6 (1M beta on by default)...");
    let start = std::time::Instant::now();

    let resp = llmshim::completion(&router, &request).await.unwrap();
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("<none>");
    let input_tok = resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0);

    println!(
        "  SUCCESS in {:.1}s — {} input tokens — content: {}",
        start.elapsed().as_secs_f32(),
        input_tok,
        content
    );

    // Should have used more than 200K tokens
    assert!(
        input_tok > 200_000,
        "Expected >200K input tokens, got {}",
        input_tok
    );
    // Should contain the expected measurement value (24999 * 3.14159 ≈ 78536.61)
    assert!(
        content.contains("78536") || content.contains("78537"),
        "Expected measurement ~78536-78537, got: {}",
        content
    );
}

#[tokio::test]
#[ignore]
async fn anthropic_large_context_fails_when_disabled() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();
    let mut request = make_large_payload("anthropic/claude-sonnet-4-6");

    // Disable the 1M context beta
    request["x-anthropic"] = json!({"disable_1m_context": true});

    println!("Sending ~250K token request WITHOUT 1M beta header...");
    let result = llmshim::completion(&router, &request).await;

    // Should fail because we exceed the default 200K window
    assert!(
        result.is_err(),
        "Expected error without 1M context header, but got success"
    );
    let err = result.unwrap_err();
    let err_str = format!("{}", err);
    println!("  Expected error: {}", &err_str[..err_str.len().min(200)]);
}

// ============================================================
// Gemini — also supports 1M+ context natively
// ============================================================

#[tokio::test]
#[ignore]
async fn gemini_large_context_succeeds() {
    if std::env::var("GEMINI_API_KEY").is_err() {
        return;
    }
    let router = router();
    let request = make_large_payload("gemini/gemini-3-flash-preview");

    println!("Sending ~250K token request to Gemini 3 Flash...");
    let start = std::time::Instant::now();

    let resp = llmshim::completion(&router, &request).await.unwrap();
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("<none>");
    let input_tok = resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0);

    println!(
        "  SUCCESS in {:.1}s — {} input tokens — content: {}",
        start.elapsed().as_secs_f32(),
        input_tok,
        content
    );

    assert!(
        input_tok > 100_000,
        "Expected substantial input tokens, got {}",
        input_tok
    );
}

// ============================================================
// OpenAI — gpt-5.4 has 1M+ context
// ============================================================

#[tokio::test]
#[ignore]
async fn openai_large_context_succeeds() {
    if std::env::var("OPENAI_API_KEY").is_err() {
        return;
    }
    let router = router();
    // Use a smaller payload for OpenAI to avoid rate limits / cost
    let mut lines = Vec::with_capacity(10000);
    for i in 0..10000 {
        lines.push(format!("Entry {}: value={:.2}", i, i as f64 * 2.71828));
    }
    let text = lines.join("\n");

    let request = json!({
        "model": "openai/gpt-5.4",
        "messages": [{
            "role": "user",
            "content": format!(
                "{}\n\nWhat is the value for entry 9999? Reply with just the number.",
                text
            )
        }],
        "max_tokens": 200,
    });

    println!("Sending large context request to GPT-5.4...");
    let start = std::time::Instant::now();

    let resp = llmshim::completion(&router, &request).await.unwrap();
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("<none>");

    println!(
        "  SUCCESS in {:.1}s — content: {}",
        start.elapsed().as_secs_f32(),
        content
    );

    // 9999 * 2.71828 ≈ 27180.08
    assert!(
        content.contains("27180"),
        "Expected ~27180, got: {}",
        content
    );
}

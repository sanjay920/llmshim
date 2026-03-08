#![cfg(feature = "proxy")]

/// Integration tests for the proxy server hitting real APIs.
/// Run with: cargo test --features proxy --test integration_proxy -- --ignored
use serde_json::{json, Value};

fn skip_if_no_keys() -> bool {
    std::env::var("OPENAI_API_KEY").is_err()
        || std::env::var("ANTHROPIC_API_KEY").is_err()
        || std::env::var("GEMINI_API_KEY").is_err()
}

async fn start_server() -> String {
    let router = llmshim::router::Router::from_env();
    let app = llmshim::proxy::app(router, None);

    // Bind to random port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{}", addr);

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    base_url
}

// ============================================================
// Health endpoint
// ============================================================

#[tokio::test]
#[ignore]
async fn health_endpoint() {
    let base = start_server().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .get(format!("{}/health", base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["status"], "ok");
    assert!(resp["providers"].as_array().unwrap().len() > 0);
}

// ============================================================
// Models endpoint
// ============================================================

#[tokio::test]
#[ignore]
async fn models_endpoint() {
    let base = start_server().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .get(format!("{}/v1/models", base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let models = resp["models"].as_array().unwrap();
    assert!(models.len() > 0);

    // Each model should have id, provider, name
    for model in models {
        assert!(model["id"].as_str().is_some());
        assert!(model["provider"].as_str().is_some());
        assert!(model["name"].as_str().is_some());
    }
}

// ============================================================
// Non-streaming chat
// ============================================================

#[tokio::test]
#[ignore]
async fn chat_anthropic() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let base = start_server().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .post(format!("{}/v1/chat", base))
        .json(&json!({
            "model": "anthropic/claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "Say 'pong'. Nothing else."}],
            "config": {"max_tokens": 100}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["provider"], "anthropic");
    assert!(resp["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("pong"));
    assert!(resp["usage"]["input_tokens"].as_u64().unwrap() > 0);
    assert!(resp["latency_ms"].as_u64().unwrap() > 0);
}

#[tokio::test]
#[ignore]
async fn chat_openai() {
    if std::env::var("OPENAI_API_KEY").is_err() {
        return;
    }
    let base = start_server().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .post(format!("{}/v1/chat", base))
        .json(&json!({
            "model": "openai/gpt-5.4",
            "messages": [{"role": "user", "content": "Say 'pong'. Nothing else."}],
            "config": {"max_tokens": 100}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["provider"], "openai");
    assert!(resp["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("pong"));
}

#[tokio::test]
#[ignore]
async fn chat_gemini() {
    if std::env::var("GEMINI_API_KEY").is_err() {
        return;
    }
    let base = start_server().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .post(format!("{}/v1/chat", base))
        .json(&json!({
            "model": "gemini/gemini-3-flash-preview",
            "messages": [{"role": "user", "content": "Say 'pong'. Nothing else."}],
            "config": {"max_tokens": 200}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["provider"], "gemini");
    assert!(resp["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("pong"));
}

// ============================================================
// Provider config passthrough
// ============================================================

#[tokio::test]
#[ignore]
async fn chat_with_provider_config() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let base = start_server().await;
    let client = reqwest::Client::new();

    let resp: Value = client
        .post(format!("{}/v1/chat", base))
        .json(&json!({
            "model": "anthropic/claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "What is 5+3?"}],
            "config": {"max_tokens": 4000},
            "provider_config": {
                "thinking": {"type": "enabled", "budget_tokens": 2000}
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert!(resp["message"]["content"].as_str().unwrap().contains("8"));
    // Should have reasoning from thinking
    assert!(resp.get("reasoning").is_some());
}

// ============================================================
// Streaming
// ============================================================

#[tokio::test]
#[ignore]
async fn stream_anthropic() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let base = start_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/chat/stream", base))
        .json(&json!({
            "model": "anthropic/claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "Count from 1 to 3."}],
            "config": {"max_tokens": 200}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    // Should contain typed events
    assert!(text.contains("event: content"), "Missing content events");
    assert!(text.contains("event: done"), "Missing done event");

    // Parse content events
    let mut full_text = String::new();
    for line in text.lines() {
        if line.starts_with("data: ") {
            let data = &line[6..];
            if let Ok(parsed) = serde_json::from_str::<Value>(data) {
                if parsed["type"] == "content" {
                    full_text.push_str(parsed["text"].as_str().unwrap_or(""));
                }
            }
        }
    }
    assert!(
        full_text.contains('1') && full_text.contains('2') && full_text.contains('3'),
        "Expected 1,2,3 in: {}",
        full_text
    );
}

// ============================================================
// Cross-provider consistency
// ============================================================

#[tokio::test]
#[ignore]
async fn response_shape_consistent_across_providers() {
    if skip_if_no_keys() {
        return;
    }
    let base = start_server().await;
    let client = reqwest::Client::new();

    let models = [
        "openai/gpt-5.4",
        "anthropic/claude-sonnet-4-6",
        "gemini/gemini-3-flash-preview",
    ];

    for model in models {
        let resp: Value = client
            .post(format!("{}/v1/chat", base))
            .json(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "Say hello."}],
                "config": {"max_tokens": 200}
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        // Verify our response shape (NOT OpenAI shape)
        assert!(resp.get("id").is_some(), "{}: missing id", model);
        assert!(
            resp.get("provider").is_some(),
            "{}: missing provider",
            model
        );
        assert!(resp.get("model").is_some(), "{}: missing model", model);
        assert!(resp.get("message").is_some(), "{}: missing message", model);
        assert_eq!(
            resp["message"]["role"], "assistant",
            "{}: wrong role",
            model
        );
        assert!(
            resp["message"]["content"].as_str().is_some(),
            "{}: missing content",
            model
        );
        assert!(resp.get("usage").is_some(), "{}: missing usage", model);
        assert!(
            resp["usage"]["input_tokens"].as_u64().unwrap() > 0,
            "{}: no input_tokens",
            model
        );
        assert!(
            resp.get("latency_ms").is_some(),
            "{}: missing latency_ms",
            model
        );
    }
}

// ============================================================
// Error handling
// ============================================================

#[tokio::test]
#[ignore]
async fn chat_missing_model_returns_400() {
    let base = start_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/chat", base))
        .json(&json!({
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 422); // axum returns 422 for missing required fields
}

#[tokio::test]
#[ignore]
async fn chat_unknown_provider_returns_400() {
    let base = start_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/chat", base))
        .json(&json!({
            "model": "unknown/model-xyz",
            "messages": [{"role": "user", "content": "hi"}],
            "config": {"max_tokens": 100}
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unknown_provider");
}

// ============================================================
// Auto-inferred provider
// ============================================================

#[tokio::test]
#[ignore]
async fn chat_auto_inferred_model() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let base = start_server().await;
    let client = reqwest::Client::new();

    // No provider prefix — should auto-detect anthropic
    let resp: Value = client
        .post(format!("{}/v1/chat", base))
        .json(&json!({
            "model": "claude-sonnet-4-6",
            "messages": [{"role": "user", "content": "Say ok."}],
            "config": {"max_tokens": 100}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["provider"], "anthropic");
}

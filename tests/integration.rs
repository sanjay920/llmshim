/// Integration tests that hit real provider APIs.
/// Requires OPENAI_API_KEY and ANTHROPIC_API_KEY env vars.
/// Run with: cargo test --test integration -- --ignored
use futures::StreamExt;
use serde_json::{json, Value};

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

fn skip_if_no_key(var: &str) -> bool {
    std::env::var(var).is_err()
}

// ============================================================
// OpenAI integration (Responses API)
// ============================================================

#[tokio::test]
#[ignore]
async fn openai_completion_basic() {
    if skip_if_no_key("OPENAI_API_KEY") {
        return;
    }
    let router = router();
    let req = json!({
        "model": "openai/gpt-5.4",
        "messages": [{"role": "user", "content": "Reply with only the word 'pong'."}],
        "max_tokens": 100,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();

    assert_eq!(resp["object"], "chat.completion");
    assert!(resp["id"].as_str().unwrap().starts_with("resp_"));
    assert_eq!(resp["choices"][0]["message"]["role"], "assistant");
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    assert!(
        content.contains("pong"),
        "Expected 'pong', got: {}",
        content
    );
    assert!(resp["choices"][0]["finish_reason"].as_str().is_some());
    assert!(resp["usage"]["prompt_tokens"].as_u64().unwrap() > 0);
    assert!(resp["usage"]["completion_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
#[ignore]
async fn openai_completion_with_system() {
    if skip_if_no_key("OPENAI_API_KEY") {
        return;
    }
    let router = router();
    let req = json!({
        "model": "openai/gpt-5.4",
        "messages": [
            {"role": "system", "content": "Always respond in exactly one word."},
            {"role": "user", "content": "What color is the sky?"},
        ],
        "max_tokens": 100,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = resp["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        content.split_whitespace().count() <= 3,
        "Expected ~1 word, got: {}",
        content
    );
}

#[tokio::test]
#[ignore]
async fn openai_completion_inferred_provider() {
    if skip_if_no_key("OPENAI_API_KEY") {
        return;
    }
    let router = router();
    let req = json!({
        "model": "gpt-5.4",
        "messages": [{"role": "user", "content": "Say 'ok'."}],
        "max_tokens": 50,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(resp["object"], "chat.completion");
}

#[tokio::test]
#[ignore]
async fn openai_stream_basic() {
    if skip_if_no_key("OPENAI_API_KEY") {
        return;
    }
    let router = router();
    let req = json!({
        "model": "openai/gpt-5.4",
        "messages": [{"role": "user", "content": "Count from 1 to 3."}],
        "max_tokens": 200,
    });
    let mut stream = llmshim::stream(&router, &req).await.unwrap();
    let mut chunks = Vec::new();
    let mut full_text = String::new();

    while let Some(chunk) = stream.next().await {
        let data = chunk.unwrap();
        let parsed: Value = serde_json::from_str(&data).unwrap();
        if let Some(text) = parsed
            .pointer("/choices/0/delta/content")
            .and_then(|c| c.as_str())
        {
            full_text.push_str(text);
        }
        chunks.push(parsed);
    }

    assert!(
        chunks.len() > 1,
        "Should have multiple chunks, got {}",
        chunks.len()
    );
    assert!(
        full_text.contains('1') && full_text.contains('2') && full_text.contains('3'),
        "Expected 1,2,3 in: {}",
        full_text
    );
}

// ============================================================
// Anthropic integration
// ============================================================

#[tokio::test]
#[ignore]
async fn anthropic_completion_basic() {
    if skip_if_no_key("ANTHROPIC_API_KEY") {
        return;
    }
    let router = router();
    let req = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "Reply with only the word 'pong'."}],
        "max_tokens": 100,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();

    assert_eq!(resp["object"], "chat.completion");
    assert!(resp["id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(resp["choices"][0]["message"]["role"], "assistant");
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase();
    assert!(
        content.contains("pong"),
        "Expected 'pong', got: {}",
        content
    );
    assert_eq!(resp["choices"][0]["finish_reason"], "stop");
    assert!(resp["usage"]["prompt_tokens"].as_u64().unwrap() > 0);
    assert!(resp["usage"]["completion_tokens"].as_u64().unwrap() > 0);
    assert!(resp["usage"]["total_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
#[ignore]
async fn anthropic_completion_with_system() {
    if skip_if_no_key("ANTHROPIC_API_KEY") {
        return;
    }
    let router = router();
    let req = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [
            {"role": "system", "content": "Always respond in exactly one word."},
            {"role": "user", "content": "What color is the sky?"},
        ],
        "max_tokens": 100,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = resp["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        content.split_whitespace().count() <= 3,
        "Expected ~1 word, got: {}",
        content
    );
}

#[tokio::test]
#[ignore]
async fn anthropic_completion_inferred_provider() {
    if skip_if_no_key("ANTHROPIC_API_KEY") {
        return;
    }
    let router = router();
    let req = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "Say 'ok'."}],
        "max_tokens": 50,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(resp["object"], "chat.completion");
}

#[tokio::test]
#[ignore]
async fn anthropic_stream_basic() {
    if skip_if_no_key("ANTHROPIC_API_KEY") {
        return;
    }
    let router = router();
    let req = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "Count from 1 to 3."}],
        "max_tokens": 200,
    });
    let mut stream = llmshim::stream(&router, &req).await.unwrap();
    let mut chunks = Vec::new();
    let mut full_text = String::new();

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
        chunks.push(parsed);
    }

    assert!(chunks.len() > 1, "Should have multiple chunks");
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
async fn both_providers_return_same_shape() {
    if skip_if_no_key("OPENAI_API_KEY") || skip_if_no_key("ANTHROPIC_API_KEY") {
        return;
    }
    let router = router();

    let models = ["openai/gpt-5.4", "anthropic/claude-sonnet-4-6"];
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

#[tokio::test]
#[ignore]
async fn both_providers_stream_same_shape() {
    if skip_if_no_key("OPENAI_API_KEY") || skip_if_no_key("ANTHROPIC_API_KEY") {
        return;
    }
    let router = router();

    let models = ["openai/gpt-5.4", "anthropic/claude-sonnet-4-6"];
    for model in models {
        let req = json!({
            "model": model,
            "messages": [{"role": "user", "content": "Say hello."}],
            "max_tokens": 200,
        });
        let mut stream = llmshim::stream(&router, &req).await.unwrap();
        let mut got_content = false;
        let mut got_finish = false;

        while let Some(chunk) = stream.next().await {
            let data = chunk.unwrap();
            let parsed: Value = serde_json::from_str(&data).unwrap();
            assert_eq!(
                parsed["object"], "chat.completion.chunk",
                "Wrong object for {}",
                model
            );

            if parsed
                .pointer("/choices/0/delta/content")
                .and_then(|c| c.as_str())
                .is_some()
            {
                got_content = true;
            }
            if parsed["choices"][0]["finish_reason"].as_str().is_some() {
                got_finish = true;
            }
        }

        assert!(got_content, "No content for {}", model);
        assert!(got_finish, "No finish_reason for {}", model);
    }
}

// ============================================================
// Alias
// ============================================================

#[tokio::test]
#[ignore]
async fn alias_works_end_to_end() {
    if skip_if_no_key("OPENAI_API_KEY") {
        return;
    }
    let router = router().alias("fast", "openai/gpt-5.4");
    let req = json!({
        "model": "fast",
        "messages": [{"role": "user", "content": "Say 'ok'."}],
        "max_tokens": 50,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(resp["object"], "chat.completion");
}

// ============================================================
// Error handling
// ============================================================

#[tokio::test]
#[ignore]
async fn bad_model_name_returns_provider_error() {
    if skip_if_no_key("OPENAI_API_KEY") {
        return;
    }
    let router = router();
    let req = json!({
        "model": "openai/nonexistent-model-xyz",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 50,
    });
    let err = llmshim::completion(&router, &req).await.unwrap_err();
    assert!(matches!(
        err,
        llmshim::error::ShimError::ProviderError { .. }
    ));
}

#[tokio::test]
#[ignore]
async fn missing_model_field_errors() {
    let router = router();
    let req = json!({"messages": [{"role": "user", "content": "hi"}]});
    let err = llmshim::completion(&router, &req).await.unwrap_err();
    assert!(matches!(err, llmshim::error::ShimError::MissingModel));
}

#[tokio::test]
#[ignore]
async fn unregistered_provider_errors() {
    let router = router();
    let req = json!({
        "model": "groq/llama-3-70b",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let err = llmshim::completion(&router, &req).await.unwrap_err();
    assert!(matches!(err, llmshim::error::ShimError::UnknownProvider(_)));
}

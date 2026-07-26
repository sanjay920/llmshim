/// Integration tests for OpenRouter hitting the real API.
/// Run with: OPENROUTER_API_KEY=... cargo test --test integration_openrouter -- --ignored
/// Override the chat model with OR_TEST_MODEL. The default is a `:free` model so
/// the suite runs on an account without purchased credits — but free models are
/// rate-limited, so add `--test-threads=1` to avoid parallel free-tier 429s.
use serde_json::{json, Value};

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

fn test_model() -> String {
    std::env::var("OR_TEST_MODEL")
        .unwrap_or_else(|_| "openrouter/poolside/laguna-s-2.1:free".into())
}

/// Run a completion, or skip the test (returning None) when OpenRouter reports
/// the account can't pay for the model (402) — so the suite passes on both free
/// and funded accounts.
async fn complete_or_skip(router: &llmshim::router::Router, req: &Value) -> Option<Value> {
    match llmshim::completion(router, req).await {
        Ok(v) => Some(v),
        Err(e) => {
            let s = e.to_string();
            if s.contains("402") || s.contains("Insufficient credits") {
                eprintln!("SKIP: OpenRouter account lacks credits for this model ({s})");
                None
            } else {
                panic!("completion failed: {e}");
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn openrouter_basic_completion() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": test_model(),
        "messages": [{"role": "user", "content": "In one short sentence, what is Rust?"}],
        "max_tokens": 2000,
    });
    let Some(resp) = complete_or_skip(&router, &req).await else {
        return;
    };
    assert_eq!(resp["object"], "chat.completion");
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(!content.is_empty(), "expected a response, got: {resp}");
    println!("model={} | said: {content}", resp["model"]);
}

#[tokio::test]
#[ignore]
async fn openrouter_reasoning() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": test_model(),
        "messages": [{"role": "user", "content": "In one sentence, why is the sky blue? Reason briefly first."}],
        "max_tokens": 2000,
        "reasoning_effort": "high",
    });
    let Some(resp) = complete_or_skip(&router, &req).await else {
        return;
    };
    let msg = &resp["choices"][0]["message"];
    // A reasoning model's reasoning is normalized into reasoning_content.
    let reasoning = msg["reasoning_content"].as_str().unwrap_or("");
    let content = msg["content"].as_str().unwrap_or("");
    assert!(
        !reasoning.is_empty() || !content.is_empty(),
        "expected reasoning or answer, got: {resp}"
    );
    println!(
        "reasoning chars={}, answer chars={}",
        reasoning.len(),
        content.len()
    );
}

#[tokio::test]
#[ignore]
async fn openrouter_tool_call() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return;
    }
    let router = router();
    // A widely-available tool-capable model; skipped on accounts without credits.
    let model =
        std::env::var("OR_TOOL_MODEL").unwrap_or_else(|_| "openrouter/openai/gpt-4o-mini".into());
    let req = json!({
        "model": model,
        "messages": [{"role": "user", "content": "What's the weather in Tokyo? Use the tool."}],
        "max_tokens": 200,
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get the weather for a city",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}
            }
        }]
    });
    let Some(resp) = complete_or_skip(&router, &req).await else {
        return;
    };
    let tool_calls = resp["choices"][0]["message"].get("tool_calls");
    assert!(tool_calls.is_some(), "expected a tool call, got: {resp}");
    println!("tool call ok");
}

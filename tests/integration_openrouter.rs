/// Integration tests for OpenRouter hitting the real API.
/// Run with: OPENROUTER_API_KEY=... cargo test --test integration_openrouter -- --ignored
use serde_json::json;

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

#[tokio::test]
#[ignore]
async fn openrouter_basic_completion() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "openrouter/openai/gpt-4o-mini",
        "messages": [{"role": "user", "content": "Reply with exactly: openrouter ok"}],
        "max_tokens": 50,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(resp["object"], "chat.completion");
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(!content.is_empty(), "expected a response, got: {resp}");
    println!("openrouter said: {content}");
}

#[tokio::test]
#[ignore]
async fn openrouter_tool_call() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "openrouter/openai/gpt-4o-mini",
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
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let tool_calls = resp["choices"][0]["message"].get("tool_calls");
    assert!(tool_calls.is_some(), "expected a tool call, got: {resp}");
    println!("openrouter tool call ok");
}

#[tokio::test]
#[ignore]
async fn openrouter_reasoning() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "openrouter/anthropic/claude-sonnet-4.5",
        "messages": [{"role": "user", "content": "Prove there are infinitely many primes. Reason it through."}],
        "max_tokens": 3000,
        "reasoning_effort": "high",
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(!content.is_empty(), "expected an answer, got: {resp}");
    println!("openrouter reasoning ok");
}

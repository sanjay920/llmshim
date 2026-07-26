/// Integration tests for a self-hosted SGLang (or any OpenAI-compatible) server.
/// Point SGLANG_BASE_URL at a running server and run:
///   SGLANG_BASE_URL=http://host:30000/v1 SGLANG_TEST_MODEL=<served-name> \
///     cargo test --test integration_sglang -- --ignored --nocapture
/// SGLANG_API_KEY is optional (only if the server was launched with --api-key).
use serde_json::json;

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

fn model() -> Option<String> {
    let m = std::env::var("SGLANG_TEST_MODEL").ok()?;
    Some(format!("sglang/{m}"))
}

#[tokio::test]
#[ignore]
async fn sglang_basic_completion() {
    if std::env::var("SGLANG_BASE_URL").is_err() {
        return;
    }
    let Some(model) = model() else {
        eprintln!("set SGLANG_TEST_MODEL");
        return;
    };
    let router = router();
    let req = json!({
        "model": model,
        "messages": [{"role": "user", "content": "In one short sentence, what is Rust?"}],
        "max_tokens": 512,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(resp["object"], "chat.completion");
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(!content.is_empty(), "expected a response, got: {resp}");
    println!("model={} | said: {content}", resp["model"]);
}

#[tokio::test]
#[ignore]
async fn sglang_reasoning() {
    if std::env::var("SGLANG_BASE_URL").is_err() {
        return;
    }
    let Some(model) = model() else { return };
    let router = router();
    let req = json!({
        "model": model,
        "messages": [{"role": "user", "content": "What is 17 * 24? Reason step by step, then give the answer."}],
        "max_tokens": 2000,
        "reasoning_effort": "high",
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let msg = &resp["choices"][0]["message"];
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
async fn sglang_tool_call() {
    if std::env::var("SGLANG_BASE_URL").is_err() {
        return;
    }
    let Some(model) = model() else { return };
    let router = router();
    let req = json!({
        "model": model,
        "messages": [{"role": "user", "content": "What's the weather in Tokyo? Use the get_weather tool."}],
        "max_tokens": 512,
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
    println!(
        "tool call ok: {}",
        tool_calls.unwrap()[0]["function"]["name"]
    );
}

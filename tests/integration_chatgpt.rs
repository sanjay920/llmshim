//! Run after `llmshim login chatgpt`:
//! cargo test --test integration_chatgpt -- --ignored
use futures::StreamExt;
use serde_json::{json, Value};

#[tokio::test]
#[ignore = "requires an explicit ChatGPT login and consumes subscription usage"]
async fn chatgpt_completion_and_stream() {
    let router = llmshim::router::Router::from_env();
    router
        .get("chatgpt")
        .expect("run `llmshim login chatgpt` first");
    let model =
        std::env::var("CHATGPT_TEST_MODEL").unwrap_or_else(|_| "chatgpt/gpt-6-astra".into());
    let request =
        json!({"model": model, "messages": [{"role": "user", "content": "Reply with only pong."}]});
    let response = llmshim::completion(&router, &request).await.unwrap();
    assert!(response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("pong"));
    let mut stream = llmshim::stream(&router, &request).await.unwrap();
    let mut text = String::new();
    let mut completed = false;
    while let Some(chunk) = stream.next().await {
        let chunk: Value = serde_json::from_str(&chunk.unwrap()).unwrap();
        if let Some(delta) = chunk["choices"][0]["delta"]["content"].as_str() {
            text.push_str(delta);
        }
        completed |= chunk["choices"][0]["finish_reason"] == "stop";
    }
    assert!(text.to_lowercase().contains("pong"));
    assert!(completed);
}

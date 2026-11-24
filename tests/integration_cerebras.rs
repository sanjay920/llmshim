//! Live, billed Cerebras regression checks. Run only with CEREBRAS_API_KEY set:
//! cargo test --features proxy --test integration_cerebras -- --ignored --nocapture --test-threads=1
use futures::StreamExt;
use llmshim::{credentials::StoredCredentials, router::Router};
use serde_json::{json, Value};
const MODEL: &str = "cerebras/qwen-3.8-27b";

struct Key(String);
impl StoredCredentials for Key {
    fn secret_for(&self, provider: &str) -> Option<String> {
        (provider == "cerebras").then(|| self.0.clone())
    }
}
fn router() -> Router {
    let key = std::env::var("CEREBRAS_API_KEY").expect("CEREBRAS_API_KEY required for live tests");
    assert!(!key.trim().is_empty());
    Router::from_scoped_credentials(&Key(key))
}
fn request(prompt: &str) -> Value {
    json!({"model":MODEL,"messages":[{"role":"user","content":prompt}],
        "reasoning_effort":"none","max_tokens":1024})
}

#[tokio::test]
#[ignore = "billed Cerebras API call"]
async fn cerebras_completion_and_structured_output() {
    let router = router();
    let body = llmshim::completion(&router, &request("Reply with exactly: ready"))
        .await
        .unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .trim(),
        "ready"
    );
    assert!(body["usage"]["prompt_tokens"].as_u64().unwrap() > 0);
    let mut req = request("Return a JSON object with answer equal to 42.");
    req["response_format"] = json!({"type":"json_schema","json_schema":{"name":"answer",
        "strict":true,"schema":{"type":"object","properties":{"answer":{"type":"integer"}},
        "required":["answer"],"additionalProperties":false}}});
    let body = llmshim::completion(&router, &req).await.unwrap();
    let answer: Value =
        serde_json::from_str(body["choices"][0]["message"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(answer["answer"], 42);
    println!("Cerebras completion, token usage, and JSON schema: passed");
}

#[tokio::test]
#[ignore = "billed Cerebras API calls"]
async fn cerebras_reasoning_and_replay() {
    let router = router();
    let mut req = request("What is 17 times 19? Check the arithmetic briefly.");
    req["reasoning_effort"] = json!("low");
    let body = llmshim::completion(&router, &req).await.unwrap();
    let message = body["choices"][0]["message"].clone();
    assert!(
        !message["reasoning"].as_array().unwrap().is_empty(),
        "{body}"
    );
    assert_eq!(message["reasoning"][0]["origin"]["provider"], "cerebras");
    req["messages"].as_array_mut().unwrap().extend([
        message,
        json!({"role":"user","content":"Reply with only that number."}),
    ]);
    let body = llmshim::completion(&router, &req).await.unwrap();
    assert!(body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .contains("323"));
    println!("Cerebras reasoning provenance and multi-turn replay: passed");
}

#[tokio::test]
#[ignore = "billed Cerebras API call"]
async fn cerebras_stream_reasoning_and_usage() {
    let router = router();
    let mut req = request("What is 23 times 7? Check briefly, then give the number.");
    req["reasoning_effort"] = json!("low");
    req["stream_options"] = json!({"include_usage":true});
    let mut stream = llmshim::stream(&router, &req).await.unwrap();
    let mut text = String::new();
    let mut reasoning = false;
    let mut usage = false;
    while let Some(chunk) = stream.next().await {
        let body: Value = serde_json::from_str(&chunk.unwrap()).unwrap();
        if let Some(value) = body["choices"][0]["delta"]["content"].as_str() {
            text.push_str(value);
        }
        reasoning |= body["choices"][0]["delta"]["reasoning"]
            .as_array()
            .is_some_and(|r| !r.is_empty());
        usage |= body["usage"]["completion_tokens"]
            .as_u64()
            .is_some_and(|t| t > 0);
    }
    assert!(text.contains("161"), "{text}");
    assert!(reasoning, "missing streamed reasoning");
    assert!(usage, "missing terminal token usage");
    println!("Cerebras streamed text, reasoning, and usage: passed");
}

#[tokio::test]
#[ignore = "billed Cerebras API calls"]
async fn cerebras_tool_round_trip() {
    let router = router();
    let mut req = request("Call get_weather for Tokyo.");
    req["tools"] = json!([{"type":"function","function":{"name":"get_weather",
        "description":"Get weather for a city","parameters":{"type":"object",
        "properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}}}]);
    req["tool_choice"] = json!({"type":"function","function":{"name":"get_weather"}});
    let body = llmshim::completion(&router, &req).await.unwrap();
    let message = body["choices"][0]["message"].clone();
    let call = &message["tool_calls"][0];
    assert_eq!(call["function"]["name"], "get_weather");
    let args: Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Tokyo");
    let id = call["id"].clone();
    req["messages"].as_array_mut().unwrap().extend([
        message,
        json!({"role":"tool","tool_call_id":id,"content":"Sunny, 25 C"}),
    ]);
    req["tool_choice"] = json!("none");
    let body = llmshim::completion(&router, &req).await.unwrap();
    assert!(!body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .is_empty());
    println!("Cerebras named tool choice, arguments, and tool result replay: passed");
}

#[cfg(feature = "proxy")]
#[tokio::test]
#[ignore = "billed Cerebras API call"]
async fn cerebras_proxy_chat_completions() {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    let app = llmshim::proxy::app(router(), None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(request("Reply with exactly: ready").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .trim(),
        "ready"
    );
    println!("Cerebras through the native proxy Chat Completions facade: passed");
}

#[tokio::test]
#[ignore = "billed Cerebras API call"]
async fn cerebras_stream_tool_call() {
    let router = router();
    let mut req = request("Use get_weather for Tokyo.");
    req["tools"] = json!([{"type":"function","function":{"name":"get_weather",
        "description":"Get weather for a city","parameters":{"type":"object",
        "properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}}}]);
    req["tool_choice"] = json!({"type":"function","function":{"name":"get_weather"}});
    let mut stream = llmshim::stream(&router, &req).await.unwrap();
    let mut calls = Vec::new();
    while let Some(chunk) = stream.next().await {
        let body: Value = serde_json::from_str(&chunk.unwrap()).unwrap();
        if let Some(complete) = body["choices"][0]["delta"]["tool_calls"].as_array() {
            calls.extend(complete.clone());
        }
    }
    let call = calls
        .iter()
        .find(|call| call["function"]["name"] == "get_weather")
        .expect("complete streamed call");
    let args: Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Tokyo");
    assert!(!call["id"].as_str().unwrap().is_empty());
    println!("Cerebras streamed tool identity and complete arguments: passed");
}

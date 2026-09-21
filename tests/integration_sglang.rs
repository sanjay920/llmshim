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
    let reasoning = msg["reasoning"][0]["text"].as_str().unwrap_or("");
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

/// The Responses wire, two turns, against a live server: turn 1 thinks and
/// calls a tool, turn 2 sends the thinking back as the item the server issued
/// and gets an answer. Needs `SGLANG_WIRE=responses`, and — because replay is
/// gated on a known model family — a catalog override declaring the served
/// model's family, e.g. `LLMSHIM_CATALOG_PROJECT=tests/fixtures/sglang-models.toml`
/// when the served model is the fixture's. Prints every body it sends and
/// receives so a `--nocapture` run is its own record.
#[tokio::test]
#[ignore]
async fn sglang_responses_wire_replays_reasoning_across_a_tool_round() {
    if std::env::var("SGLANG_BASE_URL").is_err()
        || std::env::var("SGLANG_WIRE").as_deref() != Ok("responses")
    {
        eprintln!("set SGLANG_BASE_URL and SGLANG_WIRE=responses");
        return;
    }
    let Some(model) = model() else {
        eprintln!("set SGLANG_TEST_MODEL");
        return;
    };
    let router = router();
    let (provider, served) = router.resolve(&model).unwrap();
    println!("replay target: {:?}", provider.replay_target(&served));

    let tools = json!([{"type": "function", "function": {"name": "get_weather",
        "description": "Current weather for a city",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}}]);
    let mut req = json!({"model": model, "tools": tools, "max_tokens": 400,
        "messages": [{"role": "user", "content": "What is the weather in Paris right now? Use the tool."}]});
    let sent = provider.transform_request(&served, &req).unwrap();
    println!("turn 1 -> {}\n{}", sent.url, sent.body);
    let turn1 = llmshim::completion(&router, &req).await.unwrap();
    println!("turn 1 <- {turn1}");
    let message = turn1["choices"][0]["message"].clone();
    let item_id = message["reasoning"][0]["item_id"]
        .as_str()
        .expect("reasoning came back as an item with an id")
        .to_string();
    assert!(item_id.starts_with("rs_"), "{item_id}");
    let call_id = message["tool_calls"][0]["id"]
        .as_str()
        .expect("the model called the tool")
        .to_string();

    req["messages"].as_array_mut().unwrap().extend([
        message,
        json!({"role": "tool", "tool_call_id": call_id,
            "content": "{\"city\":\"Paris\",\"temp_c\":14,\"sky\":\"overcast\"}"}),
    ]);
    let sent = provider.transform_request(&served, &req).unwrap();
    println!("turn 2 -> {}\n{}", sent.url, sent.body);
    let replayed = sent.body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("the reasoning item is in the second request");
    assert_eq!(replayed["id"], item_id);
    let turn2 = llmshim::completion(&router, &req).await.unwrap();
    println!("turn 2 <- {turn2}");
    let answer = turn2["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(
        answer.contains("14"),
        "the answer uses the tool result: {answer}"
    );
}

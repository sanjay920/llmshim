//! Live server-mode smoke test using the CLI binary and saved ChatGPT OAuth.
//! cargo test --features proxy --test integration_chatgpt_proxy -- --ignored --nocapture
#![cfg(feature = "proxy")]

use base64::Engine;
use reqwest::Client;
use serde_json::{json, Value};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

struct ProxyProcess(Child);

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn post(client: &Client, base: &str, path: &str, request: &Value) -> reqwest::Response {
    let response = client
        .post(format!("{base}{path}"))
        .json(request)
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{} {}",
        request["model"],
        response.status()
    );
    response
}

fn events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|line| serde_json::from_str(line.trim()).unwrap())
        .collect()
}

fn assert_complete(events: &[Value]) {
    assert!(
        events.iter().all(|event| event["type"] != "error"),
        "{events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "done")
            .count(),
        1,
        "{events:?}"
    );
    assert!(
        events.iter().any(
            |event| event["type"] == "usage" && event["total_tokens"].as_u64().unwrap_or(0) > 0
        ),
        "{events:?}"
    );
}

#[tokio::test]
#[ignore = "requires a saved ChatGPT login and consumes subscription usage"]
async fn chatgpt_server_mode() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = ProxyProcess(
        Command::new(env!("CARGO_BIN_EXE_llmshim"))
            .arg("proxy")
            .env("LLMSHIM_HOST", "127.0.0.1")
            .env("LLMSHIM_PORT", port.to_string())
            // Register API-key OpenAI too, without using a real key. This catches
            // accidental re-inference of a ChatGPT response as API-key OpenAI.
            .env("OPENAI_API_KEY", "server-smoke-no-api-key")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(180))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let mut healthy = false;
    for _ in 0..100 {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "proxy exited before becoming healthy"
        );
        if let Ok(response) = client.get(format!("{base}/health")).send().await {
            let health: Value = response.json().await.unwrap();
            assert_eq!(health["status"], "ok");
            assert!(
                health["providers"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("chatgpt")),
                "run `llmshim login chatgpt` first"
            );
            healthy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(healthy, "proxy did not become healthy");
    let models: Value = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let actual: Vec<_> = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["provider"] == "chatgpt")
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    let expected = [
        "chatgpt/gpt-6-astra",
        "chatgpt/gpt-5.6-sol",
        "chatgpt/gpt-5.6-terra",
        "chatgpt/gpt-5.6-luna",
    ];
    assert_eq!(actual, expected);
    println!("PASS /health and /v1/models (exactly four ChatGPT models)");

    for model in expected {
        let request = json!({"model": model, "messages": [{"role": "user", "content": "Reply with only pong."}],
            "config": {"reasoning_effort": "low", "max_tokens": 10}});
        let response: Value = post(&client, &base, "/v1/chat", &request)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(response["provider"], "chatgpt", "{response}");
        assert_eq!(response["model"], model, "{response}");
        assert!(
            response["message"]["content"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("pong"),
            "{response}"
        );
        assert!(response["usage"]["total_tokens"].as_u64().unwrap() > 0);
        println!("PASS {model} POST /v1/chat");
        let response = post(&client, &base, "/v1/chat/stream", &request).await;
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"));
        let chunks = events(&response.text().await.unwrap());
        assert_complete(&chunks);
        let text: String = chunks
            .iter()
            .filter(|c| c["type"] == "content")
            .filter_map(|c| c["text"].as_str())
            .collect();
        assert!(text.to_lowercase().contains("pong"), "{chunks:?}");
        println!("PASS {model} POST /v1/chat/stream");
    }

    let invalid = client
        .post(format!("{base}/v1/chat"))
        .json(&json!({"model": "chatgpt/gpt-5.4", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    println!("PASS older ChatGPT model rejected with HTTP 400");

    let mut tool_request = json!({"model": "chatgpt/gpt-6-astra", "messages": [{"role": "user", "content": "Use lookup_weather for Paris. After receiving the result, repeat the returned condition."}],
    "config": {"reasoning_effort": "max"}, "provider_config": {
        "tools": [{"type": "function", "function": {"name": "lookup_weather", "description": "Look up weather for a city.",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}}}],
        "tool_choice": {"type": "function", "function": {"name": "lookup_weather"}}
    }});
    let response: Value = post(&client, &base, "/v1/chat", &tool_request)
        .await
        .json()
        .await
        .unwrap();
    let call = response["message"]["tool_calls"][0].clone();
    assert_eq!(call["function"]["name"], "lookup_weather", "{response}");
    let args: Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "Paris");
    println!("PASS Astra max reasoning and forced function call");
    let mut streamed_tool_request = tool_request.clone();
    streamed_tool_request["config"]["reasoning_effort"] = json!("low");
    let chunks = events(
        &post(&client, &base, "/v1/chat/stream", &streamed_tool_request)
            .await
            .text()
            .await
            .unwrap(),
    );
    assert_complete(&chunks);
    let tool = chunks
        .iter()
        .find(|c| c["type"] == "tool_call")
        .expect("missing streamed tool call");
    assert_eq!(tool["name"], "lookup_weather");
    let args: Value = serde_json::from_str(tool["arguments"].as_str().unwrap())
        .expect("streamed function arguments must be complete JSON");
    assert_eq!(args["city"], "Paris");
    println!("PASS streamed function call with complete arguments");
    let messages = tool_request["messages"].as_array_mut().unwrap();
    messages.push(response["message"].clone());
    messages.push(json!({"role": "tool", "tool_call_id": call["id"], "content": "Sunny"}));
    tool_request["config"]["reasoning_effort"] = json!("low");
    tool_request["provider_config"]["tool_choice"] = json!("none");
    tool_request["stream"] = json!(true);
    let chunks = events(
        &post(&client, &base, "/v1/chat", &tool_request)
            .await
            .text()
            .await
            .unwrap(),
    );
    assert_complete(&chunks);
    let text: String = chunks
        .iter()
        .filter(|c| c["type"] == "content")
        .filter_map(|c| c["text"].as_str())
        .collect();
    assert!(text.to_lowercase().contains("sunny"), "{chunks:?}");
    println!("PASS tool-result round trip through POST /v1/chat with stream:true");
    // A normal-sized 256x256 fixture: the backend accepted a 32x32 red image
    // but misclassified it as beige during live testing.
    let image = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("fixtures/chatgpt-red.png"))
    );
    let request = json!({"model": "chatgpt/gpt-6-astra", "config": {"reasoning_effort": "low"},
    "messages": [{"role": "user", "content": [
        {"type": "text", "text": "What is the dominant color in this image? Answer with one color word."},
        {"type": "image_url", "image_url": {"url": image}}
    ]}]});
    let response: Value = post(&client, &base, "/v1/chat", &request)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        response["message"]["content"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("red"),
        "{response}"
    );
    println!("PASS Astra image input through server mode");
}

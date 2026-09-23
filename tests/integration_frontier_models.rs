//! Live current-model checks. Run ignored tests explicitly; they incur provider usage.
use futures::StreamExt;
use llmshim::router::Router;
use serde_json::{json, Value};
use std::time::Duration;

fn frontier_request(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with only pong."}],
        "reasoning_effort": "low",
        "max_tokens": 1024,
    })
}

fn assert_pong(response: &Value, model: &str) {
    assert_eq!(response["choices"][0]["finish_reason"], "stop", "{model}");
    assert!(
        response["choices"][0]["message"]["content"]
            .as_str()
            .is_some_and(|content| content.to_lowercase().contains("pong")),
        "{model}"
    );
}

#[tokio::test]
#[ignore = "requires OpenAI and Anthropic API keys and incurs usage charges"]
async fn new_frontier_models_complete_and_stream() {
    llmshim::env::load_all();
    let router = Router::from_env_without_catalog_refresh();
    for model in [
        "openai/gpt-6-sol",
        "openai/gpt-6-luna",
        "anthropic/claude-opus-5-5",
    ] {
        tokio::time::timeout(Duration::from_secs(120), async {
            let request = frontier_request(model);
            let response = llmshim::completion(&router, &request).await.unwrap();
            assert_pong(&response, model);
            let mut stream = llmshim::stream(&router, &request).await.unwrap();
            let mut streamed_text = String::new();
            let mut finished = false;
            while let Some(frame) = stream.next().await {
                let chunk: Value = serde_json::from_str(&frame.unwrap()).unwrap();
                if let Some(delta) = chunk["choices"][0]["delta"]["content"].as_str() {
                    streamed_text.push_str(delta);
                }
                finished |= chunk["choices"][0]["finish_reason"] == "stop";
            }
            assert!(streamed_text.to_lowercase().contains("pong"), "{model}");
            assert!(finished, "{model} stream has no terminal finish reason");
            println!("PASS {model} completion and streaming");
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires an Anthropic API key and incurs usage charges"]
async fn opus_5_5_signed_tool_round_trip() {
    llmshim::env::load_all();
    let router = Router::from_env_without_catalog_refresh();
    tokio::time::timeout(Duration::from_secs(120), async {
        let mut request = json!({
            "model": "anthropic/claude-opus-5-5",
            "messages": [
                {"role": "system", "content": "Use lookup_weather for weather requests. After its result, reply with only the condition."},
                {"role": "user", "content": "First work out whether 1739 times 2843 is divisible by 7. Then call lookup_weather for Paris. Do not answer before receiving the tool result."}
            ],
            "tools": [{"type": "function", "function": {
                "name": "lookup_weather",
                "description": "Look up weather in a city",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}
            }}],
            "tool_choice": "auto",
            "reasoning_effort": "high",
            "max_tokens": 2048,
        });
        let first_response = llmshim::completion(&router, &request).await.unwrap();
        let assistant_message = first_response["choices"][0]["message"].clone();
        let tool_call = assistant_message["tool_calls"][0].clone();
        assert_eq!(tool_call["function"]["name"], "lookup_weather");
        assert_eq!(
            serde_json::from_str::<Value>(tool_call["function"]["arguments"].as_str().unwrap()).unwrap()["city"],
            "Paris"
        );
        assert!(assistant_message["reasoning"].as_array().is_some_and(|blocks| !blocks.is_empty()));
        request["messages"].as_array_mut().unwrap().push(assistant_message);
        request["messages"].as_array_mut().unwrap().push(json!({
            "role": "tool", "tool_call_id": tool_call["id"], "content": "Sunny"
        }));
        request["tool_choice"] = json!("none");
        let second_response = llmshim::completion(&router, &request).await.unwrap();
        assert!(second_response["choices"][0]["message"]["content"]
            .as_str()
            .is_some_and(|content| content.to_lowercase().contains("sunny")));
        println!("PASS Opus 5.5 signed tool-result round trip");
    })
    .await
    .unwrap();
}

#[cfg(feature = "proxy")]
#[tokio::test]
#[ignore = "requires OpenAI and Anthropic API keys and incurs usage charges"]
async fn current_frontier_models_work_on_real_proxy_routes() {
    use llmshim::providers::chatgpt::ChatGpt;

    llmshim::env::load_all();
    let router = Router::from_env_without_catalog_refresh()
        .register("chatgpt", Box::new(ChatGpt::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, llmshim::proxy::app(router, None))
            .await
            .unwrap();
    });
    struct AbortServer(tokio::task::AbortHandle);
    impl Drop for AbortServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server_guard = AbortServer(server.abort_handle());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let models: Value = client
        .get(format!("{base_url}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let advertised: Vec<_> = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    for model in [
        "openai/gpt-6-sol",
        "openai/gpt-6-luna",
        "anthropic/claude-opus-5-5",
        "chatgpt/gpt-6-sol",
        "chatgpt/gpt-6-luna",
    ] {
        assert!(advertised.contains(&model), "missing {model}");
    }
    for old_model in [
        "openai/gpt-5.6-sol",
        "openai/gpt-5.6-terra",
        "openai/gpt-5.6-luna",
        "anthropic/claude-opus-5",
        "chatgpt/gpt-5.6-sol",
    ] {
        assert!(
            !advertised.contains(&old_model),
            "still advertising {old_model}"
        );
    }
    for model in [
        "openai/gpt-6-sol",
        "openai/gpt-6-luna",
        "anthropic/claude-opus-5-5",
    ] {
        let response = client
            .post(format!("{base_url}/v1/chat"))
            .json(&frontier_request(model))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{model}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["provider"], model.split_once('/').unwrap().0);
        assert!(body["message"]["content"]
            .as_str()
            .is_some_and(|content| content.to_lowercase().contains("pong")));
        println!("PASS {model} real proxy request");
    }
}

#[tokio::test]
#[ignore = "requires a saved ChatGPT login and consumes subscription usage"]
async fn chatgpt_gpt_6_sol_and_luna_complete_and_stream() {
    llmshim::env::load_all();
    let router = Router::from_env_without_catalog_refresh();
    for model in ["chatgpt/gpt-6-sol", "chatgpt/gpt-6-luna"] {
        tokio::time::timeout(Duration::from_secs(120), async {
            let request = frontier_request(model);
            let response = llmshim::completion(&router, &request).await.unwrap();
            assert_eq!(response["model"], model);
            assert_pong(&response, model);
            let mut stream = llmshim::stream(&router, &request).await.unwrap();
            let mut streamed_text = String::new();
            while let Some(frame) = stream.next().await {
                let chunk: Value = serde_json::from_str(&frame.unwrap()).unwrap();
                assert_eq!(chunk["model"], model);
                if let Some(delta) = chunk["choices"][0]["delta"]["content"].as_str() {
                    streamed_text.push_str(delta);
                }
            }
            assert!(streamed_text.to_lowercase().contains("pong"));
            println!("PASS {model} subscription completion and streaming");
        })
        .await
        .unwrap();
    }
}

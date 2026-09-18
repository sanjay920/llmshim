//! Live checks for the current provider models and Fable conversation binding.
//! cargo test --features proxy --test integration_current_models -- --ignored --nocapture
use futures::StreamExt;
use serde_json::{json, Value};

#[tokio::test]
#[ignore = "requires provider API keys and incurs usage charges"]
async fn current_models_completion_and_streaming() {
    llmshim::env::load_all();
    let router = llmshim::router::Router::from_env();
    for model in [
        "gemini/gemini-3.8-flash",
        "anthropic/claude-opus-5",
        "anthropic/claude-fable-5",
        "anthropic/claude-fable-5-1",
        "xai/grok-4.6",
    ] {
        let input = json!({"model": model, "messages": [{"role": "user", "content": "Reply with only pong."}],
            "max_tokens": 512, "reasoning_effort": "none"});
        let result = llmshim::completion(&router, &input).await.unwrap();
        assert_eq!(result["choices"][0]["finish_reason"], "stop", "{model}");
        assert!(result["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("pong"));
        println!("PASS {model} completion (reasoning_effort:none)");
        let mut stream = llmshim::stream(&router, &input).await.unwrap();
        let mut text = String::new();
        let mut finished = false;
        while let Some(chunk) = stream.next().await {
            let chunk: Value = serde_json::from_str(&chunk.unwrap()).unwrap();
            if let Some(delta) = chunk["choices"][0]["delta"]["content"].as_str() {
                text.push_str(delta);
            }
            finished |= chunk["choices"][0]["finish_reason"] == "stop";
        }
        assert!(text.to_lowercase().contains("pong"), "{model}: {text}");
        assert!(finished, "{model} lacks a completion event");
        println!("PASS {model} streaming");
    }
}

#[tokio::test]
#[ignore = "requires provider API keys and incurs usage charges"]
async fn fable_tool_roundtrips_and_history_binding() {
    llmshim::env::load_all();
    let router = llmshim::router::Router::from_env();
    for model in ["anthropic/claude-fable-5", "anthropic/claude-fable-5-1"] {
        let mut input = json!({"model": model, "max_tokens": 2048,
            "messages": [{"role": "system", "content": "Use the weather tool when asked about weather. Reply briefly."},
                {"role": "user", "content": "Call lookup_weather for Paris. After receiving the result, repeat the condition."}],
            "tools": [{"type": "function", "function": {"name": "lookup_weather", "description": "Look up weather for a city.",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}}}],
            "tool_choice": if model.ends_with("5-1") { "auto" } else { "required" },
            "reasoning_effort": "low"
        });
        if model.ends_with("5-1") {
            input["messages"][1]["content"] = json!("Work out whether 1739 times 2843 is divisible by 7, then call lookup_weather for Paris. After receiving the tool result, reply with the weather condition only.");
            input["x-anthropic"] = json!({
                "thinking": {"type": "adaptive", "display": "summarized", "block_binding": {"prefix_mismatch_behavior": "error"}},
                "output_config": {"effort": "high"}, "extra_betas": ["thinking-binding-controls-2026-08-01"]
            });
        }
        let result = llmshim::completion(&router, &input).await.unwrap();
        let assistant = result["choices"][0]["message"].clone();
        let call = assistant["tool_calls"][0].clone();
        assert_eq!(call["function"]["name"], "lookup_weather");
        assert_eq!(
            serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap()
                ["city"],
            "Paris"
        );
        if model.ends_with("5-1") {
            assert!(
                assistant["reasoning"][0]["signature"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                    || assistant["reasoning"][0]["data"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty()),
                "missing thinking block on {model}; response fields: {:?}",
                assistant.as_object().unwrap().keys().collect::<Vec<_>>()
            );
        }
        input["messages"].as_array_mut().unwrap().push(assistant);
        input["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "tool", "tool_call_id": call["id"], "content": "Sunny"}));
        input["tool_choice"] = json!("none");
        if model.ends_with("5-1") {
            input["messages"].as_array_mut().unwrap().push(json!({"role": "system", "content": "For this turn, reply with only the weather condition."}));
        }
        let reply = llmshim::completion(&router, &input).await.unwrap();
        assert!(reply["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("sunny"));
        println!("PASS {model} tool-result round trip");
        if model.ends_with("5-1") {
            input["messages"][0]["content"] =
                json!("Changed initial instructions, invalidating signed thinking.");
            let error = llmshim::completion(&router, &input).await.unwrap_err();
            assert!(matches!(
                error,
                llmshim::error::ShimError::ProviderError { status: 400, .. }
            ));
            assert!(error.to_string().contains("signature") || error.to_string().contains("bound"));
            println!("PASS Fable 5.1 binding enforcement rejects an edited prefix");
        }
    }
}

#[cfg(feature = "proxy")]
#[tokio::test]
#[ignore = "requires provider API keys and incurs usage charges"]
async fn current_models_proxy_routes() {
    llmshim::env::load_all();
    let router = llmshim::router::Router::from_env();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, llmshim::proxy::app(router, None))
            .await
            .unwrap();
    });
    // Abort the task even if an assertion fails.
    struct ServerGuard(tokio::task::AbortHandle);
    impl Drop for ServerGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _guard = ServerGuard(server.abort_handle());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap();
    let catalog: Value = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for model in [
        "gemini/gemini-3.8-flash",
        "anthropic/claude-opus-5",
        "anthropic/claude-fable-5",
        "anthropic/claude-fable-5-1",
        "xai/grok-4.6",
    ] {
        let advertised = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == model);
        // Fable 5 remains callable by explicit ID but is superseded in discovery.
        assert_eq!(advertised, model != "anthropic/claude-fable-5");
        let response = client.post(format!("{base}/v1/chat")).json(&json!({"model": model,
            "messages": [{"role": "user", "content": "Reply with only pong."}],
            "config": {"reasoning_effort": "none", "max_tokens": 512, "temperature": 0.5, "top_p": 0.8, "top_k": 10}}))
            .send().await.unwrap();
        assert_eq!(response.status(), 200, "{model}");
        let response: Value = response.json().await.unwrap();
        assert_eq!(response["provider"], model.split_once('/').unwrap().0);
        assert!(response["message"]["content"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("pong"));
        println!("PASS {model} real HTTP proxy request (advertised: {advertised})");
    }
}

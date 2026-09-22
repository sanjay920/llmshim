//! Live launch contract and public API checks. Uses configured keys and bills usage.
//! LLMSHIM_CATALOG_OFFLINE=1 cargo test --features proxy --test integration_grok_4_7 -- --ignored --nocapture
use futures::StreamExt;
use llmshim::{providers::xai::Xai, reasoning::ReasoningAccumulator, router::Router};
use serde_json::{json, Value};

const MODEL: &str = "xai/grok-4.7";

fn key(name: &str) -> String {
    let config = llmshim::config::load();
    let configured = match name {
        "XAI_API_KEY" => config.keys.xai,
        "OPENROUTER_API_KEY" => config.keys.openrouter,
        _ => None,
    };
    std::env::var(name)
        .ok()
        .or(configured)
        .unwrap_or_else(|| panic!("set {name} or configure its key"))
}
fn router() -> Router {
    Router::new().register("xai", Box::new(Xai::new(key("XAI_API_KEY"))))
}
fn request() -> Value {
    json!({"model":MODEL,"messages":[{"role":"user","content":"Reply with only pong."}],"max_tokens":1024,"reasoning_effort":"none"})
}
fn encrypted(message: &Value) -> &Value {
    message["reasoning"]
        .as_array()
        .expect("reasoning array")
        .iter()
        .find(|b| b["kind"] == "encrypted")
        .expect("encrypted reasoning block")
}

#[tokio::test]
#[ignore = "requires xAI key; sends over 200k input tokens and costs roughly one dollar"]
async fn long_context_estimate_matches_native_billing() {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .unwrap();
    let input = format!(
        "Ignore the repeated filler and reply with only pong.\n{}\nReply with only pong.",
        " a".repeat(205_000)
    );
    let response = client.post("https://api.x.ai/v1/responses").bearer_auth(key("XAI_API_KEY"))
        .json(&json!({"model":"grok-4.7","input":input,"reasoning":{"effort":"low"},"store":false,"max_output_tokens":512})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["status"], "completed");
    let native = &body["usage"];
    assert!(native["input_tokens"].as_u64().unwrap() > 200_000);
    let mut usage =
        json!({"prompt_tokens":native["input_tokens"],"completion_tokens":native["output_tokens"]});
    llmshim::usage::normalize_cache(native, &mut usage);
    let estimate = llmshim::cost::cost_usd("xai", "grok-4.7", &usage).unwrap();
    // xAI reports USD in 1e-10 dollar ticks, including the context tier.
    let reported = native["cost_in_usd_ticks"].as_f64().unwrap() / 10_000_000_000.0;
    assert!(
        (estimate - reported).abs() < 0.000001,
        "estimate {estimate} differs from native {reported}"
    );
    println!(
        "PASS {} input tokens: catalog estimate ${estimate:.6}, native ${reported:.6}",
        native["input_tokens"]
    );
}

#[tokio::test]
#[ignore = "requires xAI key and incurs usage charges"]
async fn native_none_rejected_and_ciphertext_returned_without_include() {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap();
    for effort in ["none", "low", "xhigh"] {
        let response = client.post("https://api.x.ai/v1/responses").bearer_auth(key("XAI_API_KEY"))
            .json(&json!({"model":"grok-4.7","input":"Reply with only pong.","reasoning":{"effort":effort},"store":false,"max_output_tokens":1024}))
            .send().await.unwrap();
        assert_eq!(
            response.status().as_u16(),
            if effort == "none" { 400 } else { 200 }
        );
        if effort != "none" {
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["status"], "completed");
            assert!(body["output"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["encrypted_content"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())));
        }
        println!("PASS native effort {effort}");
    }
}

#[tokio::test]
#[ignore = "requires xAI key and incurs usage charges"]
async fn completion_streaming_tools_and_encrypted_replay() {
    let router = router();
    for streaming in [false, true] {
        let mut req = request();
        req["messages"][0]["content"] =
            json!("Call lookup_weather for Paris, then report the tool result in one word.");
        req["tools"] = json!([{"type":"function","function":{"name":"lookup_weather","description":"Get a city's weather.","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}}}]);
        req["tool_choice"] = json!({"type":"function","function":{"name":"lookup_weather"}});
        let assistant = if streaming {
            let mut stream = llmshim::stream(&router, &req).await.unwrap();
            let mut reasoning = ReasoningAccumulator::default();
            let mut calls = Vec::new();
            let mut content = String::new();
            let mut finished = false;
            let mut priced = false;
            while let Some(chunk) = stream.next().await {
                let chunk: Value = serde_json::from_str(&chunk.unwrap()).unwrap();
                let delta = &chunk["choices"][0]["delta"];
                reasoning.push(delta);
                if let Some(text) = delta["content"].as_str() {
                    content.push_str(text);
                }
                if let Some(items) = delta["tool_calls"].as_array() {
                    calls.extend(items.iter().cloned());
                }
                finished |= chunk["choices"][0]["finish_reason"] == "tool_calls";
                priced |= chunk["usage"]["cost_usd"].as_f64().is_some_and(|n| n > 0.0);
            }
            assert!(finished && priced);
            json!({"role":"assistant","content":content,"reasoning":reasoning.blocks(),"tool_calls":calls})
        } else {
            let response = llmshim::completion(&router, &req).await.unwrap();
            assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
            assert!(response["usage"]["cost_usd"]
                .as_f64()
                .is_some_and(|n| n > 0.0));
            response["choices"][0]["message"].clone()
        };
        let block = encrypted(&assistant);
        assert_eq!(block["origin"]["model"], "grok-4.7");
        assert_eq!(block["origin"]["family"], "grok");
        let original_payload = block["payload"].clone();
        assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 1);
        let call = assistant["tool_calls"][0].clone();
        assert!(call["id"].as_str().unwrap().starts_with("call_ls_"));
        assert_eq!(
            serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap()
                ["city"],
            "Paris"
        );
        req["messages"].as_array_mut().unwrap().push(assistant);
        req["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"tool","tool_call_id":call["id"],"content":"Sunny"}));
        req["tool_choice"] = json!("none");
        let saved = req.clone();
        let (provider, model) = router.resolve(MODEL).unwrap();
        let wire = provider.transform_request(&model, &req).unwrap();
        assert!(
            wire.body["input"]
                .as_array()
                .unwrap()
                .contains(&original_payload),
            "ciphertext payload must replay unchanged"
        );
        assert_eq!(req, saved, "projection must not mutate stored messages");
        let response = llmshim::completion(&router, &req).await.unwrap();
        assert!(response["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("sunny"));
        println!(
            "PASS streaming={streaming}: tool call, encrypted replay, follow-up, priced usage"
        );
    }
}

#[tokio::test]
#[ignore = "requires xAI key and incurs usage charges"]
async fn image_and_structured_output() {
    use base64::Engine;
    let router = router();
    let mut req = request();
    let image = base64::engine::general_purpose::STANDARD
        .encode(include_bytes!("fixtures/chatgpt-red.png"));
    req["messages"][0]["content"] = json!([{"type":"text","text":"What single color fills this image? Reply with one word."},{"type":"image_url","image_url":{"url":format!("data:image/png;base64,{image}")}}]);
    let response = llmshim::completion(&router, &req).await.unwrap();
    assert!(response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("red"));
    let mut req = request();
    req["messages"][0]["content"] = json!("Return a JSON object with answer equal to pong.");
    req["response_format"] = json!({"type":"json_schema","json_schema":{"name":"reply","strict":true,"schema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}}});
    let response = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(
            response["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
        )
        .unwrap(),
        json!({"answer":"pong"})
    );
    println!("PASS image input and native structured output");
}

#[cfg(feature = "proxy")]
#[tokio::test]
#[ignore = "requires xAI key and incurs usage charges"]
async fn proxy_discovery_and_native_routes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task =
        tokio::spawn(axum::serve(listener, llmshim::proxy::app(router(), None)).into_future());
    use std::future::IntoFuture;
    struct Guard(tokio::task::AbortHandle);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _guard = Guard(task.abort_handle());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap();
    let models: Value = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(models["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == MODEL));
    assert!(!models["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "xai/grok-4.6"));
    for path in [
        "/v1/chat",
        "/v1/chat/stream",
        "/v1/messages",
        "/v1/chat/completions",
    ] {
        let mut req = request();
        req["config"] = json!({"max_tokens":1024,"reasoning_effort":"none"});
        let response = client
            .post(format!("{base}{path}"))
            .json(&req)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
        let text = response.text().await.unwrap();
        assert!(
            text.to_lowercase().contains("pong"),
            "missing expected answer at {path}"
        );
        assert!(!text.contains("event: error"));
        println!("PASS proxy {path}");
    }
}

#[tokio::test]
#[ignore = "requires OpenRouter key and incurs usage charges"]
async fn openrouter_zdr_and_nitro() {
    let router = Router::new().register(
        "openrouter",
        Box::new(llmshim::providers::openrouter::OpenRouter::new(key(
            "OPENROUTER_API_KEY",
        ))),
    );
    let mut req = request();
    req["model"] = json!("openrouter/x-ai/grok-4.7:nitro");
    req["reasoning_effort"] = json!("low");
    req["x-openrouter"] = json!({"provider":{"zdr":true}});
    let response = llmshim::completion(&router, &req).await.unwrap();
    assert_eq!(response["choices"][0]["finish_reason"], "stop");
    assert!(response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("pong"));
    println!("PASS OpenRouter Grok 4.7 with ZDR and Nitro");
}

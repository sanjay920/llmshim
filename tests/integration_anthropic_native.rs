use futures::StreamExt;
use llmshim::{provider::Provider, providers::anthropic::Anthropic, router::Router};
use serde_json::{json, Value};

#[tokio::test]
async fn native_blocks_reach_messages_with_explicit_cache_boundaries() {
    let mut server = mockito::Server::new_async().await;
    let request = json!({
        "model":"anthropic/claude-sonnet-4-6", "max_tokens":64,
        "system":[
            {"type":"text","text":"stable","cache_control":{"type":"ephemeral","ttl":"1h"}},
            {"type":"text","text":"volatile"}
        ],
        "tools":[{"name":"read","description":"Read a file","input_schema":{"type":"object","properties":{}},"cache_control":{"type":"ephemeral","ttl":"1h"}}],
        "tool_choice":{"type":"auto"}, "stop_sequences":["DONE"],
        "messages":[{"role":"user","content":[{"type":"text","text":"answer","cache_control":{"type":"ephemeral","ttl":"5m"}}]}]
    });
    let mut expected = request.clone();
    expected["model"] = json!("claude-sonnet-4-6");
    expected["stream"] = json!(false);
    let upstream = server.mock("POST", "/v1/messages")
        .match_header("x-api-key", "test-key")
        .match_header("anthropic-version", "2023-06-01")
        .match_body(mockito::Matcher::Json(expected.clone()))
        .with_status(200).with_header("content-type", "application/json")
        .with_body(json!({"id":"msg_test","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1,"cache_read_input_tokens":100,"cache_creation_input_tokens":10}}).to_string())
        .create_async().await;
    let router = Router::new().register(
        "anthropic",
        Box::new(Anthropic::new("test-key".into()).with_base_url(format!("{}/v1", server.url()))),
    );
    let response = llmshim::completion(&router, &request).await.unwrap();
    upstream.assert_async().await;
    assert_eq!(response["choices"][0]["message"]["content"], "done");
    assert_eq!(response["usage"]["cache_read_tokens"], 100);
    assert_eq!(response["usage"]["cache_write_tokens"], 10);

    expected["stream"] = json!(true);
    let events = [
        json!({"type":"message_start","message":{"id":"msg_stream","usage":{"input_tokens":2,"cache_read_input_tokens":100}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"streamed"}}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ];
    let stream_body: String = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    let streaming = server
        .mock("POST", "/v1/messages")
        .match_body(mockito::Matcher::Json(expected))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(stream_body)
        .create_async()
        .await;
    let chunks: Vec<_> = llmshim::stream(&router, &request)
        .await
        .unwrap()
        .collect()
        .await;
    let chunks: Vec<Value> = chunks
        .into_iter()
        .map(|chunk| serde_json::from_str(&chunk.unwrap()).unwrap())
        .collect();
    streaming.assert_async().await;
    assert!(chunks
        .iter()
        .any(|chunk| chunk["choices"][0]["delta"]["content"] == "streamed"));
    assert!(chunks
        .iter()
        .any(|chunk| chunk["usage"]["cache_read_tokens"] == 100));

    let provider = Anthropic::new("test-key".into());
    let mut changed = request.clone();
    changed["system"][1]["text"] = json!("new volatile budget");
    let before = provider
        .transform_request("claude-sonnet-4-6", &request)
        .unwrap();
    let after = provider
        .transform_request("claude-sonnet-4-6", &changed)
        .unwrap();
    assert_eq!(before.body["system"][0], after.body["system"][0]);
    assert_ne!(before.body["system"][1], after.body["system"][1]);
    assert!(after.body["system"][1].get("cache_control").is_none());
}

#[test]
fn chat_inputs_still_translate_and_incomplete_native_tools_do_not_pass_through() {
    let provider = Anthropic::new("test-key".into());
    let request = json!({"messages":[{"role":"system","content":"legacy"},{"role":"user","content":"hi"}],
        "stop":["END"], "tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{}}}},
        {"name":"missing_schema"},{"input_schema":{"type":"object"}}]});
    let body = provider
        .transform_request("claude-sonnet-4-6", &request)
        .unwrap()
        .body;
    assert_eq!(body["system"], "legacy");
    assert_eq!(body["stop_sequences"], json!(["END"]));
    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(body["tools"][0]["name"], "read");
    assert!(body["tools"][0].get("input_schema").is_some());
    assert!(body["tools"][0].get("function").is_none());
    assert_eq!(body["messages"], json!([{"role":"user","content":"hi"}]));
    assert!(body.get("cache_control").is_none());
    let mut native = request.clone();
    native["messages"] = json!([{"role":"user","content":"hi"}]);
    native["system"] = json!("native system");
    native["stop_sequences"] = json!(["NATIVE"]);
    let body = provider
        .transform_request("claude-sonnet-4-6", &native)
        .unwrap()
        .body;
    assert_eq!(body["system"], "native system");
    assert_eq!(body["stop_sequences"], json!(["NATIVE"]));

    // Both spellings of the system prompt at once: refused, never one of them dropped.
    let mut both = request;
    both["system"] = json!("native system");
    let refused = provider.transform_request("claude-sonnet-4-6", &both);
    assert!(
        matches!(
            refused,
            Err(llmshim::error::ShimError::ProviderError { status: 400, .. })
        ),
        "a request with both a top-level system and system messages is refused"
    );
}

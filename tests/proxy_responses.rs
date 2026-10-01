#![cfg(feature = "proxy")]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use llmshim::providers::{
    anthropic::Anthropic, gemini::Gemini, openai::OpenAi, openai_compat::OpenAiCompatible, xai::Xai,
};
use llmshim::router::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn post(app: axum::Router, body: Value) -> (StatusCode, Value) {
    // Current-thread tests retain their own turn receipts without sharing each other's lock.
    let receipts = llmshim::proxy::wire::Receipts::new(std::path::PathBuf::from(format!(
        "target/responses-receipts-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    )));
    let response = app
        .layer(axum::Extension(std::sync::Arc::new(receipts)))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn stateless_text_crosses_each_provider_family() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/responses-api-reference.json")).unwrap();
    let text = fixture["output"][0]["content"][0]["text"].as_str().unwrap();
    for family in ["openai", "anthropic", "gemini", "local", "xai"] {
        let mut server = mockito::Server::new_async().await;
        let (provider, path, response): (Box<dyn llmshim::provider::Provider>, &str, Value) =
            match family {
                "openai" => (
                    Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
                    "/responses",
                    fixture.clone(),
                ),
                "xai" => (
                    Box::new(Xai::new("key".into()).with_base_url(server.url())),
                    "/responses",
                    fixture.clone(),
                ),
                "anthropic" => (
                    Box::new(Anthropic::new("key".into()).with_base_url(server.url())),
                    "/messages",
                    json!({
                        "id": "msg_test",
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {
                                "type": "text",
                                "text": text
                            }
                        ],
                        "stop_reason": "end_turn",
                        "usage": {
                            "input_tokens": 12,
                            "output_tokens": 3,
                            "cache_read_input_tokens": 4
                        }
                    }),
                ),
                "gemini" => (
                    Box::new(Gemini::new("key".into()).with_base_url(server.url())),
                    "/models/test:generateContent?key=key",
                    json!({
                        "candidates": [
                            {
                                "content": {
                                    "role": "model",
                                    "parts": [
                                        {
                                            "text": text
                                        }
                                    ]
                                },
                                "finishReason": "STOP"
                            }
                        ],
                        "usageMetadata": {
                            "promptTokenCount": 12,
                            "candidatesTokenCount": 3,
                            "totalTokenCount": 15,
                            "cachedContentTokenCount": 4
                        }
                    }),
                ),
                _ => (
                    Box::new(OpenAiCompatible::new("local", server.url(), None)),
                    "/chat/completions",
                    json!({
                        "id": "chat_test",
                        "choices": [
                            {
                                "message": {
                                    "role": "assistant",
                                    "content": text
                                },
                                "finish_reason": "stop"
                            }
                        ],
                        "usage": {
                            "prompt_tokens": 12,
                            "completion_tokens": 3,
                            "total_tokens": 15,
                            "prompt_tokens_details": {
                                "cached_tokens": 4
                            }
                        }
                    }),
                ),
            };
        let upstream = server
            .mock("POST", path)
            .match_body(mockito::Matcher::Regex("hello".into()))
            .with_body(response.to_string())
            .expect(1)
            .create_async()
            .await;
        let app = llmshim::proxy::app(Router::new().register(family, provider), None);
        let (status, response) = post(
            app.clone(),
            json!({
                "model": format!("{family}/test"),
                "input": "hello",
                "store": false,
                "stream": false
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{family}: {response}");
        assert_eq!(response["object"], "response");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(response["output"][0]["content"][0]["text"], text);
        assert_eq!(response["store"], false);
        if matches!(family, "anthropic" | "gemini" | "local") {
            assert_eq!(
                response["usage"]["input_tokens_details"]["cached_tokens"],
                4
            );
        }
        assert!(response["usage"]["input_tokens"].as_u64().unwrap() > 0);
        assert!(response["usage"]["output_tokens_details"]["reasoning_tokens"].is_u64());
        let (status, error) = post(
            app,
            json!({"model": format!("{family}/test"),"input": "hello","store": true}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("store"));
        upstream.assert_async().await;
    }
}

#[path = "responses/controls.rs"]
mod controls;
#[path = "responses/output.rs"]
mod output;

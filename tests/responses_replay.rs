#![cfg(feature = "proxy")]

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use llmshim::{
    providers::{anthropic::Anthropic, openai::OpenAi, openai_compat::OpenAiCompatible},
    proxy::wire::Receipts,
    router::Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn post(app: axum::Router, body: Value, key: &str) -> Value {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn encrypted_replay_preserves_issuer_and_cli_function_subset() {
    let mut server = mockito::Server::new_async().await;
    let fixture = json!({
        "id": "resp_original",
        "object": "response",
        "model": "gpt-6-astra",
        "status": "completed",
        "output": [
            {
                "type": "reasoning",
                "id": "rs_original",
                "summary": [
                ],
                "encrypted_content": "opaque"
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {
                        "type": "output_text",
                        "text": "Ready"
                    }
                ]
            }
        ],
        "usage": {
            "input_tokens": 2,
            "output_tokens": 1,
            "total_tokens": 3
        }
    });
    let first = server
        .mock("POST", "/responses")
        .match_body(mockito::Matcher::PartialJson(
            json!({"include": ["reasoning.encrypted_content"],"stream": false}),
        ))
        .with_body(fixture.to_string())
        .expect(1)
        .create_async()
        .await;
    let receipts = Arc::new(Receipts::new(std::path::PathBuf::from(format!(
        "target/replay-encrypted-{}",
        uuid::Uuid::new_v4()
    ))));
    let app = llmshim::proxy::app(
        Router::new()
            .register(
                "openai",
                Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
            )
            .register(
                "local",
                Box::new(OpenAiCompatible::new("local", server.url(), None)),
            ),
        None,
    )
    .layer(axum::Extension(receipts.clone()));
    let response = post(
        app.clone(),
        json!({
            "model": "openai/gpt-6-astra",
            "input": "hi",
            "include": ["reasoning.encrypted_content"]
        }),
        "client",
    )
    .await;
    let item = response["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "reasoning")
        .unwrap()
        .clone();
    assert_eq!(item["encrypted_content"], "opaque");
    let first_response_id = response["id"].clone();
    let first_message_id = response["output"][0]["id"].clone();
    first.assert_async().await;
    let second = server
        .mock("POST", "/responses")
        .match_body(mockito::Matcher::PartialJson(json!({
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "tools": [
                {
                    "type": "function",
                    "name": "weather",
                    "parameters": {
                        "type": "object",
                        "properties": {
                        }
                    }
                }
            ],
            "input": [
                {
                    "type": "reasoning",
                    "id": "rs_original",
                    "summary": [
                    ],
                    "encrypted_content": "opaque"
                },
                {
                    "role": "user",
                    "content": "next"
                }
            ]
        })))
        .with_header("content-type", "text/event-stream")
        .with_body(
            [
                json!({
                    "type": "response.created",
                    "response": {
                        "id": "resp_original",
                        "created_at": 1,
                        "model": "gpt-6-astra",
                        "status": "in_progress",
                        "output": [
                        ]
                    }
                }),
                json!({
                    "type": "response.output_text.delta",
                    "delta": "Ready"
                }),
                json!({
                    "type": "response.completed",
                    "response": fixture
                }),
            ]
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>(),
        )
        .expect(1)
        .create_async()
        .await;
    let captured: Value =
        serde_json::from_str(include_str!("fixtures/codex-responses-request.json")).unwrap();
    let mut body: Value = Value::Object(
        captured
            .as_object()
            .unwrap()
            .iter()
            .filter(|(key, _)| {
                matches!(
                    key.as_str(),
                    "model" | "store" | "stream" | "include" | "tool_choice"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    body["input"] = json!([item.clone(),{"role": "user","content": "next"}]);
    body["tools"] = json!([
        {
            "type": "function",
            "name": "weather",
            "parameters": {
                "type": "object",
                "properties": {
                }
            }
        }
    ]);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .header("authorization", "Bearer client")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("response.completed"));
    assert!(text.contains("opaque"));
    let terminal: Value = serde_json::from_str(
        text.split("\n\n")
            .filter_map(|frame| frame.lines().find_map(|line| line.strip_prefix("data: ")))
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(terminal["response"]["id"], first_response_id);
    assert_eq!(terminal["response"]["output"][0]["id"], first_message_id);
    assert_eq!(
        terminal["response"]["output"][0]["content"][0]["text"],
        "Ready"
    );
    second.assert_async().await;

    let omitted = server
        .mock("POST", "/responses")
        .match_body(mockito::Matcher::PartialJson(
            json!({"input": [{"role": "user","content": "omit"}]}),
        ))
        .with_body(fixture.to_string())
        .expect(1)
        .create_async()
        .await;
    let result = post(
        app.clone(),
        json!({"model": "openai/gpt-6-astra","input": "omit"}),
        "client",
    )
    .await;
    let omitted_item = result["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "reasoning")
        .unwrap();
    assert!(omitted_item.get("encrypted_content").is_none());
    omitted.assert_async().await;
    let account = server
        .mock("POST", "/responses")
        .match_request(|request| !request.utf8_lossy_body().unwrap().contains("opaque"))
        .match_body(mockito::Matcher::PartialJson(
            json!({"input": [{"role": "user","content": "account"}]}),
        ))
        .with_body(fixture.to_string())
        .expect(1)
        .create_async()
        .await;
    let other = llmshim::proxy::app(
        Router::new().register(
            "openai",
            Box::new(OpenAi::new("other-key".into()).with_base_url(server.url())),
        ),
        None,
    )
    .layer(axum::Extension(receipts));
    let result = post(
        other,
        json!({
            "model": "openai/gpt-6-astra",
            "input": [
                item.clone(),
                {
                    "role": "user",
                    "content": "account"
                }
            ]
        }),
        "client",
    )
    .await;
    assert_eq!(
        result["metadata"]["reasoning_dropped"],
        json!(["incompatible_target"])
    );
    account.assert_async().await;
    let dropped = server
        .mock("POST", "/chat/completions")
        .match_request(|request| {
            let text = request.utf8_lossy_body().unwrap();
            !text.contains("opaque") && !text.contains("changed") && !text.contains("reasoning")
        })
        .match_body(mockito::Matcher::PartialJson(json!({
            "messages": [
                {
                    "role": "user",
                    "content": "next"
                }
            ]
        })))
        .with_body(
            json!({
                "id": "chat",
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "content": "ok"
                        },
                        "finish_reason": "stop"
                    }
                ],
                "usage": {
                }
            })
            .to_string(),
        )
        .expect(3)
        .create_async()
        .await;
    for (key, alter) in [("client", false), ("other", false), ("client", true)] {
        let mut replay = item.clone();
        if alter {
            replay["encrypted_content"] = json!("changed");
        }
        let result = post(
            app.clone(),
            json!({"model": "local/test","input": [replay,{"role": "user","content": "next"}]}),
            key,
        )
        .await;
        assert_eq!(result["output"][0]["content"][0]["text"], "ok");
        assert_eq!(
            result["metadata"]["reasoning_dropped"],
            json!([if key == "client" && !alter {
                "incompatible_target"
            } else {
                "unissued_or_expired"
            }])
        );
    }
    dropped.assert_async().await;
}

#[path = "responses/thinking.rs"]
mod thinking;

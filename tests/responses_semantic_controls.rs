#![cfg(feature = "proxy")]

#[path = "support/native_post.rs"]
mod native_post;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use llmshim::{
    providers::{anthropic::Anthropic, openai_compat::OpenAiCompatible},
    proxy::wire::Receipts,
    reasoning::WireFormat,
    router::Router,
};
use native_post::post;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

#[tokio::test]
async fn current_turn_keeps_current_reasoning_and_excludes_previous_reasoning() {
    let mut server = mockito::Server::new_async().await;
    let directory = tempfile::tempdir().unwrap();
    let app = llmshim::proxy::app(
        Router::new().register(
            "anthropic",
            Box::new(Anthropic::new("key".into()).with_base_url(server.url())),
        ),
        None,
    )
    .layer(axum::Extension(Arc::new(Receipts::new(
        directory.path().to_owned(),
    ))));
    let mut outputs = Vec::new();
    for signature in ["old_signature", "current_signature"] {
        let upstream = server
            .mock("POST", "/messages")
            .with_body(
                json!({
                    "id": signature, "type": "message", "role": "assistant",
                    "content": [{"type": "thinking", "thinking": signature, "signature": signature},
                        {"type": "text", "text": "ready"}], "stop_reason": "end_turn",
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let (status, result) = post(
            app.clone(),
            "/v1/responses",
            json!({
                "model": "anthropic/claude-sonnet-5-5", "input": "hello",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        outputs.push(result["output"].as_array().unwrap().clone());
        upstream.assert_async().await;
        upstream.remove_async().await;
    }
    for context in ["all_turns", "current_turn"] {
        let upstream = server
            .mock("POST", "/messages")
            .match_request(move |r| {
                let body = String::from_utf8_lossy(r.body().unwrap());
                body.contains("current_signature")
                    && body.contains("old_signature") == (context == "all_turns")
            })
            .with_body(
                json!({"id": "last", "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn"})
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let mut input = vec![json!({"role": "user", "content": "old question"})];
        input.extend(outputs[0].clone());
        input.push(json!({"role": "user", "content": "current question"}));
        input.extend(outputs[1].clone());
        let (status, result) = post(
            app.clone(),
            "/v1/responses",
            json!({
                "model": "anthropic/claude-sonnet-5-5", "input": input,
                "reasoning": {"context": context},
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        upstream.assert_async().await;
        upstream.remove_async().await;
    }
}

#[tokio::test]
async fn summary_none_suppresses_public_summary_without_removing_the_reasoning_receipt() {
    let mut server = mockito::Server::new_async().await;
    let directory = tempfile::tempdir().unwrap();
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(
                OpenAiCompatible::new("local", server.url(), Some("test-key".into()))
                    .with_wire(WireFormat::OpenAiResponses),
            ),
        ),
        None,
    )
    .layer(axum::Extension(Arc::new(Receipts::new(
        directory.path().to_owned(),
    ))));
    for summary in ["auto", "none"] {
        let upstream = server
            .mock("POST", "/responses")
            .with_body(
                json!({
                    "id": "response", "model": "gpt-6-astra", "status": "completed",
                    "output": [{"type": "reasoning",
                        "id": "reasoning", "summary": [{"type": "summary_text", "text": "brief"}],
                        "encrypted_content": "opaque"}],
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let (status, result) = post(
            app.clone(),
            "/v1/responses",
            json!({
                "model": "local/gpt-6-astra", "input": "hello", "reasoning": {"summary": summary},
                "include": ["reasoning.encrypted_content"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(
            result["output"][0]["summary"].as_array().unwrap().len(),
            usize::from(summary == "auto")
        );
        assert_eq!(result["output"][0]["encrypted_content"], "opaque");
        upstream.assert_async().await;
        upstream.remove_async().await;
        let replay = server
            .mock("POST", "/responses")
            .match_body(mockito::Matcher::PartialJson(json!({"input": [
                {"type": "reasoning", "id": "reasoning", "encrypted_content": "opaque",
                    "summary": [{"type": "summary_text", "text": "brief"}]},
                {"role": "user", "content": "next"},
            ]})))
            .with_body(json!({"id": "next", "status": "completed", "output": []}).to_string())
            .expect(1)
            .create_async()
            .await;
        let (status, replayed) = post(
            app.clone(),
            "/v1/responses",
            json!({
                "model": "local/gpt-6-astra", "input": [result["output"][0],
                    {"role": "user", "content": "next"}],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{replayed}");
        assert_eq!(replayed["metadata"], json!({}));
        replay.assert_async().await;
        replay.remove_async().await;
    }
}

#[tokio::test]
async fn disabled_parallel_calls_allow_one_and_fail_multiple_in_json_and_sse() {
    for streaming in [false, true] {
        for count in [1, 2] {
            let mut server = mockito::Server::new_async().await;
            let directory = tempfile::tempdir().unwrap();
            let calls: Vec<Value> = (0..count)
                .map(|index| {
                    json!({
                        "index": index, "id": format!("call_{index}"), "type": "function",
                        "function": {"name": "patch", "arguments": "{\"input\":\"patch\"}"},
                    })
                })
                .collect();
            let response = json!({"id": "response", "choices": [{"index": 0,
                "finish_reason": "tool_calls", "message": {"content": null, "tool_calls": calls},
            }]});
            let body = if streaming {
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({"id": "response",
                    "choices": [{"index": 0, "finish_reason": "tool_calls",
                        "delta": response["choices"][0]["message"]}]})
                )
            } else {
                response.to_string()
            };
            let upstream = server
                .mock("POST", "/chat/completions")
                .with_header(
                    "content-type",
                    if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .with_body(body)
                .expect(1)
                .create_async()
                .await;
            let app = llmshim::proxy::app(
                Router::new().register(
                    "local",
                    Box::new(OpenAiCompatible::new("local", server.url(), None)),
                ),
                None,
            )
            .layer(axum::Extension(Arc::new(Receipts::new(
                directory.path().to_owned(),
            ))));
            let request = json!({"model": "local/test", "input": "hello", "stream": streaming,
                "parallel_tool_calls": false, "tools": [{"type": "custom", "name": "patch",
                    "format": {"type": "text"}}]});
            let result = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/responses")
                        .header("content-type", "application/json")
                        .body(Body::from(request.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = result.status();
            let body = to_bytes(result.into_body(), 100_000).await.unwrap();
            let text = std::str::from_utf8(&body).unwrap();
            if streaming {
                assert_eq!(status, StatusCode::OK);
                assert!(
                    text.contains(if count == 1 {
                        "event: response.completed"
                    } else {
                        "event: response.failed"
                    }),
                    "{text}"
                );
                assert!(!text.contains("x-responses-controls"));
            } else {
                assert_eq!(
                    status,
                    if count == 1 {
                        StatusCode::OK
                    } else {
                        StatusCode::BAD_GATEWAY
                    },
                    "{text}"
                );
            }
            upstream.assert_async().await;
        }
    }
}

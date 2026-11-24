//! Host response filtering precedes native replay persistence on every wire.
#![cfg(feature = "proxy")]

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    response::{
        sse::{Event, Sse},
        IntoResponse,
    },
    routing::post,
    Extension, Json, Router,
};
use futures::stream;
use llmshim::proxy::wire::{stream_frames, translate, Receipts, ResponseRedactor, Wire};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{convert::Infallible, sync::Arc};
use tower::ServiceExt;

#[tokio::test]
async fn native_receipts_persist_filtered_canonical_values() {
    for filtered in [false, true] {
        for streaming in [false, true] {
            for path in [
                "/v1/chat/completions",
                "/v1/messages",
                "/v1/responses",
                "/v1beta/models/openai/gpt-6-sol:generateContent",
            ] {
                let path = if streaming {
                    path.replace(":generateContent", ":streamGenerateContent")
                } else {
                    path.to_owned()
                };
                let dir = tempfile::tempdir().unwrap();
                let receipts = Arc::new(Receipts::new(dir.path().join("receipts")));
                let value = json!({
                    "id": "reply", "model": "openai/gpt-6-sol",
                    "message": {
                        "role": "assistant", "content": "ordinary answer",
                        "tool_calls": [{
                            "id": "call_ls_echo", "type": "function",
                            "function": {"name": "read",
                                "arguments": "{\"key\":\"echoed-secret\"}"},
                            "thought_signature": {"data": "echoed-secret",
                                "origin": {"wire": "google"}},
                            "wire_ids": []
                        }],
                        "reasoning": [{
                            "kind": "text", "text": "ordinary thought",
                            "signature": "echoed-secret",
                            "origin": {"wire": "anthropic-messages"}
                        }]
                    },
                    "finish_reason": "tool_calls", "usage": {}
                });
                let application = Router::new().route(&path, post(move || {
                    let value = value.clone();
                    async move {
                        if !streaming {
                            return Json(value).into_response();
                        }
                        let call = &value["message"]["tool_calls"][0];
                        let frames = vec![
                            json!({"type": "tool_call", "id": call["id"],
                                "name": call["function"]["name"],
                                "arguments": call["function"]["arguments"],
                                "thought_signature": call["thought_signature"], "wire_ids": []}),
                            json!({"type": "reasoning", "blocks": value["message"]["reasoning"]}),
                            json!({"type": "done", "finish_reason": "tool_calls"}),
                        ];
                        Sse::new(stream::iter(frames.into_iter().map(|frame| {
                            Ok::<_, Infallible>(Event::default().data(frame.to_string()))
                        }))).into_response()
                    }
                })).layer(axum::middleware::from_fn(translate))
                    .layer(Extension(receipts.clone()));
                let application = if filtered {
                    application.layer(Extension(ResponseRedactor(Arc::new(|value| {
                        serde_json::from_str(
                            &value.to_string().replace("echoed-secret", "[redacted]"),
                        )
                        .unwrap()
                    }))))
                } else {
                    application
                };
                let body = if path.contains("/v1beta/") {
                    json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]})
                } else if path == "/v1/responses" {
                    json!({"model": "openai/gpt-6-sol", "input": "hi", "stream": streaming})
                } else {
                    json!({"model": "openai/gpt-6-sol", "messages": [{"role": "user",
                        "content": "hi"}], "stream": streaming})
                };
                let response = application
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(path.as_str())
                            .header("content-type", "application/json")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{path}");
                let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
                let output = String::from_utf8(bytes.to_vec()).unwrap();
                assert_eq!(
                    output.contains("echoed-secret"),
                    !filtered,
                    "{path}: {output}"
                );
                let reopened = Receipts::new(dir.path().join("receipts"));
                let scope = format!("{:x}", Sha256::digest(b"anonymous"));
                let stored = reopened
                    .get(&scope, "call", &json!("call_ls_echo"))
                    .unwrap()
                    .unwrap();
                let mut directories = vec![dir.path().join("receipts")];
                let mut echoed = false;
                while let Some(directory) = directories.pop() {
                    for entry in std::fs::read_dir(directory).unwrap() {
                        let path = entry.unwrap().path();
                        if path.is_dir() {
                            directories.push(path);
                        } else {
                            echoed |= String::from_utf8_lossy(&std::fs::read(path).unwrap())
                                .contains("echoed-secret");
                        }
                    }
                }
                assert_eq!(echoed, !filtered, "{path}: persisted receipt files");
                assert_eq!(stored["function"]["name"], "read");
                assert_eq!(
                    stored.to_string().contains("echoed-secret"),
                    !filtered,
                    "{path}"
                );
                assert_eq!(
                    stored["thought_signature"]["data"],
                    if filtered {
                        "[redacted]"
                    } else {
                        "echoed-secret"
                    }
                );
            }
        }
    }
}

#[test]
fn native_chat_frames_reject_malformed_choices_messages_and_calls() {
    let valid = json!({"choices": [{"message": {"content": "ordinary",
        "tool_calls": [{"id": "call", "function": {"name": "read", "arguments": "{}"}}]
    }}]});
    let frames = stream_frames(&valid, Wire::Chat);
    assert_eq!(frames.len(), 2);
    let chunk: Value = serde_json::from_str(&frames[0].1).unwrap();
    assert_eq!(chunk["choices"][0]["delta"]["content"], "ordinary");
    assert_eq!(chunk["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
    for invalid in [
        Value::Null,
        json!({}),
        json!({"choices": {}}),
        json!({"choices": []}),
        json!({"choices": ["choice"]}),
        json!({"choices": [{}]}),
        json!({"choices": [{"message": "message"}]}),
        json!({"choices": [{"message": {"tool_calls": "calls"}}]}),
        json!({"choices": [{"message": {"tool_calls": ["call"]}}]}),
    ] {
        let frames = stream_frames(&invalid, Wire::Chat);
        assert_eq!(frames.len(), 1);
        let error: Value = serde_json::from_str(&frames[0].1).unwrap();
        assert_eq!(
            error["error"]["message"],
            "native chat response requires object choices, messages, and tool calls"
        );
    }
    for message in [json!({"content": "text"}), json!({"tool_calls": []})] {
        assert_eq!(
            stream_frames(&json!({"choices": [{"message": message}]}), Wire::Chat).len(),
            2
        );
    }
}

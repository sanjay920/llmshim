#![cfg(feature = "proxy")]

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use llmshim::{
    provider::Provider,
    providers::{anthropic::Anthropic, gemini::Gemini, openai::OpenAi, xai::Xai},
    router::Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

#[tokio::test]
async fn native_parallel_control_preserves_prefix_and_reports_verbosity_drop() {
    for streaming in [false, true] {
        for verbosity in [false, true] {
            let mut server = mockito::Server::new_async().await;
            let reply = if streaming {
                [
                    json!({"type": "message_start", "message": {"id": "turn",
                        "model": "claude-sonnet-5-5", "role": "assistant", "content": []}}),
                    json!({"type": "content_block_delta", "index": 0,
                        "delta": {"type": "text_delta", "text": "ok"}}),
                    json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
                    json!({"type": "message_stop"}),
                ]
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>()
            } else {
                json!({"id": "turn", "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn"})
                .to_string()
            };
            let upstream = server
                .mock("POST", "/messages")
                .match_request(|request| {
                    let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                    body["system"] == "original prefix"
                        && body["messages"] == json!([{"role": "user", "content": "hello"}])
                        && body["tool_choice"]["disable_parallel_tool_use"] == true
                        && body.get("text").is_none()
                        && body.get("verbosity").is_none()
                })
                .with_header(
                    "content-type",
                    if streaming {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .with_body(reply)
                .expect(1)
                .create_async()
                .await;
            let app = llmshim::proxy::app(
                Router::new().register(
                    "anthropic",
                    Box::new(Anthropic::new("key".into()).with_base_url(server.url())),
                ),
                None,
            );
            let mut request = json!({"model": "anthropic/claude-sonnet-5-5",
                "instructions": "original prefix", "input": "hello", "stream": streaming,
                "parallel_tool_calls": false, "tools": [{"type": "function", "name": "read",
                    "parameters": {"type": "object", "properties": {}}}]});
            if verbosity {
                request["text"] = json!({"verbosity": "low"});
            }
            let response = app
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
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
            let result: Value = if streaming {
                let body = std::str::from_utf8(&bytes).unwrap();
                let terminal = body
                    .split("\n\n")
                    .filter_map(|frame| frame.lines().find_map(|line| line.strip_prefix("data: ")))
                    .last()
                    .unwrap();
                let event: Value = serde_json::from_str(terminal).unwrap();
                assert_eq!(event["type"], "response.completed");
                event["response"].clone()
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            assert_eq!(result["output"][0]["content"][0]["text"], "ok");
            if verbosity {
                assert_eq!(
                    result["metadata"]["controls_dropped"],
                    json!(["text.verbosity"])
                );
            } else {
                assert!(result["metadata"].get("controls_dropped").is_none());
            }
            upstream.assert_async().await;
        }
    }
}

#[test]
fn unsupported_parallel_control_is_refused_by_name() {
    for (provider, model) in [
        (
            Box::new(Gemini::new("key".into())) as Box<dyn Provider>,
            "gemini-2.5-pro",
        ),
        (
            Box::new(Xai::new("key".into())) as Box<dyn Provider>,
            "grok-4.7",
        ),
    ] {
        let mut request = json!({"messages": [{"role": "user", "content": "hello"}]});
        assert!(provider.transform_request(model, &request).is_ok());
        for parallel in [false, true] {
            request["parallel_tool_calls"] = json!(parallel);
            let error = provider.transform_request(model, &request).err().unwrap();
            assert!(error
                .to_string()
                .contains("parallel_tool_calls is unsupported"));
        }
    }
}

#[test]
fn native_call_and_declaration_names_require_nonempty_strings() {
    let provider = OpenAi::new("key".into());
    for kind in ["function_call", "custom_tool_call"] {
        for name in [json!("patch"), Value::Null, json!(3), json!("")] {
            let mut item = json!({"type": kind, "id": "item", "call_id": "call", "name": name,
                "arguments": "{}", "input": "patch"});
            if name.is_null() {
                item.as_object_mut().unwrap().remove("name");
            }
            let result = provider.transform_response(
                "gpt-6-astra",
                json!({"id": "turn",
                "status": "completed", "output": [item]}),
            );
            if name == "patch" {
                assert!(result.is_ok());
            } else {
                assert!(result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("tool name must be a nonempty string"));
            }
        }
    }
    for name in [json!("patch"), Value::Null, json!(3), json!("")] {
        let mut request = json!({"messages": [{"role": "user", "content": "hello"}],
            "x-responses-tools": [{"type": "custom", "name": name,
                "format": {"type": "text"}}]});
        if name.is_null() {
            request["x-responses-tools"][0]
                .as_object_mut()
                .unwrap()
                .remove("name");
        }
        let result = provider.transform_request("gpt-6-astra", &request);
        if name == "patch" {
            assert_eq!(result.unwrap().body["tools"][0]["name"], name);
        } else {
            assert!(result
                .err()
                .unwrap()
                .to_string()
                .contains("tool name must be a nonempty string"));
        }
    }
}

#[tokio::test]
async fn xai_verbosity_is_named_as_dropped() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/responses")
        .match_body(mockito::Matcher::PartialJson(json!({"input": [
            {"role": "user", "content": "hello"},
        ]})))
        .with_body(json!({"id": "turn", "status": "completed", "output": []}).to_string())
        .expect(1)
        .create_async()
        .await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "xai",
            Box::new(Xai::new("key".into()).with_base_url(server.url())),
        ),
        None,
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": "xai/grok-4.7", "input": "hello",
            "text": {"verbosity": "low"}})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
    let result: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        result["metadata"]["controls_dropped"],
        json!(["text.verbosity"])
    );
    upstream.assert_async().await;
}

#[test]
fn namespace_container_names_are_checked_even_without_children() {
    let provider = OpenAi::new("key".into());
    for children in [
        json!([]),
        json!([{"type": "custom", "name": "patch",
        "format": {"type": "text"}}]),
    ] {
        for name in [json!("tools"), Value::Null, json!(3), json!("")] {
            let mut tool = json!({"type": "namespace", "name": name, "tools": children});
            if name.is_null() {
                tool.as_object_mut().unwrap().remove("name");
            }
            let result = provider.transform_request(
                "gpt-6-astra",
                &json!({
                    "messages": [{"role": "user", "content": "hello"}], "x-responses-tools": [tool],
                }),
            );
            if name == "tools" {
                assert!(result.is_ok());
            } else {
                assert!(result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("tool name must be a nonempty string"));
            }
        }
    }
}

#![cfg(feature = "proxy")]

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use llmshim::{providers::openai_compat::OpenAiCompatible, router::Router};
use serde_json::{json, Value};
use tower::ServiceExt;

fn request(input: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(input.to_string()))
        .unwrap()
}

fn frames(text: &str) -> Vec<Value> {
    text.split("\n\n")
        .filter_map(|frame| {
            let data = frame.lines().find_map(|line| line.strip_prefix("data: "))?;
            let value: Value = serde_json::from_str(data).unwrap();
            let event = frame
                .lines()
                .find_map(|line| line.strip_prefix("event: "))
                .unwrap();
            assert_eq!(event, value["type"]);
            Some(value)
        })
        .collect()
}

#[tokio::test]
async fn text_call_and_incomplete_event_goldens() {
    for (finish, call, terminal) in [
        ("stop", false, "response.completed"),
        ("length", false, "response.incomplete"),
        ("tool_calls", true, "response.completed"),
    ] {
        let mut server = mockito::Server::new_async().await;
        let delta = if call {
            json!({
                "tool_calls": [
                    {
                        "index": 0,
                        "id": "call_weather",
                        "type": "function",
                        "function": {
                            "name": "weather",
                            "arguments": "{\"city\":"
                        }
                    }
                ]
            })
        } else {
            json!({"content": "Hel"})
        };
        let tail = if call {
            json!({"tool_calls": [{"index": 0,"function": {"arguments": "\"Paris\"}"}}]})
        } else {
            json!({"content": "lo"})
        };
        let mut body = String::new();
        for chunk in [
            json!({
                "id": "upstream",
                "choices": [
                    {
                        "index": 0,
                        "delta": delta,
                        "finish_reason": null
                    }
                ]
            }),
            json!({"id": "upstream","choices": [{"index": 0,"delta": tail,"finish_reason": null}]}),
            json!({
                "id": "upstream",
                "choices": [
                    {
                        "index": 0,
                        "delta": {
                        },
                        "finish_reason": finish
                    }
                ],
                "usage": {
                    "prompt_tokens": 9,
                    "completion_tokens": 4,
                    "total_tokens": 13
                }
            }),
        ] {
            body.push_str(&format!("data: {chunk}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");
        let upstream = server
            .mock("POST", "/chat/completions")
            .match_body(mockito::Matcher::PartialJson(json!({"stream": true})))
            .with_header("content-type", "text/event-stream")
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
        );
        let response = app
            .oneshot(request(
                json!({"model": "local/test","input": "hello","stream": true}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"));
        let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
        let events = frames(std::str::from_utf8(&bytes).unwrap());
        let expected = if call {
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                terminal,
            ]
        } else {
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                terminal,
            ]
        };
        assert_eq!(
            events
                .iter()
                .map(|v| v["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], index);
        }
        let final_response = &events.last().unwrap()["response"];
        assert_eq!(final_response["id"], events[0]["response"]["id"]);
        use sha2::{Digest, Sha256};
        assert_eq!(
            final_response["id"],
            format!(
                "resp_{:x}",
                Sha256::digest(json!("upstream").to_string().as_bytes())
            )
        );
        assert_eq!(final_response["output"][0]["id"], events[2]["item"]["id"]);
        assert_eq!(final_response["usage"]["input_tokens"], 9);
        assert_eq!(final_response["usage"]["output_tokens"], 4);
        if call {
            assert_eq!(events[3]["delta"], "{\"city\":\"Paris\"}");
            assert!(final_response["output"][0]["call_id"]
                .as_str()
                .unwrap()
                .starts_with("call_ls_"));
            assert_eq!(final_response["output"][0]["name"], "weather");
            assert_eq!(final_response["output"][0]["arguments"], events[3]["delta"]);
        } else {
            assert_eq!(events[4]["delta"], "Hel");
            assert_eq!(events[5]["delta"], "lo");
            assert_eq!(final_response["output"][0]["content"][0]["text"], "Hello");
        }
        if finish == "length" {
            assert_eq!(
                final_response["incomplete_details"]["reason"],
                "max_output_tokens"
            );
        }
        upstream.assert_async().await;
    }
}

#[tokio::test]
async fn premature_eof_fails_instead_of_completing() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"conten",
            "t\":\"partial\"},\"finish_reason\":null}]}\n\n",
        ))
        .expect(1)
        .create_async()
        .await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        ),
        None,
    );
    let response = app
        .oneshot(request(
            json!({"model": "local/test","input": "hello","stream": true}),
        ))
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
    let events = frames(std::str::from_utf8(&bytes).unwrap());
    assert_eq!(events.last().unwrap()["type"], "response.failed");
    assert_eq!(events.last().unwrap()["response"]["status"], "failed");
    assert!(!events.iter().any(|v| v["type"] == "response.completed"));
    upstream.assert_async().await;
}

#[tokio::test]
async fn empty_text_does_not_open_an_orphan_item() {
    for text in ["", "visible"] {
        let mut server = mockito::Server::new_async().await;
        let chunks = [
            json!({
                "id": "upstream",
                "choices": [
                    {
                        "index": 0,
                        "delta": {
                            "role": "assistant",
                            "content": ""
                        },
                        "finish_reason": null
                    }
                ]
            }),
            json!({
                "id": "upstream",
                "choices": [
                    {
                        "index": 0,
                        "delta": {
                            "content": text
                        },
                        "finish_reason": null
                    }
                ]
            }),
            json!({"id": "upstream","choices": [{"index": 0,"delta": {},"finish_reason": "stop"}]}),
        ];
        let body = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>();
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_header("content-type", "text/event-stream")
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
        );
        let response = app
            .oneshot(request(
                json!({"model": "local/test","input": "hi","stream": true}),
            ))
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
        let events = frames(std::str::from_utf8(&bytes).unwrap());
        let items = events.last().unwrap()["response"]["output"]
            .as_array()
            .unwrap();
        assert_eq!(items.len(), usize::from(!text.is_empty()));
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "response.output_item.added")
                .count(),
            items.len()
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "response.output_item.done")
                .count(),
            items.len()
        );
        if !text.is_empty() {
            assert_eq!(items[0]["content"][0]["text"], text);
        }
        upstream.assert_async().await;
    }
}

#[cfg(feature = "gateway")]
#[tokio::test]
async fn queued_gateway_preserves_responses_identity() {
    use sha2::{Digest, Sha256};
    let mut server = mockito::Server::new_async().await;
    let body = [
        json!({
            "id": "queued_id",
            "choices": [
                {
                    "index": 0,
                    "delta": {
                        "content": "queued"
                    },
                    "finish_reason": null
                }
            ]
        }),
        json!({
            "id": "queued_id",
            "choices": [
                {
                    "index": 0,
                    "delta": {
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 2,
                "completion_tokens": 1,
                "total_tokens": 3
            }
        }),
    ]
    .iter()
    .map(|chunk| format!("data: {chunk}\n\n"))
    .collect::<String>();
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .expect(1)
        .create_async()
        .await;
    let state = llmshim::gateway::http::GatewayState::from_env(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        ),
        None,
    );
    let response = llmshim::gateway::http::app(state)
        .oneshot(request(
            json!({"model": "local/test","input": "hi","stream": true}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 100_000).await.unwrap();
    let events = frames(std::str::from_utf8(&body).unwrap());
    let final_response = &events.last().unwrap()["response"];
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    assert_eq!(
        final_response["id"],
        format!(
            "resp_{:x}",
            Sha256::digest(json!("queued_id").to_string().as_bytes())
        )
    );
    assert_eq!(final_response["output"][0]["content"][0]["text"], "queued");
    assert_eq!(final_response["usage"]["input_tokens"], 2);
    upstream.assert_async().await;
}

#[test]
fn gemini_stream_identity_matches_unary_identity() {
    use llmshim::{provider::Provider, providers::gemini::Gemini};
    let provider = Gemini::new("unused".into());
    for id in [Some("gemini_response"), None] {
        let mut native = json!({
            "candidates": [
                {
                    "content": {
                        "parts": [
                            {
                                "text": "hello"
                            }
                        ]
                    },
                    "finishReason": "STOP"
                }
            ]
        });
        if let Some(id) = id {
            native["responseId"] = json!(id);
        }
        let unary = provider
            .transform_response("gemini-2.5-flash", native.clone())
            .unwrap();
        let mut normalizer = provider.stream_normalizer("gemini-2.5-flash");
        let chunk = normalizer.push(&native.to_string()).unwrap().unwrap();
        let chunk: Value = serde_json::from_str(&chunk).unwrap();
        assert_eq!(chunk["id"].as_str().unwrap_or(""), unary["id"]);
        assert_eq!(chunk["choices"][0]["delta"]["content"], "hello");
    }
}

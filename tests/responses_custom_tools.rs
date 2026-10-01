#![cfg(feature = "proxy")]

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use llmshim::{
    providers::openai_compat::OpenAiCompatible, proxy::wire::Receipts, reasoning::WireFormat,
    router::Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const PATCH: &str = "*** Begin Patch\n*** Add File: café.txt\n+quote: \"\\\"\r\n*** End Patch\n";

async fn post(app: axum::Router, request: Value) -> (StatusCode, String) {
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
    let status = response.status();
    let body = to_bytes(response.into_body(), 100_000).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn full_cli_custom_calls_round_trip_on_both_wires_and_delivery_modes() {
    for native in [true, false] {
        for streaming in [true, false] {
            let mut server = mockito::Server::new_async().await;
            let mut request: Value =
                serde_json::from_str(include_str!("fixtures/codex-responses-request.json"))
                    .unwrap();
            request["model"] = json!("local/gpt-6-astra");
            request["stream"] = json!(streaming);
            let tools = request["input"][0]["tools"].clone();
            let namespace_name = tools[0]["name"].as_str().unwrap();
            let wire_name = concat!(
                "rt_b9c1ca7960ec4c37f7eb21b3c2ef2e2df877",
                "3925a6174652395fdbf7eb94",
            );
            let arguments = json!({"input": PATCH}).to_string();
            let native_item = json!({
                "type": "custom_tool_call", "id": "ctc_patch", "call_id": "call_patch",
                "name": "apply_patch", "namespace": namespace_name, "input": PATCH,
                "status": "completed",
            });
            let response = if native {
                json!({
                    "id": "resp_patch", "model": "gpt-6-astra", "status": "completed",
                    "output": [native_item], "usage": {"input_tokens": 4,"output_tokens": 2},
                })
            } else {
                json!({
                    "id": "chat_patch", "model": "gpt-6-astra",
                    "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                        "role": "assistant", "content": null, "tool_calls": [{
                            "id": "call_patch", "type": "function", "function": {
                                "name": wire_name, "arguments": arguments,
                            }
                        }],
                    }}], "usage": {"prompt_tokens": 4, "completion_tokens": 2},
                })
            };
            let body = if streaming {
                if native {
                    format!(
                        "data: {}\n\ndata: {}\n\n",
                        json!({"type": "response.output_item.done", "output_index": 0,
                            "item": native_item}),
                        json!({"type": "response.completed", "response": response}),
                    )
                } else {
                    format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"id": "chat_patch", "model": "gpt-6-astra", "choices": [{
                            "index": 0, "delta": response["choices"][0]["message"],
                            "finish_reason": "tool_calls",
                        }]}),
                    )
                }
            } else {
                response.to_string()
            };
            let expected = if native {
                json!({
                    "tools": tools, "reasoning": request["reasoning"], "text": request["text"],
                    "parallel_tool_calls": false, "prompt_cache_key": "recorded-affinity",
                })
            } else {
                json!({"tools": [{"type": "function", "function": {
                    "name": wire_name,
                    "parameters": {"type": "object", "properties": {
                        "input": {"type": "string"}
                    }, "required": ["input"], "additionalProperties": false},
                }}], "parallel_tool_calls": false})
            };
            let path = if native {
                "/responses"
            } else {
                "/chat/completions"
            };
            let upstream = server
                .mock("POST", path)
                .match_body(mockito::Matcher::PartialJson(expected))
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
            let directory = tempfile::tempdir().unwrap();
            let provider =
                OpenAiCompatible::new("local", server.url(), None).with_wire(if native {
                    WireFormat::OpenAiResponses
                } else {
                    WireFormat::OpenAiChat
                });
            let app =
                llmshim::proxy::app(Router::new().register("local", Box::new(provider)), None)
                    .layer(axum::Extension(Arc::new(Receipts::new(
                        directory.path().to_owned(),
                    ))));
            let (status, body) = post(app.clone(), request.clone()).await;
            assert_eq!(status, StatusCode::OK, "{native}/{streaming}: {body}");
            let result: Value = if streaming {
                let events: Vec<Value> = body
                    .split("\n\n")
                    .filter_map(|frame| {
                        let data = frame.lines().find_map(|line| line.strip_prefix("data: "))?;
                        serde_json::from_str(data).ok()
                    })
                    .collect();
                let delta = events
                    .iter()
                    .find(|event| event["type"] == "response.custom_tool_call_input.delta")
                    .unwrap();
                assert_eq!(
                    delta["delta"].as_str().unwrap().as_bytes(),
                    PATCH.as_bytes()
                );
                assert!(!events
                    .iter()
                    .any(|event| { event["type"] == "response.function_call_arguments.delta" }));
                assert!(!body.contains("x-responses-tools"));
                events.last().unwrap()["response"].clone()
            } else {
                serde_json::from_str(&body).unwrap()
            };
            let call = &result["output"][0];
            assert_eq!(call["type"], "custom_tool_call", "{result}");
            assert_eq!(call["name"], "apply_patch");
            assert_eq!(call["namespace"], namespace_name);
            assert_eq!(call["input"].as_str().unwrap().as_bytes(), PATCH.as_bytes());
            assert!(call.get("arguments").is_none());
            upstream.assert_async().await;
            upstream.remove_async().await;
            let replay_item = if native {
                json!({"type": "custom_tool_call", "call_id": "call_patch",
                    "name": "apply_patch", "namespace": namespace_name, "input": PATCH,
                    "id": "ctc_patch"})
            } else {
                json!({"id": "call_patch", "type": "function", "function": {
                    "name": wire_name, "arguments": arguments,
                }})
            };
            let expected = if native {
                json!({"input": [replay_item, {
                    "type": "custom_tool_call_output", "call_id": "call_patch", "output": "ok",
                }]})
            } else {
                json!({"messages": [
                    {"role": "assistant", "content": null, "tool_calls": [replay_item]},
                    {"role": "tool", "tool_call_id": "call_patch", "content": "ok"},
                ]})
            };
            // Match the suffix: developer/user context remains ahead of the replayed turn.
            let expected_items = expected[if native { "input" } else { "messages" }]
                .as_array()
                .unwrap()
                .clone();
            let second = server
                .mock("POST", path)
                .match_request(move |request| {
                    let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                    let items = body[if native { "input" } else { "messages" }]
                        .as_array()
                        .unwrap();
                    items.ends_with(&expected_items)
                })
                .with_body(response.to_string())
                .expect(1)
                .create_async()
                .await;
            request["stream"] = json!(false);
            let input = request["input"].as_array_mut().unwrap();
            input.push(call.clone());
            input.push(
                json!({"type": "custom_tool_call_output", "call_id": call["call_id"],
                "output": "ok"}),
            );
            let (status, body) = post(app, request).await;
            assert_eq!(status, StatusCode::OK, "{native}/{streaming}: {body}");
            let result: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(result["output"][0]["input"], PATCH);
            second.assert_async().await;
        }
    }
}

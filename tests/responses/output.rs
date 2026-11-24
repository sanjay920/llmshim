//! Responses output and owned function-history cases.
use super::*;

#[tokio::test]
async fn summary_and_usage_keep_provider_values() {
    let mut server = mockito::Server::new_async().await;
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/responses-api-reference.json")).unwrap();
    let mut response = fixture.clone();
    response["output"].as_array_mut().unwrap().insert(
        0,
        json!({
            "type": "reasoning",
            "id": "rs_test",
            "summary": [
                {
                    "type": "summary_text",
                    "text": "A concise summary"
                }
            ]
        }),
    );
    let upstream = server
        .mock("POST", "/responses")
        .with_body(response.to_string())
        .expect(1)
        .create_async()
        .await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "openai",
            Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
        ),
        None,
    );
    let (status, response) = post(app, json!({"model": "openai/test","input": "hi"})).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["output"][1]["type"], "reasoning");
    assert_eq!(
        response["output"][1]["summary"][0]["text"],
        "A concise summary"
    );
    assert_eq!(
        response["output"][0]["content"][0]["text"],
        fixture["output"][0]["content"][0]["text"]
    );
    assert_eq!(
        response["usage"]["input_tokens"],
        fixture["usage"]["input_tokens"]
    );
    assert_eq!(
        response["usage"]["output_tokens_details"],
        fixture["usage"]["output_tokens_details"]
    );
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        fixture["usage"]["input_tokens_details"]["cached_tokens"]
    );
    upstream.assert_async().await;
}

#[tokio::test]
async fn incomplete_refusal_and_empty_output_are_distinct() {
    for (finish, message, expected) in [
        (
            "length",
            json!({
                "role": "assistant",
                "content": "Partial",
                "tool_calls": [
                    {
                        "id": "truncated",
                        "type": "function",
                        "function": {
                            "name": "weather",
                            "arguments": "{}"
                        }
                    }
                ]
            }),
            "incomplete",
        ),
        (
            "content_filter",
            json!({"role": "assistant","content": null,"refusal": "Cannot answer"}),
            "completed",
        ),
        (
            "stop",
            json!({"role": "assistant","content": ""}),
            "completed",
        ),
    ] {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(
                json!({
                    "id": "chat_test",
                    "choices": [
                        {
                            "message": message,
                            "finish_reason": finish
                        }
                    ],
                    "usage": {
                    }
                })
                .to_string(),
            )
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
        let (status, response) = post(app, json!({"model": "local/test","input": "hi"})).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["status"], expected);
        if finish == "length" {
            assert_eq!(
                response["incomplete_details"]["reason"],
                "max_output_tokens"
            );
            assert_eq!(response["output"][0]["status"], "incomplete");
            assert_eq!(response["output"][1]["status"], "incomplete");
            assert_eq!(response["output"][1]["arguments"], "{}");
        } else if finish == "content_filter" {
            assert!(response["incomplete_details"].is_null());
            assert_eq!(
                response["output"][0]["content"][0],
                json!({"type": "refusal","refusal": "Cannot answer"})
            );
        } else {
            assert_eq!(response["output"], json!([]));
        }
        upstream.assert_async().await;
    }
}

#[tokio::test]
async fn parallel_calls_remain_one_turn_and_owned_ids_require_receipts() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::PartialJson(json!({
            "messages": [
            {"role": "assistant","content": null,"tool_calls": [
                {"id": "a","type": "function","function": {"name": "weather","arguments": "{}"}},
                {"id": "b","type": "function","function": {"name": "weather","arguments": "{}"}}]},
            {"role": "tool","tool_call_id": "a","content": "sunny"},
            {"role": "tool","tool_call_id": "b","content": "rainy"}
        ]
        })))
        .with_body(
            json!({
                "id": "chat_test",
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "content": "Mixed"
                        },
                        "finish_reason": "stop"
                    }
                ],
                "usage": {
                }
            })
            .to_string(),
        )
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
    let mut request = json!({"model": "local/test","input": [
        {"type": "function_call","call_id": "a","name": "weather","arguments": "{}"},
        {"type": "function_call","call_id": "b","name": "weather","arguments": "{}"},
        {"type": "function_call_output","call_id": "a","output": "sunny"},
        {"type": "function_call_output","call_id": "b","output": "rainy"}
    ]});
    let (status, response) = post(app.clone(), request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["output"][0]["content"][0]["text"], "Mixed");
    request["input"][0]["call_id"] = json!("call_ls_unissued");
    request["input"][2]["call_id"] = json!("call_ls_unissued");
    let (status, error) = post(app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("receipt"));
    upstream.assert_async().await;
}

#[tokio::test]
async fn missing_upstream_ids_do_not_merge_distinct_responses() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_body(
            json!({
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "content": "Hello"
                        },
                        "finish_reason": "stop"
                    }
                ],
                "usage": {
                }
            })
            .to_string(),
        )
        .expect(2)
        .create_async()
        .await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        ),
        None,
    );
    let request = json!({"model": "local/test","input": "hi"});
    let (status, first) = post(app.clone(), request.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, second) = post(app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(first["id"], second["id"]);
    assert_ne!(first["output"][0]["id"], second["output"][0]["id"]);
    upstream.assert_async().await;
}

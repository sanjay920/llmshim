//! Signed thinking and full assistant tool-result replay cases.
use super::*;

#[tokio::test]
async fn signed_thinking_round_trip_and_summary_only_drop() {
    let mut server = mockito::Server::new_async().await;
    let fixture = json!({
        "id": "msg_original",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-5-5",
        "content": [
            {
                "type": "thinking",
                "thinking": "Private thought",
                "signature": "signed"
            },
            {
                "type": "text",
                "text": "Ready"
            }
        ],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 2,
            "output_tokens": 1
        }
    });
    let first = server
        .mock("POST", "/messages")
        .match_body(mockito::Matcher::PartialJson(
            json!({"messages": [{"role": "user","content": "hi"}]}),
        ))
        .with_body(fixture.to_string())
        .expect(1)
        .create_async()
        .await;
    let receipts = Arc::new(Receipts::new(std::path::PathBuf::from(format!(
        "target/replay-thinking-{}",
        uuid::Uuid::new_v4()
    ))));
    let app = llmshim::proxy::app(
        Router::new().register(
            "anthropic",
            Box::new(Anthropic::new("key".into()).with_base_url(server.url())),
        ),
        None,
    )
    .layer(axum::Extension(receipts));
    let result = post(
        app.clone(),
        json!({
            "model": "anthropic/claude-sonnet-5-5",
            "input": "hi",
            "include": ["reasoning.encrypted_content"]
        }),
        "client",
    )
    .await;
    let item = result["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "reasoning")
        .unwrap()
        .clone();
    assert_eq!(item["summary"], json!([]));
    assert!(item.get("encrypted_content").is_none());
    first.assert_async().await;
    let replay = server
        .mock("POST", "/messages")
        .match_body(mockito::Matcher::PartialJson(json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "thinking",
                            "thinking": "Private thought",
                            "signature": "signed"
                        },
                        {
                            "type": "text",
                            "text": "Ready"
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": "next"
                }
            ]
        })))
        .with_body(fixture.to_string())
        .expect(1)
        .create_async()
        .await;
    let result = post(
        app.clone(),
        json!({
            "model": "anthropic/claude-sonnet-5-5",
            "input": [
                result["output"][
                    0
                ].clone(),
                item,
                {
                    "role": "user",
                    "content": "next"
                }
            ]
        }),
        "client",
    )
    .await;
    assert_eq!(result["metadata"], json!({}));
    replay.assert_async().await;
    let drop = server
        .mock("POST", "/messages")
        .match_body(mockito::Matcher::PartialJson(
            json!({"messages": [{"role": "user","content": "next"}]}),
        ))
        .with_body(fixture.to_string())
        .expect(1)
        .create_async()
        .await;
    let result = post(
        app,
        json!({
            "model": "anthropic/claude-sonnet-5-5",
            "input": [
                {
                    "type": "reasoning",
                    "summary": [
                        {
                            "type": "summary_text",
                            "text": "Unissued summary"
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": "next"
                }
            ]
        }),
        "client",
    )
    .await;
    assert_eq!(
        result["metadata"]["reasoning_dropped"][0],
        "unissued_or_expired"
    );
    drop.assert_async().await;
}

#[tokio::test]
async fn signed_tool_result_replays_one_complete_assistant_turn() {
    let mut server = mockito::Server::new_async().await;
    let fixture = json!({
        "id": "msg_tool",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-5-5",
        "content": [
            {
                "type": "thinking",
                "thinking": "Plan",
                "signature": "signed"
            },
            {
                "type": "text",
                "text": "Checking"
            },
            {
                "type": "tool_use",
                "id": "upstream_call",
                "name": "weather",
                "input": {
                }
            }
        ],
        "stop_reason": "tool_use",
        "usage": {
            "input_tokens": 2,
            "output_tokens": 1
        }
    });
    let first = server
        .mock("POST", "/messages")
        .match_body(mockito::Matcher::PartialJson(
            json!({"messages": [{"role": "user","content": "hi"}]}),
        ))
        .with_body(fixture.to_string())
        .expect(1)
        .create_async()
        .await;
    let receipts = Arc::new(Receipts::new(std::path::PathBuf::from(format!(
        "target/replay-tool-{}",
        uuid::Uuid::new_v4()
    ))));
    let app = llmshim::proxy::app(
        Router::new().register(
            "anthropic",
            Box::new(Anthropic::new("key".into()).with_base_url(server.url())),
        ),
        None,
    )
    .layer(axum::Extension(receipts));
    let result = post(
        app.clone(),
        json!({
            "model": "anthropic/claude-sonnet-5-5",
            "input": "hi",
            "reasoning": {
                "effort": "low"
            }
        }),
        "client",
    )
    .await;
    first.assert_async().await;
    assert_eq!(result["output"].as_array().unwrap().len(), 3);
    let mut input = vec![json!({"role": "user","content": "hi"})];
    input.extend(result["output"].as_array().unwrap().iter().cloned());
    input.push(json!({
        "type": "function_call_output",
        "call_id": result["output"][
            2
        ]["call_id"],
        "output": "sunny"
    }));
    let replay = server
        .mock("POST", "/messages")
        .match_body(mockito::Matcher::PartialJson(json!({
            "messages": [
                {
                    "role": "user",
                    "content": "hi"
                },
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "thinking",
                            "thinking": "Plan",
                            "signature": "signed"
                        },
                        {
                            "type": "text",
                            "text": "Checking"
                        },
                        {
                            "type": "tool_use",
                            "id": "upstream_call",
                            "name": "weather",
                            "input": {
                            }
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "upstream_call",
                            "content": "sunny"
                        }
                    ]
                }
            ]
        })))
        .with_body(
            json!({
                "id": "msg_final",
                "type": "message",
                "role": "assistant",
                "content": [
                    {
                        "type": "text",
                        "text": "Done"
                    }
                ],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 2,
                    "output_tokens": 1
                }
            })
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let result = post(
        app,
        json!({
            "model": "anthropic/claude-sonnet-5-5",
            "input": input,
            "reasoning": {
                "effort": "low"
            }
        }),
        "client",
    )
    .await;
    assert_eq!(result["output"][0]["content"][0]["text"], "Done");
    assert_eq!(result["metadata"], json!({}));
    replay.assert_async().await;
}

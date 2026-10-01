//! Responses tool, image and structured-output cases.
use super::*;

#[tokio::test]
async fn tools_round_trip_and_translate_controls() {
    let mut server = mockito::Server::new_async().await;
    let tool = json!({
        "type": "function",
        "name": "weather",
        "description": "Weather",
        "parameters": {
            "type": "object",
            "properties": {
                "city": {
                    "type": "string"
                }
            },
            "required": ["city"]
        },
        "strict": true
    });
    let first = server
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::PartialJson(json!({
            "messages": [
                {
                    "role": "system",
                    "content": "Be brief"
                },
                {
                    "role": "user",
                    "content": "Paris?"
                }
            ],
            "max_tokens": 100,"temperature": 0.2,"top_p": 0.9,
            "tool_choice": {"type": "function","function": {"name": "weather"}},
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "weather",
                        "description": "Weather",
                        "parameters": tool["parameters"],
                        "strict": true
                    }
                }
            ]
        })))
        .with_body(
            json!({
                "id": "chat_tool",
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [
                                {
                                    "id": "call_weather",
                                    "type": "function",
                                    "function": {
                                        "name": "weather",
                                        "arguments": "{\"city\":\"Paris\"}"
                                    }
                                }
                            ]
                        },
                        "finish_reason": "tool_calls"
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
    let request = json!({
        "model": "local/test",
        "input": "Paris?",
        "instructions": "Be brief",
        "tools": [
            tool
        ],
        "tool_choice": {
            "type": "function",
            "name": "weather"
        },
        "max_output_tokens": 100,
        "temperature": 0.2,
        "top_p": 0.9
    });
    let (status, response) = post(app.clone(), request).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let call = response["output"][0].clone();
    assert_eq!(call["type"], "function_call");
    assert!(call["call_id"].as_str().unwrap().starts_with("call_ls_"));
    let call_id = call["call_id"].clone();
    assert_eq!(call["name"], "weather");
    assert_eq!(call["arguments"], "{\"city\":\"Paris\"}");
    first.assert_async().await;
    let second = server
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::PartialJson(json!({
            "messages": [
            {"role": "user","content": "Paris?"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "call_weather",
                        "type": "function",
                        "function": {
                            "name": "weather",
                            "arguments": "{\"city\":\"Paris\"}"
                        }
                    }
                ]
            },
            {"role": "tool","tool_call_id": "call_weather","content": "sunny"}
        ]
        })))
        .with_body(
            json!({
                "id": "chat_answer",
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "content": "Sunny in Paris"
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
    let mut followup = json!({
        "model": "local/test",
        "input": [
            {
                "role": "user",
                "content": "Paris?"
            },
            call,
            {
                "type": "function_call_output",
                "call_id": call_id,
                "output": "sunny"
            }
        ]
    });
    let (status, answer) = post(app.clone(), followup.clone()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["output"][0]["content"][0]["text"], "Sunny in Paris");
    followup["input"][2]["call_id"] = json!("orphan");
    let (status, _) = post(app, followup).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    second.assert_async().await;
}

#[tokio::test]
async fn images_schema_and_effort_reach_upstream() {
    let mut server = mockito::Server::new_async().await;
    let schema = json!({
        "type": "object",
        "properties": {
            "ok": {
                "type": "boolean"
            }
        },
        "required": ["ok"],
        "additionalProperties": false
    });
    let upstream = server
        .mock("POST", "/responses")
        .match_body(mockito::Matcher::PartialJson(json!({
            "instructions": "Inspect",
            "input": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "input_text",
                            "text": "Image?"
                        },
                        {
                            "type": "input_image",
                            "image_url": "https://example.com/image.png",
                            "detail": "low"
                        }
                    ]
                }
            ],
            "reasoning": {
                "effort": "low"
            },
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "answer",
                    "strict": true,
                    "schema": schema
                }
            }
        })))
        .with_body(
            json!({
                "id": "resp_schema",
                "status": "completed",
                "output": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {
                                "type": "output_text",
                                "text": "{\"ok\":true}"
                            }
                        ]
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
            "openai",
            Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
        ),
        None,
    );
    let mut request = json!({
        "model": "openai/gpt-6-astra",
        "input": [
            {
                "role": "developer",
                "content": "Inspect"
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Image?"
                    },
                    {
                        "type": "input_image",
                        "image_url": "https://example.com/image.png",
                        "detail": "low"
                    }
                ]
            }
        ],
        "reasoning": {
            "effort": "low"
        },
        "text": {
            "format": {
                "type": "json_schema",
                "name": "answer",
                "strict": true,
                "schema": schema
            }
        }
    });
    let (status, response) = post(app.clone(), request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["output"][0]["content"][0]["text"], "{\"ok\":true}");
    request["input"][1]["content"][1]["image_url"] = Value::Null;
    let (status, error) = post(app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("image_url"));
    upstream.assert_async().await;
}

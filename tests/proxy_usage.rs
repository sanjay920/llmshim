#![cfg(feature = "proxy")]

use axum::{
    body::{to_bytes, Body},
    http::Request,
    Extension,
};
use llmshim::{
    providers::{anthropic::Anthropic, openai_compat::OpenAiCompatible},
    proxy::{app, wire::Receipts},
    router::Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

#[tokio::test]
async fn normalized_usage_crosses_every_inbound_wire_and_upstream_convention() {
    for anthropic in [false, true] {
        for cached in [false, true] {
            for stream in [false, true] {
                for wire in ["compact", "chat", "messages", "gemini", "responses"] {
                    // Inbound Responses does not stream yet; it refuses `stream: true` by name.
                    if wire == "responses" && stream {
                        continue;
                    }
                    let mut server = mockito::Server::new_async().await;
                    let read = if cached { 7 } else { 0 };
                    let write = if cached { 3 } else { 0 };
                    let reasoning = if anthropic { 0 } else { 2 };
                    let mut usage = if anthropic {
                        json!({"input_tokens":11,"output_tokens":5,"cache_read_input_tokens":read,"cache_creation_input_tokens":write})
                    } else {
                        json!({"prompt_tokens":11+read,"completion_tokens":5,"total_tokens":16+read+write,"cache_write_tokens":write,"prompt_tokens_details":{"cached_tokens":read},"completion_tokens_details":{"reasoning_tokens":reasoning}})
                    };
                    if !cached {
                        for key in [
                            "cache_read_input_tokens",
                            "cache_creation_input_tokens",
                            "cache_write_tokens",
                            "prompt_tokens_details",
                        ] {
                            usage.as_object_mut().unwrap().remove(key);
                        }
                    }
                    let native = if anthropic {
                        json!({"id":"msg_1","model":"test","type":"message","role":"assistant","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":usage})
                    } else {
                        json!({"id":"chat_1","model":"test","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":usage})
                    };
                    let body = if !stream {
                        native.to_string()
                    } else if anthropic {
                        format!("event: message_start\ndata: {}\n\nevent: content_block_delta\ndata: {}\n\nevent: message_delta\ndata: {}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
                            json!({"type":"message_start","message":{"id":"msg_1","model":"test","role":"assistant","content":[],"usage":{"input_tokens":11,"cache_read_input_tokens":read,"cache_creation_input_tokens":write}}}),
                            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}),
                            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}))
                    } else {
                        format!(
                            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                            json!({"id":"chat_1","model":"test","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":"stop"}]}),
                            json!({"choices":[],"usage":usage})
                        )
                    };
                    let upstream = server
                        .mock(
                            "POST",
                            if anthropic {
                                "/messages"
                            } else {
                                "/chat/completions"
                            },
                        )
                        .with_header(
                            "content-type",
                            if stream {
                                "text/event-stream"
                            } else {
                                "application/json"
                            },
                        )
                        .with_body(body)
                        .expect(1)
                        .create_async()
                        .await;
                    let router = if anthropic {
                        Router::new().register(
                            "anthropic",
                            Box::new(Anthropic::new("test".into()).with_base_url(server.url())),
                        )
                    } else {
                        Router::new().register(
                            "local",
                            Box::new(OpenAiCompatible::new("local", server.url(), None)),
                        )
                    };
                    let model = if anthropic {
                        "anthropic/test"
                    } else {
                        "local/test"
                    };
                    let (path, request) = match wire {
                        "compact" => (
                            "/v1/chat".to_string(),
                            json!({"model":model,"messages":[{"role":"user","content":"hi"}],"stream":stream}),
                        ),
                        "chat" => (
                            "/v1/chat/completions".to_string(),
                            json!({"model":model,"messages":[{"role":"user","content":"hi"}],"stream":stream}),
                        ),
                        "responses" => (
                            "/v1/responses".to_string(),
                            json!({"model":model,"input":"hi"}),
                        ),
                        "messages" => (
                            "/v1/messages".to_string(),
                            json!({"model":model,"messages":[{"role":"user","content":"hi"}],"max_tokens":20,"stream":stream}),
                        ),
                        _ => (
                            format!(
                                "/v1beta/models/{model}:{}",
                                if stream {
                                    "streamGenerateContent"
                                } else {
                                    "generateContent"
                                }
                            ),
                            json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}),
                        ),
                    };
                    let dir = tempfile::tempdir().unwrap();
                    let response = app(router, None)
                        .layer(Extension(Arc::new(Receipts::new(dir.path().to_owned()))))
                        .oneshot(
                            Request::builder()
                                .method("POST")
                                .uri(path)
                                .header("content-type", "application/json")
                                .body(Body::from(request.to_string()))
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(
                        response.status(),
                        200,
                        "{wire}, stream={stream}, anthropic={anthropic}"
                    );
                    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                    let text = String::from_utf8(bytes.to_vec()).unwrap();
                    let result: Value = if stream {
                        text.lines()
                            .filter_map(|line| line.strip_prefix("data: "))
                            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                            .rfind(|data| {
                                data.get("usage").is_some()
                                    || data.get("usageMetadata").is_some()
                                    || data["type"] == "usage"
                            })
                            .unwrap_or_else(|| panic!("{text}"))
                    } else {
                        serde_json::from_str(&text).unwrap()
                    };
                    let counts = if wire == "gemini" {
                        &result["usageMetadata"]
                    } else if wire == "compact" && stream {
                        &result
                    } else {
                        &result["usage"]
                    };
                    assert_eq!(
                        counts["x-llmshim-usage"],
                        json!({"uncached_input_tokens":11,"cache_read_tokens":read,"cache_write_tokens":write,"output_tokens":5,"reasoning_tokens":reasoning}),
                        "{wire}: {text}"
                    );
                    match wire {
                        "messages" => {
                            assert_eq!(counts["input_tokens"], 11);
                            assert_eq!(counts["cache_read_input_tokens"], read);
                            assert_eq!(counts["cache_creation_input_tokens"], write);
                        }
                        "chat" => {
                            assert_eq!(counts["prompt_tokens"], 11 + read + write);
                            assert_eq!(counts["prompt_tokens_details"]["cached_tokens"], read);
                            assert_eq!(
                                counts["completion_tokens_details"]["reasoning_tokens"],
                                reasoning
                            );
                        }
                        "responses" => {
                            assert_eq!(counts["input_tokens"], 11 + read + write);
                            assert_eq!(counts["input_tokens_details"]["cached_tokens"], read);
                            assert_eq!(
                                counts["output_tokens_details"]["reasoning_tokens"],
                                reasoning
                            );
                        }
                        "gemini" => {
                            assert_eq!(counts["promptTokenCount"], 11 + read + write);
                            assert_eq!(counts["cachedContentTokenCount"], read);
                            assert_eq!(counts["thoughtsTokenCount"], reasoning);
                        }
                        _ => assert_eq!(counts["uncached_input_tokens"], 11),
                    }
                    upstream.assert_async().await;
                }
            }
        }
    }
}

#[test]
fn native_usage_extensions_include_zero_counts_when_canonical_usage_is_absent_or_invalid() {
    use llmshim::proxy::wire::{response_from_chat, Wire};
    let dir = tempfile::tempdir().unwrap();
    let receipts = Receipts::new(dir.path().to_owned());
    for usage in [
        json!(null),
        json!({"uncached_input_tokens":-1,
        "cache_read_tokens":"7","cache_write_tokens":false,
        "output_tokens":null,"reasoning_tokens":-2}),
    ] {
        for wire in [Wire::Chat, Wire::Messages, Wire::Gemini] {
            let response = response_from_chat(
                &json!({"message":{"content":"answer"},"usage":usage}),
                wire,
                &receipts,
                "test",
            )
            .unwrap();
            let field = if wire == Wire::Gemini {
                "usageMetadata"
            } else {
                "usage"
            };
            assert_eq!(
                response[field]["x-llmshim-usage"],
                json!({"uncached_input_tokens":0,
                "cache_read_tokens":0,"cache_write_tokens":0,"output_tokens":0,"reasoning_tokens":0})
            );
        }
    }
}

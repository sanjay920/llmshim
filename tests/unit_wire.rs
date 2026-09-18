#![cfg(feature = "proxy")]
use axum::{
    body::{to_bytes, Body},
    http::Request,
    Extension,
};
use llmshim::{
    provider::Provider,
    providers::{anthropic::Anthropic, openai_compat::OpenAiCompatible},
    proxy::{
        app,
        wire::{request_to_chat, response_from_chat, Receipts, Wire},
    },
    router::Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn canonical() -> Value {
    let p = Anthropic::new("test".into());
    let normalized=p.transform_response("claude-sonnet-4-6",json!({"id":"r","stop_reason":"tool_use","content":[{"type":"thinking","thinking":"brief","signature":"opaque"},{"type":"tool_use","id":"native-id","name":"read","input":{"path":"a"}}],"usage":{"input_tokens":4,"output_tokens":2,"cache_read_input_tokens":3}})).unwrap();
    json!({"id":"r","model":"anthropic/claude-sonnet-4-6","message":normalized["choices"][0]["message"],"finish_reason":"tool_calls","usage":{"input_tokens":4,"output_tokens":2,"total_tokens":9,"cache_read_tokens":3,"cache_write_tokens":0}})
}

#[test]
fn native_replay_survives_reopen_preserves_ids_and_is_scoped_to_credential() {
    let dir = tempfile::tempdir().unwrap();
    let store = Receipts::new(dir.path().join("receipts"));
    let canonical = canonical();
    for wire in [Wire::Messages, Wire::Chat] {
        let native = response_from_chat(&canonical, wire, &store, "credential-a").unwrap();
        let assistant = if wire == Wire::Messages {
            json!({"role":"assistant","content":native["content"]})
        } else {
            native["choices"][0]["message"].clone()
        };
        let call_id = canonical["message"]["tool_calls"][0]["id"].clone();
        let result = if wire == Wire::Messages {
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":call_id,"content":"done"}]})
        } else {
            json!({"role":"tool","tool_call_id":call_id,"content":"done"})
        };
        let request = json!({"model":"anthropic/claude-sonnet-4-6","messages":[{"role":"user","content":"read"},assistant,result]});
        let reopened = Receipts::new(dir.path().join("receipts"));
        let imported = request_to_chat(&request, wire, &reopened, "credential-a").unwrap();
        assert_eq!(
            imported["messages"][1]["reasoning"],
            canonical["message"]["reasoning"]
        );
        assert_eq!(
            imported["messages"][1]["tool_calls"],
            canonical["message"]["tool_calls"]
        );
        let upstream = Anthropic::new("test".into())
            .transform_request(
                "claude-sonnet-4-6",
                &json!({"messages":imported["messages"]}),
            )
            .unwrap();
        assert_eq!(
            upstream.body["messages"][1]["content"][0]["signature"],
            "opaque"
        );
        assert_eq!(
            upstream.body["messages"][1]["content"][1]["id"],
            "native-id"
        );
        assert_eq!(
            upstream.body["messages"][2]["content"][0]["tool_use_id"],
            "native-id"
        );
        assert!(request_to_chat(&request, wire, &reopened, "credential-b").is_err());
    }
}

#[test]
fn native_untracked_thinking_drops_and_changed_calls_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Receipts::new(dir.path().to_owned());
    let request = json!({"model":"anthropic/claude-sonnet-4-6","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"untracked","signature":"opaque"},{"type":"text","text":"answer"}]}]});
    let imported = request_to_chat(&request, Wire::Messages, &store, "a").unwrap();
    assert!(imported["messages"][0].get("reasoning").is_none());
    let canonical = canonical();
    let native = response_from_chat(&canonical, Wire::Chat, &store, "a").unwrap();
    let mut msg = native["choices"][0]["message"].clone();
    msg["tool_calls"][0]["function"]["arguments"] = json!("{\"path\":\"changed\"}");
    assert!(request_to_chat(
        &json!({"model":"m","messages":[msg]}),
        Wire::Chat,
        &store,
        "a"
    )
    .is_err());
}

#[test]
fn encrypted_reasoning_can_cross_inbound_wire_without_losing_original_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let store = Receipts::new(dir.path().to_owned());
    let mut canonical = canonical();
    canonical["message"]["tool_calls"] = json!([]);
    canonical["message"]["reasoning"] = json!([{"kind":"encrypted","data":"opaque-encrypted","item_id":"rs1","origin":{"provider":"openai","model":"gpt-6-astra","family":"gpt","wire":"openai-responses","received_at":"2026-09-16T00:00:00Z","account":"non-secret-binding"},"payload":{"type":"reasoning","encrypted_content":"opaque-encrypted","id":"rs1","summary":[]}}]);
    let native = response_from_chat(&canonical, Wire::Messages, &store, "a").unwrap();
    assert_eq!(native["content"][0]["type"], "redacted_thinking");
    let imported=request_to_chat(&json!({"model":"openai/gpt-6-astra","messages":[{"role":"assistant","content":native["content"]}]}),Wire::Messages,&store,"a").unwrap();
    assert_eq!(
        imported["messages"][0]["reasoning"],
        canonical["message"]["reasoning"]
    );
}

async fn post(app: axum::Router, path: &str, body: Value) -> (axum::http::StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn both_native_http_endpoints_use_shared_completion_and_retain_usage() {
    let mut server = mockito::Server::new_async().await;
    let upstream=server.mock("POST","/chat/completions").match_body(mockito::Matcher::PartialJson(json!({"messages":[{"role":"user","content":"hi"}]}))).with_body(json!({"id":"r","model":"test","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,"prompt_tokens_details":{"cached_tokens":3}}}).to_string()).expect(2).create_async().await;
    let dir = tempfile::tempdir().unwrap();
    let receipts = Arc::new(Receipts::new(dir.path().to_owned()));
    let router = || {
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", &server.url(), None)),
        )
    };
    for (path, wire) in [
        ("/v1/messages", Wire::Messages),
        ("/v1/chat/completions", Wire::Chat),
    ] {
        let (status,body)=post(app(router(),None).layer(Extension(receipts.clone())),path,json!({"model":"local/test","messages":[{"role":"user","content":"hi"}],"max_tokens":30})).await;
        assert_eq!(status, 200, "{body}");
        if wire == Wire::Messages {
            assert_eq!(body["content"][0]["text"], "hello");
            assert_eq!(body["usage"]["cache_read_input_tokens"], 3);
        } else {
            assert_eq!(body["choices"][0]["message"]["content"], "hello");
            assert_eq!(body["usage"]["prompt_tokens_details"]["cached_tokens"], 3);
        }
    }
    upstream.assert_async().await;
}

#[tokio::test]
async fn native_sse_has_complete_frames_and_no_internal_event_types() {
    let mut server = mockito::Server::new_async().await;
    let data = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"r","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}),
        json!({"id":"r","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}})
    );
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(data)
        .expect(2)
        .create_async()
        .await;
    let dir = tempfile::tempdir().unwrap();
    for path in ["/v1/messages", "/v1/chat/completions"] {
        let router = Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", &server.url(), None)),
        );
        let app =
            app(router, None).layer(Extension(Arc::new(Receipts::new(dir.path().to_owned()))));
        let response=app.oneshot(Request::builder().method("POST").uri(path).header("content-type","application/json").body(Body::from(json!({"model":"local/test","messages":[{"role":"user","content":"hi"}],"stream":true}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("text/event-stream"));
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("hello"), "{text}");
        assert!(!text.contains("event: content\n"));
        if path == "/v1/messages" {
            assert!(text.contains("event: message_start"));
            assert!(text.contains("event: message_stop"));
        } else {
            assert!(text.contains("chat.completion.chunk"));
            assert!(text.contains("data: [DONE]"));
        }
    }
    upstream.assert_async().await;
}

#[tokio::test]
async fn native_endpoints_preserve_backpressure_status_and_retry_header() {
    use llmshim::proxy::{
        app_with_state,
        ratelimit::{Backpressure, InMemoryRateLimiter, RateLimitConfig},
        AppState,
    };
    let state = Arc::new(AppState {
        router: Router::new(),
        logger: None,
        limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default())),
        backpressure: Backpressure::new(1, std::time::Duration::from_millis(5)),
    });
    let _held = state.backpressure.acquire().await.unwrap();
    for path in ["/v1/messages", "/v1/chat/completions"] {
        let response = app_with_state(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model":"local/test","messages":[]}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
        assert!(response.headers().contains_key("retry-after"));
    }
}

#[tokio::test]
async fn native_text_is_forwarded_before_upstream_completion() {
    use futures::StreamExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let receiver = Arc::new(tokio::sync::Mutex::new(Some(rx)));
    let upstream=axum::Router::new().route("/chat/completions",axum::routing::post(move ||{let receiver=receiver.clone();async move{
        let mut receiver=receiver.lock().await.take().unwrap();
        let stream=async_stream::stream! {
            yield Ok::<_,std::io::Error>(format!("data: {}\n\n",json!({"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]})));
            (&mut receiver).await.unwrap();
            yield Ok(format!("data: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})));
        };
        ([("content-type","text/event-stream")],Body::from_stream(stream))
    }}));
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let router = Router::new().register(
        "local",
        Box::new(OpenAiCompatible::new(
            "local",
            &format!("http://{addr}"),
            None,
        )),
    );
    let app = app(router, None).layer(Extension(Arc::new(Receipts::new(dir.path().to_owned()))));
    let response=app.oneshot(Request::builder().method("POST").uri("/v1/messages").header("content-type","application/json").body(Body::from(json!({"model":"local/test","messages":[{"role":"user","content":"hi"}],"stream":true}).to_string())).unwrap()).await.unwrap();
    let mut chunks = response.into_body().into_data_stream();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(chunk) = chunks.next().await {
            if String::from_utf8_lossy(&chunk.unwrap()).contains("hello") {
                return;
            }
        }
        panic!("missing live content");
    })
    .await
    .unwrap();
    tx.send(()).unwrap();
    while let Some(chunk) = chunks.next().await {
        chunk.unwrap();
    }
    server.abort();
}

#[test]
fn native_system_and_tool_result_expansion_preserve_cache_boundaries_and_error_flags() {
    let dir = tempfile::tempdir().unwrap();
    let store = Receipts::new(dir.path().to_owned());
    let request = json!({"model":"anthropic/claude-sonnet-4-6","system":"system","messages":[{"role":"user","content":"read"},{"role":"assistant","content":[{"type":"tool_use","id":"legacy","name":"read","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"legacy","is_error":true,"content":"failed"},{"type":"text","text":"try again"}]}],"x-cache":{"segments":[{"upto_message":2,"stability":"session"}]},"stop_sequences":["END"]});
    let imported = request_to_chat(&request, Wire::Messages, &store, "a").unwrap();
    assert_eq!(
        imported["provider_config"]["x-cache"]["segments"][0]["upto_message"],
        4
    );
    assert_eq!(imported["provider_config"]["stop"], json!(["END"]));
    let native = Anthropic::new("test".into())
        .transform_request(
            "claude-sonnet-4-6",
            &json!({"messages":imported["messages"]}),
        )
        .unwrap();
    assert_eq!(native.body["messages"][2]["content"][0]["is_error"], true);
}

#[test]
fn native_function_strictness_and_admission_output_budget_survive_translation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Receipts::new(dir.path().to_owned());
    let req = json!({"model":"anthropic/claude-sonnet-4-6","max_tokens":u64::MAX,"messages":[],"tools":[{"name":"read","strict":true,"input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}],"stop_sequences":["END"]});
    let imported = request_to_chat(&req, Wire::Messages, &store, "a").unwrap();
    assert!(imported["provider_config"]["tools"][0]["function"]
        .get("description")
        .is_none());
    let typed: llmshim::proxy::types::ChatRequest =
        serde_json::from_value(imported.clone()).unwrap();
    assert_eq!(
        llmshim::proxy::ratelimit::estimate_request_tokens(&typed),
        u32::MAX
    );
    let mut request = imported["provider_config"].clone();
    request["messages"] = json!([]);
    let native = Anthropic::new("test".into())
        .transform_request("claude-sonnet-4-6", &request)
        .unwrap();
    assert_eq!(native.body["tools"][0]["strict"], true);
    assert_eq!(
        native.body["tools"][0]["input_schema"]["required"],
        json!(["path"])
    );
    assert_eq!(native.body["stop_sequences"], json!(["END"]));
    assert!(native.body.get("stop").is_none());
}

#[tokio::test]
async fn upstream_http_errors_become_native_messages_with_inner_types_and_codes() {
    let mut server = mockito::Server::new_async().await;
    let inner = json!({"type":"error","error":{"type":"authentication_error","message":"API key is invalid."}});
    let fixtures = [
        (401, inner.to_string(), "API key is invalid.", "authentication_error", "authentication_error", Value::Null, Value::Null),
        (400, json!({"error":{"message":"Input is too long.","type":"invalid_request_error","code":"context_length_exceeded","param":"messages"}}).to_string(), "Input is too long.", "invalid_request_error", "invalid_request_error", json!("context_length_exceeded"), json!("messages")),
        (401, json!({"error":{"message":"Invalid key.","type":"invalid_request_error","code":"invalid_api_key"}}).to_string(), "Invalid key.", "authentication_error", "invalid_request_error", json!("invalid_api_key"), Value::Null),
        (400, json!({"error":{"message":"Invalid key.","code":"invalid_api_key"}}).to_string(), "Invalid key.", "authentication_error", "authentication_error", json!("invalid_api_key"), Value::Null),
        (400, json!({"error":{"type":"wrapper","message":inner.to_string()}}).to_string(), "API key is invalid.", "authentication_error", "authentication_error", Value::Null, Value::Null),
        (401, "Plain-text failure.".to_owned(), "Plain-text failure.", "authentication_error", "authentication_error", Value::Null, Value::Null),
        (401, "{\"error\":{\"message\":null}}".to_owned(), "{\"error\":{\"message\":null}}", "authentication_error", "authentication_error", Value::Null, Value::Null),
    ];
    let dir = tempfile::tempdir().unwrap();
    let router = Router::new().register(
        "local",
        Box::new(OpenAiCompatible::new("local", &server.url(), None)),
    );
    let application =
        app(router, None).layer(Extension(Arc::new(Receipts::new(dir.path().to_owned()))));
    for (status, upstream_body, message, messages_type, chat_type, code, param) in fixtures {
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_status(status)
            .with_body(upstream_body)
            .expect(2)
            .create_async()
            .await;
        for (path, kind) in [
            ("/v1/messages", messages_type),
            ("/v1/chat/completions", chat_type),
        ] {
            let (actual_status, body) = post(
                application.clone(),
                path,
                json!({"model":"local/test","messages":[{"role":"user","content":"hi"}]}),
            )
            .await;
            assert_eq!(actual_status.as_u16(), status as u16, "{body}");
            assert_eq!(body["error"]["message"], message);
            assert_eq!(body["error"]["type"], kind);
            if path == "/v1/messages" {
                assert_eq!(body["type"], "error");
                assert!(body["error"].get("code").is_none());
            } else {
                assert_eq!(body["error"]["code"], code);
                assert_eq!(body["error"]["param"], param);
            }
        }
        upstream.assert_async().await;
        upstream.remove_async().await;
    }
}

#[tokio::test]
async fn native_stream_errors_unwrap_gateway_display_prefixes() {
    use axum::response::sse::{Event, Sse};
    use std::convert::Infallible;

    let encoded =
        json!({"error":{"type":"server_error","code":"backend_busy","message":"Try again later."}})
            .to_string();
    let handler = move || {
        let message = format!("upstream error: provider error (503): {encoded}");
        async move {
            Sse::new(futures::stream::iter([Ok::<_, Infallible>(
                Event::default()
                    .event("error")
                    .data(json!({"type":"error","message":message}).to_string()),
            )]))
        }
    };
    let application = axum::Router::new()
        .route("/v1/messages", axum::routing::post(handler.clone()))
        .route("/v1/chat/completions", axum::routing::post(handler))
        .layer(axum::middleware::from_fn(llmshim::proxy::wire::translate));
    for (path, kind) in [
        ("/v1/messages", "api_error"),
        ("/v1/chat/completions", "server_error"),
    ] {
        let response = application
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model":"local/test","messages":[],"stream":true}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let error = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .find(|event| event.get("error").is_some())
            .expect("native error event");
        assert_eq!(error["error"]["message"], "Try again later.");
        assert_eq!(error["error"]["type"], kind);
        if path == "/v1/chat/completions" {
            assert_eq!(error["error"]["code"], "backend_busy");
        }
        assert!(!text.contains("data: [DONE]"));
        assert!(!text.contains("event: message_stop"));
    }
}

#[tokio::test]
async fn upstream_authentication_failure_is_readable_in_native_sse() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server.mock("POST", "/chat/completions")
        .with_status(401)
        .with_body(json!({"type":"error","error":{"type":"authentication_error","message":"API key is invalid."}}).to_string())
        .expect(2).create_async().await;
    let router = Router::new().register(
        "local",
        Box::new(OpenAiCompatible::new("local", &server.url(), None)),
    );
    let application = app(router, None);
    for path in ["/v1/messages", "/v1/chat/completions"] {
        let response = application.clone().oneshot(Request::builder()
            .method("POST").uri(path).header("content-type", "application/json")
            .body(Body::from(json!({"model":"local/test","messages":[{"role":"user","content":"hi"}],"stream":true}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let error = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .find(|event| event.get("error").is_some())
            .expect("native error event");
        assert_eq!(error["error"]["message"], "API key is invalid.");
        assert_eq!(error["error"]["type"], "authentication_error");
        assert!(!text.contains("data: [DONE]"));
        assert!(!text.contains("event: message_stop"));
    }
    upstream.assert_async().await;
}

#[tokio::test]
async fn first_class_json_and_sse_errors_have_human_readable_messages() {
    let mut server = mockito::Server::new_async().await;
    let raw = json!({"type":"error","error":{"type":"authentication_error","message":"API key is invalid."},"request_id":null});
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_status(401)
        .with_body(raw.to_string())
        .expect(3)
        .create_async()
        .await;
    let router = Router::new().register(
        "local",
        Box::new(OpenAiCompatible::new("local", &server.url(), None)),
    );
    let application = app(router, None);
    for (path, stream) in [
        ("/v1/chat", false),
        ("/v1/chat", true),
        ("/v1/chat/stream", true),
    ] {
        let response=application.clone().oneshot(Request::builder().method("POST").uri(path).header("content-type","application/json").body(Body::from(json!({"model":"local/test","messages":[{"role":"user","content":"hi"}],"stream":stream}).to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 100000).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        if stream {
            assert_eq!(status, 200);
            let data = text
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            let error: Value = serde_json::from_str(data).unwrap();
            assert_eq!(error["type"], "error");
            assert_eq!(error["message"], "API key is invalid.");
            assert_eq!(error["message"], error["error"]["message"]);
            assert_eq!(error["error"]["type"], "authentication_error");
        } else {
            assert_eq!(status, 401);
            let error: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(error["error"]["message"], "API key is invalid.");
            assert_eq!(error["error"]["code"], "authentication_error");
        }
        assert!(!text.contains("provider error ("));
        assert!(!text.contains("stream error:"));
    }
    upstream.assert_async().await;
}

#[tokio::test]
async fn malformed_tool_calls_are_rejected_before_http_or_sse_dispatch() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_status(401)
        .expect(0)
        .create_async()
        .await;
    let router = Router::new().register(
        "local",
        Box::new(OpenAiCompatible::new("local", &server.url(), None)),
    );
    let application = app(router, None);
    for path in ["/v1/chat", "/v1/chat/stream", "/v1/chat/completions"] {
        for bad in [json!({}), json!("bad"), json!(false), json!(1)] {
            let (status,body)=post(application.clone(),path,json!({"model":"local/test","messages":[{"role":"assistant","content":"answer","tool_calls":bad}]})).await;
            assert_eq!(status, 400, "{body}");
            if path != "/v1/chat/completions" {
                assert_eq!(body["error"]["code"], "invalid_request");
            }
            assert!(body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("tool_calls must be an array"));
        }
    }
    upstream.assert_async().await;
}

#[tokio::test]
async fn unsupported_n_is_refused_in_the_openai_error_shape() {
    // An OpenAI SDK surfaces `param`/`code`; a bare message string leaves the
    // caller guessing which parameter it has to drop.
    let (status, body) = post(
        app(Router::new(), None),
        "/v1/chat/completions",
        json!({"model":"local/test","messages":[{"role":"user","content":"hi"}],"n":2}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["param"], "n");
    assert_eq!(body["error"]["code"], "unsupported_parameter");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("'n'"),
        "the message must name the parameter: {body}"
    );
    assert!(
        body["error"]["message"].as_str().unwrap().len() < 200,
        "no JSON envelope should leak into the message: {body}"
    );

    // `n: 1` is the OpenAI default: it must pass translation and fail later on
    // the unresolvable provider, not on the parameter.
    let (_, body) = post(
        app(Router::new(), None),
        "/v1/chat/completions",
        json!({"model":"local/test","messages":[{"role":"user","content":"hi"}],"n":1}),
    )
    .await;
    assert_ne!(
        body["error"]["code"], "unsupported_parameter",
        "n=1 must not be refused: {body}"
    );
}

#[tokio::test]
async fn native_error_shape_survives_the_anthropic_facade_too() {
    let (status, body) = post(
        app(Router::new(), None),
        "/v1/messages",
        json!({"model":"local/test","messages":[{"role":"user","content":"hi"}],"n":2}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"].as_str().unwrap().contains("'n'"));
}

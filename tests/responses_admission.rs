#![cfg(feature = "proxy")]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use llmshim::providers::openai_compat::OpenAiCompatible;
use llmshim::proxy::ratelimit::{Backpressure, InMemoryRateLimiter, RateLimitConfig};
use llmshim::proxy::AppState;
use llmshim::router::Router;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

fn request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

#[tokio::test]
async fn responses_share_rate_limits_and_errors_with_chat() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server.mock("POST","/chat/completions")
        .with_status(400).with_header("retry-after","7")
        .with_body(json!({"error":{"type":"invalid_request_error","message":"unsupported upstream parameter","param":"temperature","code":"unsupported_parameter"}}).to_string())
        .expect(1).create_async().await;
    let router = Router::new().register(
        "local",
        Box::new(OpenAiCompatible::new("local", server.url(), None)),
    );
    let state = Arc::new(AppState {
        router,
        logger: None,
        limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::with_global(
            Some(1),
            None,
        ))),
        backpressure: Backpressure::new(8, Duration::from_secs(1)),
    });
    let app = llmshim::proxy::app_with_state(state);
    let body = json!({"model":"local/test","input":"hi"}).to_string();
    let response = app.clone().oneshot(request(&body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap();
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(error["error"]["param"], "temperature");
    assert_eq!(error["error"]["code"], "unsupported_parameter");
    let denied = app.oneshot(request(&body)).await.unwrap();
    assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(denied.headers().contains_key("retry-after"));
    let error: Value =
        serde_json::from_slice(&to_bytes(denied.into_body(), 10_000).await.unwrap()).unwrap();
    assert_eq!(error["error"]["type"], "rate_limit_error");
    upstream.assert_async().await;
}

#[tokio::test]
async fn invalid_and_oversized_json_use_responses_errors() {
    let app = llmshim::proxy::app(Router::new(), None);
    for (body, status) in [
        ("{".to_owned(), StatusCode::BAD_REQUEST),
        (
            " ".repeat(2 * 1024 * 1024 + 1),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let response = app.clone().oneshot(request(&body)).await.unwrap();
        assert_eq!(response.status(), status);
        let error: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap();
        assert_eq!(error["error"]["type"], "invalid_request_error");
        assert!(error["error"]["message"].is_string());
    }
    let response = app.clone().oneshot(request(&json!({"model":"missing/test","input":"hi","text":{"format":{"type":"json_schema","schema":{"type":"object"}}}}).to_string())).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap();
    assert_eq!(
        error["error"]["message"],
        "text.format requires schema and name"
    );
    let response = app
        .oneshot(request(
            &json!({"model":"missing/test","input":"hi"}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap();
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("provider"));
}

#[cfg(feature = "gateway")]
#[tokio::test]
async fn gateway_authentication_and_idempotency_apply_to_responses() {
    let directory = tempfile::tempdir_in("target").unwrap();
    let keys = directory.path().join("keys.json");
    std::fs::write(&keys, r#"{"client-key":{"tenant":"test","tier":1}}"#).unwrap();
    std::env::set_var("LLMSHIM_GATEWAY_KEYS_FILE", &keys);
    let mut server = mockito::Server::new_async().await;
    let upstream = server.mock("POST","/chat/completions")
        .with_body(json!({"choices":[{"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],"usage":{}}).to_string())
        .expect(1).create_async().await;
    let state = llmshim::gateway::http::GatewayState::from_env(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        ),
        None,
    );
    std::env::remove_var("LLMSHIM_GATEWAY_KEYS_FILE");
    let app = llmshim::gateway::http::app(state);
    let body = json!({"model":"local/test","input":"hi"}).to_string();
    let response = app.clone().oneshot(request(&body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let error: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap();
    assert_eq!(error["error"]["type"], "authentication_error");
    let mut first = None;
    for replay in [false, true] {
        if replay {
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        let mut request = request(&body);
        request
            .headers_mut()
            .insert("authorization", "Bearer client-key".parse().unwrap());
        request
            .headers_mut()
            .insert("idempotency-key", "responses-test".parse().unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().contains_key("idempotency-replayed"),
            replay
        );
        let response: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap();
        if let Some(first) = &first {
            assert_eq!(&response, first);
        } else {
            first = Some(response.clone());
        }
        assert_eq!(response["object"], "response");
        assert_eq!(response["output"][0]["content"][0]["text"], "Hello");
    }
    upstream.assert_async().await;
}

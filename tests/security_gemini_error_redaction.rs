#![cfg(feature = "proxy")]

use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use llmshim::{
    client::ShimClient, error::ShimError, log::Logger, providers::gemini::Gemini, router::Router,
};
use mockito::Matcher;
use serde_json::json;
use tower::ServiceExt;

const SYNTHETIC_GEMINI_KEY: &str = "a02-synthetic-gemini-key";
const GEMINI_MODEL: &str = "gemini-a02-model";

fn request() -> serde_json::Value {
    json!({
        "model": format!("gemini/{GEMINI_MODEL}"),
        "messages": [{"role": "user", "content": "audit"}],
    })
}

fn provider(base_url: String) -> Gemini {
    Gemini::new(SYNTHETIC_GEMINI_KEY.into()).with_base_url(base_url)
}

fn sensitive_url(base_url: &str) -> String {
    format!("{base_url}/models/{GEMINI_MODEL}:generateContent?key={SYNTHETIC_GEMINI_KEY}")
}

fn assert_error_has_no_sensitive_url(
    error: &ShimError,
    expected_sensitive_url: &str,
    error_kind: &str,
) {
    let rendered_error = error.to_string();
    let debug_error = format!("{error:?}");
    for rendered_value in [&rendered_error, &debug_error] {
        assert!(
            !rendered_value.contains(SYNTHETIC_GEMINI_KEY),
            "{error_kind} error retained the synthetic query credential"
        );
        assert!(
            !rendered_value.contains(expected_sensitive_url),
            "{error_kind} error retained the full request URL"
        );
    }
}

#[tokio::test]
async fn gemini_transport_and_malformed_json_errors_redact_query_credentials() {
    std::env::set_var("LLMSHIM_MAX_RETRIES", "0");

    let closed_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_base_url = format!("http://{}", closed_listener.local_addr().unwrap());
    drop(closed_listener);

    let transport_error = ShimClient::new()
        .completion(&provider(closed_base_url.clone()), GEMINI_MODEL, &request())
        .await
        .expect_err("a closed loopback port must produce a transport error");
    assert!(matches!(transport_error, ShimError::Http(_)));
    assert_error_has_no_sensitive_url(
        &transport_error,
        &sensitive_url(&closed_base_url),
        "transport",
    );

    let mut upstream_server = mockito::Server::new_async().await;
    let malformed_response = upstream_server
        .mock("POST", "/models/gemini-a02-model:generateContent")
        .match_query(Matcher::UrlEncoded(
            "key".into(),
            SYNTHETIC_GEMINI_KEY.into(),
        ))
        .with_status(200)
        .with_body("this is not JSON")
        .expect(2)
        .create_async()
        .await;
    let malformed_base_url = upstream_server.url();
    let expected_sensitive_url = sensitive_url(&malformed_base_url);

    let malformed_json_error = ShimClient::new()
        .completion(
            &provider(malformed_base_url.clone()),
            GEMINI_MODEL,
            &request(),
        )
        .await
        .expect_err("a malformed successful response must fail JSON decoding");
    assert!(matches!(malformed_json_error, ShimError::Http(_)));
    assert_error_has_no_sensitive_url(
        &malformed_json_error,
        &expected_sensitive_url,
        "malformed JSON",
    );

    let log_directory = tempfile::tempdir().unwrap();
    let log_path = log_directory.path().join("proxy.jsonl");
    let logger = Logger::to_file(log_path.to_str().unwrap()).unwrap();
    let router = Router::new().register("gemini", Box::new(provider(malformed_base_url.clone())));
    let response = llmshim::proxy::app(router, Some(logger))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat")
                .header("content-type", "application/json")
                .body(Body::from(request().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    let response_body = to_bytes(response.into_body(), 100_000).await.unwrap();
    let rendered_response = String::from_utf8(response_body.to_vec()).unwrap();
    assert!(!rendered_response.contains(SYNTHETIC_GEMINI_KEY));
    assert!(!rendered_response.contains(&expected_sensitive_url));

    let rendered_log = std::fs::read_to_string(&log_path).unwrap();
    assert!(!rendered_log.contains(SYNTHETIC_GEMINI_KEY));
    assert!(!rendered_log.contains(&expected_sensitive_url));
    malformed_response.assert_async().await;
}

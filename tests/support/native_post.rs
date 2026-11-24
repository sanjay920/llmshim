//! A POST to a native facade, parsed as JSON. Shared by the test binaries that
//! drive more than one wire, so each keeps only its own cases.
use axum::body::{to_bytes, Body};
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

pub async fn post(app: axum::Router, path: &str, body: Value) -> (axum::http::StatusCode, Value) {
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

#![cfg(feature = "proxy")]

use axum::{
    body::Body,
    response::{
        sse::{Event, Sse},
        IntoResponse,
    },
    routing::post,
};
use futures::StreamExt;
use llmshim::{providers::openai_compat::OpenAiCompatible, router::Router};
use serde_json::json;
use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

struct OnDrop(Option<oneshot::Sender<()>>);
impl Drop for OnDrop {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[tokio::test]
async fn disconnect_after_text_closes_upstream_body() {
    let (closed_sender, closed) = oneshot::channel();
    let sender = Arc::new(Mutex::new(Some(closed_sender)));
    let upstream = axum::Router::new().route(
        "/chat/completions",
        post(move || {
            let sender = sender.clone();
            async move {
                let guard = OnDrop(sender.lock().unwrap().take());
                Sse::new(async_stream::stream! {
                    let _guard = guard;
                    loop {
                        let chunk = json!({
                            "id": "upstream",
                            "choices": [
                                {
                                    "index": 0,
                                    "delta": {
                                        "content": "token"
                                    },
                                    "finish_reason": null
                                }
                            ]
                        });
                        yield Ok::<_, Infallible>(Event::default().data(chunk.to_string()));
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = listener.local_addr().unwrap();
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new(
                "local",
                format!("http://{upstream_address}"),
                None,
            )),
        ),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let proxy_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/responses"))
        .json(&json!({"model": "local/test","input": "hi","stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let mut stream = response.bytes_stream();
    let mut received = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !received.contains("response.output_text.delta") {
            received.push_str(std::str::from_utf8(&stream.next().await.unwrap().unwrap()).unwrap());
        }
    })
    .await
    .unwrap();
    assert!(!received.contains("response.completed"));
    drop(stream);
    let closed_result = tokio::time::timeout(Duration::from_secs(5), closed).await;
    proxy_task.abort();
    upstream_task.abort();
    closed_result
        .expect("upstream body remained open after disconnect")
        .unwrap();
}

#[tokio::test]
async fn upstream_error_after_text_is_terminal_failure() {
    use tower::ServiceExt;
    let upstream = axum::Router::new().route(
        "/chat/completions",
        post(|| async {
            let frames = async_stream::stream! {
                yield Ok::<_, std::io::Error>(bytes::Bytes::from(concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"conten",
                    "t\":\"partial\"},\"finish_reason\":null}]}\n\n",
                )));
                yield Ok(bytes::Bytes::from(concat!(
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":",
                    "9,\"completion_tokens\":2,\"total_tokens\":11}}\n\n",
                )));
                tokio::time::sleep(Duration::from_millis(20)).await;
                yield Err(std::io::Error::other("scripted failure"));
            };
            axum::http::Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(frames))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new(
                "local",
                format!("http://{address}"),
                None,
            )),
        ),
        None,
    );
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": "local/test","input": "hi","stream": true}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 100_000)
        .await
        .unwrap();
    task.abort();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("response.failed"), "{text}");
    assert!(!text.contains("response.completed"));
    let terminal: serde_json::Value = serde_json::from_str(
        text.split("\n\n")
            .filter_map(|frame| frame.lines().find_map(|line| line.strip_prefix("data: ")))
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        terminal["response"]["output"][0]["content"][0]["text"],
        "partial"
    );
    assert_eq!(terminal["response"]["usage"]["input_tokens"], 9);
}

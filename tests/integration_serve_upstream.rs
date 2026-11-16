//! `llmshim serve` taking the upstream client and the listener's limits from the
//! file (0021). The harness sets canary `LLMSHIM_*` variables that would break
//! every request or connection if `serve` read them, so every other serve test
//! also proves they are not read.
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{chat, completion, config, start, CALLER_KEY};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

/// The harness file with `[upstream]` keys and `[server]` keys added.
fn render(upstream: &str, server: &str, upstream_table: &str) -> String {
    let text = config(upstream, &format!("\n[upstream]\n{upstream_table}\n"));
    text.replacen("port = 0\n", &format!("port = 0\n{server}\n"), 1)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_retry_setting_decides_how_often_a_failing_provider_is_asked() {
    for (retries, calls) in [(0u32, 1usize), (2, 3)] {
        let mut upstream = mockito::Server::new_async().await;
        let mock = upstream
            .mock("POST", "/chat/completions")
            .with_status(503)
            .with_body("down")
            .expect(calls)
            .create_async()
            .await;
        // A one-second backoff cap keeps the retries quick.
        let text = render(
            &upstream.url(),
            "",
            &format!("max_retries = {retries}\nmax_backoff_secs = 1"),
        );
        let server = start(&text, &format!("upstream-retries-{retries}"), &[]);
        let response = chat(&server, Some(CALLER_KEY), "inference/test-model").await;
        assert!(response.status().as_u16() >= 500, "{}", response.status());
        mock.assert_async().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_configured_header_deadline_closes_a_connection_that_says_nothing() {
    let mut upstream = mockito::Server::new_async().await;
    let _ok = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .create_async()
        .await;
    let text = render(&upstream.url(), "header_timeout_ms = 400", "");
    let server = start(&text, "upstream-header", &[]);
    let address = server.base.trim_start_matches("http://").to_owned();
    let mut idle = tokio::net::TcpStream::connect(&address).await.unwrap();
    let started = Instant::now();
    let mut byte = [0u8; 1];
    // The default is 15 s; the file says 400 ms, so the server closes it far sooner.
    let closed = tokio::time::timeout(Duration::from_secs(8), idle.read(&mut byte)).await;
    assert!(
        matches!(closed, Ok(Ok(0)) | Ok(Err(_))),
        "the silent connection was closed: {closed:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(8));
    // And the server still answers a request that sends its headers.
    let response = chat(&server, Some(CALLER_KEY), "inference/test-model").await;
    assert_eq!(response.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_connection_limit_is_the_files_and_a_second_connection_waits_for_the_first() {
    let upstream = mockito::Server::new_async().await;
    let text = render(
        &upstream.url(),
        "max_connections = 1\nheader_timeout_ms = 60000",
        "",
    );
    let server = start(&text, "upstream-connections", &[]);
    let address = server.base.trim_start_matches("http://").to_owned();
    // The first connection takes the only slot and says nothing.
    let first = tokio::net::TcpStream::connect(&address).await.unwrap();
    // A request that needs a second connection is not served while it is held.
    let waiting = tokio::time::timeout(
        Duration::from_millis(1500),
        chat(&server, Some(CALLER_KEY), "inference/test-model"),
    )
    .await;
    assert!(
        waiting.is_err(),
        "the second connection had to wait: {waiting:?}"
    );
    drop(first);
}

//! `llmshim serve` binding a key to networks (0017): a real server process, a
//! mock upstream, and the caller's address as the socket reports it (loopback).
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{
    completion, config, hangup, rewrite, start, wait_for_reload, ADMIN_TOKEN, CALLER_KEY,
};
use serde_json::json;

async fn upstream(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(calls)
        .create_async()
        .await;
    (upstream, mock)
}

/// The harness file with `allowed_cidrs` on its key, `trusted_proxies` on the
/// server when given, and an admin listener for reloads.
fn render(upstream: &str, cidrs: &str, proxies: Option<&str>) -> String {
    let text = config(
        upstream,
        &format!(
            "allowed_cidrs = {cidrs}\n[admin]\nport = 0\n\
             token = {{ env = \"SERVE_E2E_ADMIN_TOKEN\" }}\n"
        ),
    );
    match proxies {
        Some(proxies) => text.replacen(
            "port = 0\n",
            &format!("port = 0\ntrusted_proxies = {proxies}\n"),
            1,
        ),
        None => text,
    }
}

async fn call(
    base: &str,
    path: &str,
    headers: &[(&str, &str)],
    credential: (&str, &str),
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header(credential.0, credential.1)
        .json(&json!({"model": "inference/test-model",
            "messages": [{"role": "user", "content": "ping"}]}));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

const BEARER: fn() -> (&'static str, String) = || ("authorization", format!("Bearer {CALLER_KEY}"));

async fn chat_status(base: &str, headers: &[(&str, &str)]) -> u16 {
    let (name, value) = BEARER();
    call(base, "/v1/chat/completions", headers, (name, &value))
        .await
        .status()
        .as_u16()
}

fn env() -> [(&'static str, &'static str); 1] {
    [("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_works_from_its_network_and_is_refused_before_the_upstream_elsewhere() {
    let (up, mock) = upstream(1).await;
    let inside = start(
        &render(&up.url(), r#"["127.0.0.0/8", "2001:db8::/32"]"#, None),
        "allow-inside",
        &env(),
    );
    assert_eq!(chat_status(&inside.base, &[]).await, 200);
    mock.assert_async().await;

    let (up, mock) = upstream(0).await;
    let outside = start(
        &render(&up.url(), r#"["10.0.0.0/8"]"#, None),
        "allow-outside",
        &env(),
    );
    let (name, value) = BEARER();
    let refused = call(&outside.base, "/v1/chat/completions", &[], (name, &value)).await;
    assert_eq!(refused.status(), 403);
    let body: serde_json::Value = refused.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("address"),
        "{body}"
    );
    // The caller learns the address is the reason, never what the list holds.
    assert!(!body.to_string().contains("10.0.0.0"), "{body}");
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_forwarded_for_means_nothing_without_a_trusted_proxy() {
    let (up, mock) = upstream(0).await;
    let server = start(
        &render(&up.url(), r#"["10.0.0.0/8"]"#, None),
        "allow-forged",
        &env(),
    );
    assert_eq!(
        chat_status(&server.base, &[("x-forwarded-for", "10.1.2.3")]).await,
        403
    );
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn behind_a_trusted_proxy_the_forwarded_client_is_judged_and_a_bad_header_fails_closed() {
    let (up, mock) = upstream(1).await;
    let server = start(
        &render(&up.url(), r#"["10.0.0.0/8"]"#, Some(r#"["127.0.0.0/8"]"#)),
        "allow-proxy",
        &env(),
    );
    assert_eq!(
        chat_status(&server.base, &[("x-forwarded-for", "10.1.2.3")]).await,
        200,
        "the client the proxy reports is inside"
    );
    for header in [
        Some("203.0.113.9"),
        Some("10.1.2.3, 203.0.113.9"),
        Some("10.1.2.3:443"),
        Some("garbage"),
        Some("127.0.0.1"),
        None,
    ] {
        let headers: Vec<(&str, &str)> =
            header.map(|h| ("x-forwarded-for", h)).into_iter().collect();
        assert_eq!(
            chat_status(&server.base, &headers).await,
            403,
            "{header:?}: the proxy's own address never stands in for the client"
        );
    }
    mock.assert_async().await;
}

/// The proxy's own address is inside the list here, which is how a fallback to it
/// would show: it must still be refused, since the client is unknown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proxy_inside_the_allowed_range_does_not_vouch_for_an_unknown_client() {
    let (up, mock) = upstream(0).await;
    let server = start(
        &render(&up.url(), r#"["127.0.0.0/8"]"#, Some(r#"["127.0.0.0/8"]"#)),
        "allow-proxy-inside",
        &env(),
    );
    assert_eq!(chat_status(&server.base, &[]).await, 403);
    assert_eq!(
        chat_status(&server.base, &[("x-forwarded-for", "garbage")]).await,
        403
    );
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_native_routes_credentials_are_bound_too() {
    let (up, mock) = upstream(0).await;
    let server = start(
        &render(&up.url(), r#"["10.0.0.0/8"]"#, None),
        "allow-native",
        &env(),
    );
    // x-api-key on the Messages route is the key; refused for its address (403),
    // not for being unknown (401).
    let response = call(&server.base, "/v1/messages", &[], ("x-api-key", CALLER_KEY)).await;
    assert_eq!(response.status(), 403);
    // A wrong key is still just unknown, and learns nothing about any list.
    let wrong = call(
        &server.base,
        "/v1/chat/completions",
        &[],
        ("authorization", "Bearer a-wrong-token-0123456789"),
    )
    .await;
    assert_eq!(wrong.status(), 401);
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_that_changes_only_an_allowlist_applies() {
    let (up, _mock) = upstream(1).await;
    let server = start(
        &render(&up.url(), r#"["10.0.0.0/8"]"#, None),
        "allow-reload",
        &env(),
    );
    assert_eq!(chat_status(&server.base, &[]).await, 403);
    rewrite(&server, &render(&up.url(), r#"["127.0.0.0/8"]"#, None));
    hangup(&server);
    wait_for_reload(&server, "generation 1", |status| status["generation"] == 1).await;
    assert_eq!(chat_status(&server.base, &[]).await, 200);
    rewrite(&server, &render(&up.url(), r#"["10.0.0.0/8"]"#, None));
    hangup(&server);
    wait_for_reload(&server, "generation 2", |status| status["generation"] == 2).await;
    assert_eq!(chat_status(&server.base, &[]).await, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gemini_wire_credential_is_bound_and_the_refusal_is_logged_with_the_key() {
    let (up, mock) = upstream(0).await;
    let dir = harness::tempdir::Dir::new("allow-log");
    let log = dir.0.join("access.jsonl");
    let text = render(&up.url(), r#"["10.0.0.0/8"]"#, None)
        + &format!("\n[logging]\naccess_log = \"{}\"\n", log.display());
    let server = start(&text, "allow-log", &env());
    let response = call(
        &server.base,
        "/v1beta/models/gemini-4-argon:generateContent",
        &[],
        ("x-goog-api-key", CALLER_KEY),
    )
    .await;
    assert_eq!(response.status(), 403);
    mock.assert_async().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let line = loop {
        let found: Option<serde_json::Value> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .find(|l: &serde_json::Value| l["status"] == 403);
        if let Some(found) = found {
            break found;
        }
        assert!(std::time::Instant::now() < deadline, "no 403 line");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(line["key"], "ci");
    assert_eq!(line["tenant"], "platform");
}

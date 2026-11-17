//! Deployments (note 0031): a real server process, two mock upstreams as two providers.
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{
    completion, hangup, rewrite, start, tempdir, wait_for_reload, ADMIN_TOKEN, CALLER_KEY,
    UPSTREAM_KEY,
};
use serde_json::{json, Value};
use std::time::Duration;

async fn upstream(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut up = mockito::Server::new_async().await;
    let mock = up
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(calls)
        .create_async()
        .await;
    (up, mock)
}

/// Two openai-compatible providers, `east` and `west`, a deployment `chat`, an admin listener and
/// (when given) a usage log; `key_extra` goes on the one key.
fn render(
    east: &str,
    west: &str,
    deployment: &str,
    key_extra: &str,
    log: Option<&std::path::Path>,
) -> String {
    let logging = log
        .map(|path| format!("[logging]\nusage_log = \"{}\"\n", path.display()))
        .unwrap_or_default();
    format!(
        r#"
        version = 1
        [server]
        port = 0
        [state]
        receipts_dir = "{{DIR}}/receipts"
        [providers.east]
        kind = "openai-compatible"
        base_url = "{east}"
        api_key = {{ env = "SERVE_E2E_UPSTREAM_KEY" }}
        [providers.west]
        kind = "openai-compatible"
        base_url = "{west}"
        api_key = {{ env = "SERVE_E2E_UPSTREAM_KEY" }}
        [[keys]]
        name = "ci"
        tenant = "platform"
        secret = {{ env = "SERVE_E2E_CALLER_KEY" }}
        {key_extra}
        [admin]
        port = 0
        token = {{ env = "SERVE_E2E_ADMIN_TOKEN" }}
        {deployment}
        {logging}
        "#
    )
}

fn env() -> [(&'static str, &'static str); 1] {
    [("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)]
}

async fn ask(base: &str, model: &str, idem: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"model": model,
            "messages": [{"role": "user", "content": "ping"}]}));
    if let Some(key) = idem {
        request = request.header("idempotency-key", key);
    }
    request.send().await.unwrap()
}

const WEIGHTED: &str = r#"
    [deployments.chat]
    strategy = "weighted"
    targets = [{ model = "east/m", weight = 3 }, { model = "west/m", weight = 1 }]
"#;
const ROUND_ROBIN: &str = r#"
    [deployments.chat]
    targets = [{ model = "east/m" }, { model = "west/m" }]
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_reach_the_targets_in_the_weighted_ratio_and_the_usage_record_names_both() {
    // Eight requests, 3:1: six to east, two to west, exactly (smooth weighted round robin).
    let (east, east_mock) = upstream(6).await;
    let (west, west_mock) = upstream(2).await;
    let dir = tempdir::Dir::new("dep-ratio");
    let log = dir.0.join("usage.jsonl");
    let server = start(
        &render(&east.url(), &west.url(), WEIGHTED, "", Some(&log)),
        "dep-ratio",
        &env(),
    );
    for _ in 0..8 {
        assert_eq!(
            ask(&server.base, "deployment/chat", None).await.status(),
            200
        );
    }
    east_mock.assert_async().await;
    west_mock.assert_async().await;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let records = loop {
        let found: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if found.len() >= 8 || std::time::Instant::now() > deadline {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(records.len(), 8, "{records:?}");
    for r in &records {
        // The caller addressed the deployment; the record is of the target that served it.
        assert_eq!(r["deployment"], "chat");
        assert!(r["provider"] == "east" || r["provider"] == "west", "{r}");
        assert_eq!(r["model"], "m");
    }
    assert_eq!(
        records.iter().filter(|r| r["provider"] == "east").count(),
        6
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_allowlist_sees_the_pick_and_a_list_naming_the_deployment_allows_every_target() {
    // A key allowed only east: round robin sends every other request to west, which is refused,
    // and exactly the east requests reach the provider.
    let (east, east_mock) = upstream(2).await;
    let (west, west_mock) = upstream(0).await;
    let server = start(
        &render(
            &east.url(),
            &west.url(),
            ROUND_ROBIN,
            "allowed_models = [\"east/m\"]",
            None,
        ),
        "dep-allow",
        &env(),
    );
    let statuses: Vec<u16> = {
        let mut v = Vec::new();
        for _ in 0..4 {
            v.push(
                ask(&server.base, "deployment/chat", None)
                    .await
                    .status()
                    .as_u16(),
            );
        }
        v
    };
    assert_eq!(statuses, [200, 403, 200, 403]);
    east_mock.assert_async().await;
    west_mock.assert_async().await;
    // Naming the deployment admits whichever target it picks.
    let (east2, east_mock2) = upstream(2).await;
    let (west2, west_mock2) = upstream(2).await;
    let server2 = start(
        &render(
            &east2.url(),
            &west2.url(),
            ROUND_ROBIN,
            "allowed_models = [\"deployment/chat\"]",
            None,
        ),
        "dep-allow-name",
        &env(),
    );
    for _ in 0..4 {
        assert_eq!(
            ask(&server2.base, "deployment/chat", None).await.status(),
            200
        );
    }
    // But the targets themselves are not on the list.
    assert_eq!(ask(&server2.base, "east/m", None).await.status(), 403);
    east_mock2.assert_async().await;
    west_mock2.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idempotent_retry_that_would_pick_the_other_target_is_a_replay_not_a_conflict() {
    // One call reaches a provider in all: the retry is answered from the cache.
    let (east, east_mock) = upstream(1).await;
    let (west, west_mock) = upstream(0).await;
    let server = start(
        &render(&east.url(), &west.url(), ROUND_ROBIN, "", None),
        "dep-idem",
        &env(),
    );
    let first = ask(&server.base, "deployment/chat", Some("k1")).await;
    assert_eq!(first.status(), 200);
    // Round robin would now pick west; the same key and request replay instead.
    let retry = ask(&server.base, "deployment/chat", Some("k1")).await;
    assert_eq!(retry.status(), 200, "{}", retry.text().await.unwrap());
    east_mock.assert_async().await;
    west_mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_changes_the_targets_and_unknown_names_are_a_400() {
    let (east, _e) = upstream(0).await;
    let (west, west_mock) = upstream(3).await;
    let server = start(
        &render(&east.url(), &west.url(), ROUND_ROBIN, "", None),
        "dep-reload",
        &env(),
    );
    assert_eq!(
        ask(&server.base, "deployment/nope", None).await.status(),
        400
    );
    // Remove east from the deployment: every request goes west.
    rewrite(
        &server,
        &render(
            &east.url(),
            &west.url(),
            "[deployments.chat]\ntargets = [{ model = \"west/m\" }]",
            "",
            None,
        ),
    );
    hangup(&server);
    wait_for_reload(&server, "router generation 1", |s| {
        s["router_generation"] == 1
    })
    .await;
    for _ in 0..3 {
        assert_eq!(
            ask(&server.base, "deployment/chat", None).await.status(),
            200
        );
    }
    west_mock.assert_async().await;
}

#[test]
fn a_deployment_is_checked_by_path() {
    let _ = UPSTREAM_KEY;
    use std::process::Command;
    for (table, path) in [
        ("[deployments.chat]\ntargets = []", "deployments.chat.targets"),
        (
            "[deployments.chat]\ntargets = [{ model = \"nowhere/m\" }]",
            "deployments.chat.targets[0].model",
        ),
        (
            "[deployments.chat]\ntargets = [{ model = \"route/x\" }]",
            "deployments.chat.targets[0].model",
        ),
        (
            "[deployments.chat]\ntargets = [{ model = \"east/m\" }, { model = \"east/m\" }]",
            "deployments.chat.targets[1].model",
        ),
        (
            "[deployments.chat]\ntargets = [{ model = \"east/m\", weight = 2 }]",
            "deployments.chat.targets[0].weight",
        ),
        (
            "[deployments.chat]\nstrategy = \"weighted\"\ntargets = [{ model = \"east/m\", weight = 5000 }]",
            "deployments.chat.targets[0].weight",
        ),
        (
            "[deployments.chat]\nstrategy = \"nope\"\ntargets = [{ model = \"east/m\" }]",
            "unknown value, expected one of",
        ),
        ("[deployments.Bad_Name]\ntargets = [{ model = \"east/m\" }]", "deployments.Bad_Name"),
    ] {
        let dir = tempdir::Dir::new("dep-bad");
        let file = dir.0.join("gateway.toml");
        std::fs::write(
            &file,
            render("http://127.0.0.1:1", "http://127.0.0.1:2", table, "", None)
                .replace("{DIR}", dir.0.to_str().unwrap()),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
            .args(["serve", "--config", file.to_str().unwrap(), "--check", "--structure-only"])
            .env_clear()
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stderr).to_string()
            + &String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(1), "{table}: {text}");
        assert!(text.contains(path), "{table}: {text}");
    }
    let _ = CALLER_KEY;
}

// ---- failover (note 0031, slice 3) ----------------------------------------------------------

/// `[upstream] max_retries = 0` keeps the client from retrying a 5xx itself, so what the tests count
/// is the deployment's own failover (a file's `[upstream]` is the only place `serve` reads that).
const ORDERED: &str = r#"
    [deployments.chat]
    strategy = "ordered"
    targets = [{ model = "east/m" }, { model = "west/m" }]
    [upstream]
    max_retries = 0
"#;

/// A mock that answers every call with `status`.
async fn failing(status: usize, calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut up = mockito::Server::new_async().await;
    let mock = up
        .mock("POST", "/chat/completions")
        .with_status(status)
        .with_header("content-type", "application/json")
        .with_body("{\"error\":{\"message\":\"nope\"}}")
        .expect(calls)
        .create_async()
        .await;
    (up, mock)
}

/// No retries inside the client: each target is hit once per attempt, so what the test counts is
/// the deployment's own failover.
fn no_client_retries() -> [(&'static str, &'static str); 2] {
    [
        ("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN),
        ("LLMSHIM_MAX_RETRIES", "0"),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_5xx_from_the_first_target_is_retried_on_the_next_and_each_attempt_is_recorded() {
    let (east, east_mock) = failing(502, 1).await;
    let (west, west_mock) = upstream(1).await;
    let dir = tempdir::Dir::new("dep-failover");
    let log = dir.0.join("usage.jsonl");
    let server = start(
        &render(&east.url(), &west.url(), ORDERED, "", Some(&log)),
        "dep-failover",
        &no_client_retries(),
    );
    assert_eq!(
        ask(&server.base, "deployment/chat", None).await.status(),
        200
    );
    east_mock.assert_async().await;
    west_mock.assert_async().await;
    // Two usage records, one per attempt, under one request id and the one deployment name.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let records = loop {
        let found: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if found.len() >= 2 || std::time::Instant::now() > deadline {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(records.len(), 2, "{records:?}");
    assert!(records.iter().all(|r| r["deployment"] == "chat"));
    let by = |p: &str| records.iter().find(|r| r["provider"] == p).unwrap().clone();
    assert_eq!(by("east")["outcome"], "http_failure");
    assert_eq!(by("west")["outcome"], "completed");
    assert_eq!(by("east")["request_id"], by("west")["request_id"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_400_or_a_429_is_not_retried_elsewhere() {
    for status in [400, 429] {
        let (east, east_mock) = failing(status, 1).await;
        // The second target must never be called.
        let (west, west_mock) = upstream(0).await;
        let server = start(
            &render(&east.url(), &west.url(), ORDERED, "", None),
            "dep-no-failover",
            &no_client_retries(),
        );
        let answer = ask(&server.base, "deployment/chat", None).await;
        assert_ne!(answer.status(), 200, "{status}");
        east_mock.assert_async().await;
        west_mock.assert_async().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_target_is_tried_once_and_the_last_failure_is_what_the_caller_sees() {
    let (east, east_mock) = failing(502, 1).await;
    let (west, west_mock) = failing(503, 1).await;
    let server = start(
        &render(&east.url(), &west.url(), ORDERED, "", None),
        "dep-all-fail",
        &no_client_retries(),
    );
    let answer = ask(&server.base, "deployment/chat", None).await;
    assert!(answer.status().is_server_error(), "{}", answer.status());
    east_mock.assert_async().await;
    west_mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_the_key_may_not_make_returns_the_original_failure_not_a_403() {
    // The key may call east only. East fails with a 502; west would be the retry, which the list does
    // not allow: the caller sees the 502 it earned, and west is never called.
    let (east, east_mock) = failing(502, 1).await;
    let (west, west_mock) = upstream(0).await;
    let server = start(
        &render(
            &east.url(),
            &west.url(),
            ORDERED,
            "allowed_models = [\"east/m\"]",
            None,
        ),
        "dep-failover-allow",
        &no_client_retries(),
    );
    let answer = ask(&server.base, "deployment/chat", None).await;
    assert_eq!(answer.status(), 502);
    east_mock.assert_async().await;
    west_mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_addressed_to_a_target_is_never_moved() {
    let (east, east_mock) = failing(502, 1).await;
    let (west, west_mock) = upstream(0).await;
    let server = start(
        &render(&east.url(), &west.url(), ORDERED, "", None),
        "dep-direct",
        &no_client_retries(),
    );
    assert_eq!(ask(&server.base, "east/m", None).await.status(), 502);
    east_mock.assert_async().await;
    west_mock.assert_async().await;
}

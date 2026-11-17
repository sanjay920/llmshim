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

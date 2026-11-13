//! `llmshim serve` as a fleet: two real server processes sharing one Redis.
//!
//! Needs a Redis instance: `LLMSHIM_TEST_REDIS_URL=redis://127.0.0.1:6379
//! cargo test --features serve-redis --test integration_serve_fleet -- --ignored`
//! The `Serve feature` CI job runs this against a Redis service container.
#![cfg(feature = "serve-redis")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{chat, completion, config, start, CALLER_KEY};
use serde_json::Value;

fn redis_url() -> String {
    std::env::var("LLMSHIM_TEST_REDIS_URL").expect("set LLMSHIM_TEST_REDIS_URL")
}

/// Names no earlier run or concurrent test has used. In fleet mode the queue,
/// the rate buckets and the spend ledger are shared through Redis and keyed by
/// provider name and tenant, so tests that share a Redis must not share either:
/// a replica of one test would otherwise take another test's queued job.
struct Names {
    provider: String,
    tenant: String,
}

impl Names {
    fn new() -> Self {
        let id = uuid::Uuid::new_v4().simple();
        Self {
            provider: format!("fleet-{id}"),
            tenant: format!("tenant-{id}"),
        }
    }

    fn model(&self) -> String {
        format!("{}/test-model", self.provider)
    }
}

fn fleet_config(upstream: &str, names: &Names, key_limits: &str) -> String {
    let base = config(
        upstream,
        "[fleet]\nredis_url = { env = \"SERVE_E2E_REDIS_URL\" }",
    );
    base.replace(
        "providers.inference",
        &format!("providers.{}", names.provider),
    )
    .replace(
        "tenant = \"platform\"",
        &format!("tenant = \"{}\"\n{key_limits}", names.tenant),
    )
}

#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn two_replicas_serve_traffic_and_report_fleet_mode() {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(2)
        .create_async()
        .await;
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    let names = Names::new();
    let text = fleet_config(&upstream.url(), &names, "");
    let a = start(&text, "fleet-a", &env);
    let b = start(&text, "fleet-b", &env);

    for server in [&a, &b] {
        let response = chat(server, Some(CALLER_KEY), &names.model()).await;
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "pong");
    }
    mock.assert_async().await;

    let stats: Value = reqwest::Client::new()
        .get(format!("{}/v1/gateway/stats", a.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["mode"], "distributed", "{stats}");
}

#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn a_tenant_limit_is_shared_across_replicas() {
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect_at_most(3)
        .create_async()
        .await;
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    // Three requests per minute for the tenant, fleet-wide.
    let names = Names::new();
    let text = fleet_config(&upstream.url(), &names, "rpm = 3");
    let a = start(&text, "limit-a", &env);
    let b = start(&text, "limit-b", &env);

    let mut statuses = Vec::new();
    for server in [&a, &b, &a, &b] {
        statuses.push(
            chat(server, Some(CALLER_KEY), &names.model())
                .await
                .status()
                .as_u16(),
        );
    }
    // With per-process limits each replica would allow all of its two requests.
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        3,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 429).count(),
        1,
        "{statuses:?}"
    );
}

#[test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
fn an_unreachable_redis_fails_the_start_without_leaking_the_url() {
    let dir = harness::tempdir::Dir::new("fleet-down");
    let path = dir.0.join("gateway.toml");
    let text = fleet_config("http://127.0.0.1:1", &Names::new(), "")
        .replace("{DIR}", dir.0.to_str().unwrap());
    std::fs::write(&path, text).unwrap();
    let secret_url = "redis://:hunter2hunter2@127.0.0.1:1/0";
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["serve", "--config", path.to_str().unwrap()])
        .env_clear()
        .env("LLMSHIM_CATALOG_OFFLINE", "1")
        .env("SERVE_E2E_CALLER_KEY", CALLER_KEY)
        .env("SERVE_E2E_UPSTREAM_KEY", harness::UPSTREAM_KEY)
        .env("SERVE_E2E_REDIS_URL", secret_url)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("fleet.redis_url: cannot start fleet mode"),
        "{stderr}"
    );
    assert!(!stderr.contains("hunter2hunter2"), "{stderr}");
    assert!(!stderr.contains("listening on"), "{stderr}");
}

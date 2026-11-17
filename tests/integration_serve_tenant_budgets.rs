//! Tenant budgets above key budgets (note 0030): a real server process and a mock
//! upstream that must not be reached by a request the ledger refuses.
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{
    completion, config, hangup, rewrite, start, tempdir, wait_for_reload, ADMIN_TOKEN, CALLER_KEY,
};
use serde_json::json;

const FREE_KEY: &str = "free-caller-key-0123456789abcdef";

fn responses_body(text: &str) -> String {
    json!({
        "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-6-luna",
        "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
    })
    .to_string()
}

/// Two keys of tenant `platform`: `ci` with its own cap (`ci_cap`), and `free` with none, and
/// `table` (a `[tenants.platform]` section, or nothing).
fn render(upstream: &str, ci_cap: &str, table: &str) -> String {
    config(
        upstream,
        &format!(
            "{ci_cap}\n[[keys]]\nname = \"free\"\ntenant = \"platform\"\n\
             secret = {{ env = \"SERVE_E2E_FREE_KEY\" }}\n{table}\n"
        ),
    )
    .replace("kind = \"openai-compatible\"", "kind = \"openai\"")
    .replace("providers.inference", "providers.openai")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_that_changes_the_tenant_cap_applies_to_the_next_request() {
    let (up, _mock) = upstream(1).await;
    let admin = "[admin]\nport = 0\ntoken = { env = \"SERVE_E2E_ADMIN_TOKEN\" }\n";
    let with = |cap: &str| {
        render(
            &up.url(),
            "",
            &format!("[tenants.platform]\nbudget_usd = {cap}"),
        ) + admin
    };
    let env = [
        ("SERVE_E2E_FREE_KEY", FREE_KEY),
        ("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN),
    ];
    let server = start(&with("0.0"), "tb-reload", &env);
    assert_eq!(ask(&server.base, FREE_KEY).await, 429);
    rewrite(&server, &with("100.0"));
    hangup(&server);
    wait_for_reload(&server, "generation 1", |s| s["generation"] == 1).await;
    assert_eq!(ask(&server.base, FREE_KEY).await, 200);
}

async fn ask(base: &str, key: &str) -> u16 {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(key)
        .json(&json!({"model": "openai/gpt-6-luna", "max_tokens": 16,
            "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn upstream(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/responses")
        .with_header("content-type", "application/json")
        .with_body(responses_body("pong"))
        .expect(calls)
        .create_async()
        .await;
    (upstream, mock)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_cap_refuses_its_key_while_the_tenant_has_room_and_the_other_key_is_served() {
    // One call reaches the provider: the uncapped key's.
    let (up, mock) = upstream(1).await;
    let server = start(
        &render(
            &up.url(),
            "budget_usd = 0.0",
            "[tenants.platform]\nbudget_usd = 100.0",
        ),
        "tb-key",
        &[("SERVE_E2E_FREE_KEY", FREE_KEY)],
    );
    assert_eq!(ask(&server.base, CALLER_KEY).await, 429);
    assert_eq!(ask(&server.base, FREE_KEY).await, 200);
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_tenant_cap_refuses_every_key_of_the_tenant() {
    let (up, mock) = upstream(0).await;
    let server = start(
        &render(
            &up.url(),
            "budget_usd = 100.0",
            "[tenants.platform]\nbudget_usd = 0.0",
        ),
        "tb-tenant",
        &[("SERVE_E2E_FREE_KEY", FREE_KEY)],
    );
    assert_eq!(ask(&server.base, CALLER_KEY).await, 429);
    assert_eq!(ask(&server.base, FREE_KEY).await, 429);
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_table_a_keys_budget_works_as_it_always_did() {
    let (up, mock) = upstream(1).await;
    let server = start(
        &render(&up.url(), "budget_usd = 0.0", ""),
        "tb-none",
        &[("SERVE_E2E_FREE_KEY", FREE_KEY)],
    );
    assert_eq!(ask(&server.base, CALLER_KEY).await, 429);
    // The uncapped key has no budget and is not limited by its sibling's.
    assert_eq!(ask(&server.base, FREE_KEY).await, 200);
    mock.assert_async().await;
}

fn check(text: &str, label: &str) -> (Option<i32>, String) {
    let dir = tempdir::Dir::new(label);
    let file = dir.0.join("gateway.toml");
    std::fs::write(&file, text.replace("{DIR}", dir.0.to_str().unwrap())).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args([
            "serve",
            "--config",
            file.to_str().unwrap(),
            "--check",
            "--structure-only",
        ])
        .env_clear()
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).to_string()
            + &String::from_utf8_lossy(&output.stdout),
    )
}

#[test]
fn a_table_is_checked_by_path() {
    for (table, path) in [
        ("[tenants.nobody]\nbudget_usd = 1.0", "tenants.nobody"),
        (
            "[tenants.platform]\nbudget_usd = -1.0",
            "tenants.platform.budget_usd",
        ),
        (
            "[tenants.platform]\nbudget_usd = 1e300",
            "tenants.platform.budget_usd",
        ),
        (
            "[tenants.platform]\nbudget_usd = 1.0\nbudget_window_secs = 0",
            "tenants.platform.budget_window_secs",
        ),
        (
            "[tenants.platform]\nbudget_usd = 1.0\nsurprise = 1",
            "unknown field",
        ),
    ] {
        let (code, text) = check(&render("http://127.0.0.1:1", "", table), "tb-bad");
        assert_eq!(code, Some(1), "{table}: {text}");
        assert!(text.contains(path), "{table}: {text}");
    }
    // A fleet has them (the shared ledger holds both counters).
    let fleet = render(
        "http://127.0.0.1:1",
        "",
        "[tenants.platform]\nbudget_usd = 1.0\n[fleet]\nredis_url = { env = \"R\" }",
    );
    let (code, text) = check(&fleet, "tb-fleet");
    assert_eq!(code, Some(0), "{text}");
    // A good table passes.
    let (code, text) = check(
        &render(
            "http://127.0.0.1:1",
            "",
            "[tenants.platform]\nbudget_usd = 1.0\nbudget_window_secs = 3600",
        ),
        "tb-ok",
    );
    assert_eq!(code, Some(0), "{text}");
    let _ = completion("x");
}

/// The same two levels across a fleet: the shared Redis ledger refuses the capped key and
/// serves the other, with the tenant cap above them. Needs Redis; run one test at a time
/// (the queue is keyed by provider name):
/// `LLMSHIM_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test --features serve-redis
/// --test integration_serve_tenant_budgets -- --ignored --test-threads=1`
#[cfg(feature = "serve-redis")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn a_fleet_replica_enforces_both_levels_through_the_shared_ledger() {
    let redis = std::env::var("LLMSHIM_TEST_REDIS_URL").expect("set LLMSHIM_TEST_REDIS_URL");
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4().simple());
    let (up, mock) = upstream(1).await;
    let text = render(
        &up.url(),
        "budget_usd = 0.0",
        "[tenants.platform]\nbudget_usd = 100.0\n[fleet]\nredis_url = { env = \"SERVE_E2E_REDIS_URL\" }",
    )
    .replace("platform", &tenant);
    let server = start(
        &text,
        "tb-fleet",
        &[
            ("SERVE_E2E_FREE_KEY", FREE_KEY),
            ("SERVE_E2E_REDIS_URL", redis.as_str()),
        ],
    );
    assert_eq!(ask(&server.base, CALLER_KEY).await, 429);
    assert_eq!(ask(&server.base, FREE_KEY).await, 200);
    mock.assert_async().await;
}

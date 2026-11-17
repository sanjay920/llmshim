//! Two accounts of one hosted vendor (note 0032): two `kind = "openai"` entries on two mock upstreams.
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{start, tempdir, ADMIN_TOKEN, CALLER_KEY};
use serde_json::{json, Value};
use std::time::Duration;

fn responses_body() -> String {
    json!({
        "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-6-luna",
        "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": "pong", "annotations": []}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
    })
    .to_string()
}

async fn upstream(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut up = mockito::Server::new_async().await;
    let mock = up
        .mock("POST", "/responses")
        .with_header("content-type", "application/json")
        .with_body(responses_body())
        .expect(calls)
        .create_async()
        .await;
    (up, mock)
}

/// `openai` and `openai-b`, both hosted OpenAI entries on their own mock; `extra` goes to the end.
fn render(a: &str, b: &str, key_extra: &str, extra: &str) -> String {
    format!(
        r#"
        version = 1
        [server]
        port = 0
        [state]
        receipts_dir = "{{DIR}}/receipts"
        [providers.openai]
        kind = "openai"
        base_url = "{a}"
        api_key = {{ env = "SERVE_E2E_UPSTREAM_KEY" }}
        [providers.openai-b]
        kind = "openai"
        base_url = "{b}"
        api_key = {{ env = "SERVE_E2E_UPSTREAM_KEY" }}
        [[keys]]
        name = "ci"
        tenant = "platform"
        secret = {{ env = "SERVE_E2E_CALLER_KEY" }}
        {key_extra}
        [admin]
        port = 0
        token = {{ env = "SERVE_E2E_ADMIN_TOKEN" }}
        {extra}
        "#
    )
}

fn env() -> [(&'static str, &'static str); 1] {
    [("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)]
}

async fn ask(base: &str, model: &str) -> u16 {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"model": model, "max_tokens": 16,
            "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_account_has_its_own_lane_limit_and_usage_record_and_is_priced_as_the_kind() {
    let (a, a_mock) = upstream(3).await;
    let (b, b_mock) = upstream(1).await;
    let dir = tempdir::Dir::new("inst-limits");
    let log = dir.0.join("usage.jsonl");
    // The second account is limited to one request a minute; the first is not. The key has a
    // budget, so a priced model must be priceable through the second account too (the catalog is
    // asked about the kind, `openai`, not the instance).
    let extra = format!(
        "[limits.providers.openai-b]\nrpm = 1\n[logging]\nusage_log = \"{}\"\n",
        log.display()
    );
    let server = start(
        &render(&a.url(), &b.url(), "budget_usd = 100.0", &extra),
        "inst-limits",
        &env(),
    );
    assert_eq!(ask(&server.base, "openai-b/gpt-6-luna").await, 200);
    // A second request to the limited account is refused by ITS bucket...
    assert_eq!(ask(&server.base, "openai-b/gpt-6-luna").await, 429);
    // ...while the other account, unlimited and in its own lane, serves.
    for _ in 0..3 {
        assert_eq!(ask(&server.base, "openai/gpt-6-luna").await, 200);
    }
    a_mock.assert_async().await;
    b_mock.assert_async().await;
    // The usage records name the account that served each attempt.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let records = loop {
        let found: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if found.len() >= 4 || std::time::Instant::now() > deadline {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let count = |p: &str| records.iter().filter(|r| r["provider"] == p).count();
    assert_eq!((count("openai"), count("openai-b")), (3, 1), "{records:?}");
    // Both lanes exist.
    let stats: Value = reqwest::Client::new()
        .get(format!(
            "{}/v1/gateway/stats",
            server.admin.clone().expect("an [admin] listener")
        ))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let lanes: Vec<String> = stats["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["provider"].as_str().unwrap().to_string())
        .collect();
    assert!(lanes.contains(&"openai".to_string()), "{lanes:?}");
    assert!(lanes.contains(&"openai-b".to_string()), "{lanes:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deployment_over_two_accounts_balances_between_them() {
    let (a, a_mock) = upstream(6).await;
    let (b, b_mock) = upstream(2).await;
    let extra = r#"
        [deployments.chat]
        strategy = "weighted"
        targets = [{ model = "openai/gpt-6-luna", weight = 3 }, { model = "openai-b/gpt-6-luna", weight = 1 }]
    "#;
    let server = start(&render(&a.url(), &b.url(), "", extra), "inst-dep", &env());
    for _ in 0..8 {
        assert_eq!(ask(&server.base, "deployment/chat").await, 200);
    }
    a_mock.assert_async().await;
    b_mock.assert_async().await;
}

#[test]
fn the_name_rule_allows_instances_and_still_protects_reserved_names() {
    use std::process::Command;
    let check = |extra_provider: &str| -> (Option<i32>, String) {
        let dir = tempdir::Dir::new("inst-names");
        let file = dir.0.join("gateway.toml");
        std::fs::write(
            &file,
            render(
                "http://127.0.0.1:1",
                "http://127.0.0.1:2",
                "",
                extra_provider,
            )
            .replace("{DIR}", dir.0.to_str().unwrap()),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
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
    };
    // Two OpenAI entries are fine.
    let (code, text) = check("");
    assert_eq!(code, Some(0), "{text}");
    // An OpenAI entry may not take another kind's reserved name.
    let (code, text) = check(
        "[providers.anthropic]\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:3\"\napi_key = { env = \"K\" }",
    );
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("providers.anthropic"), "{text}");
    // `deployment` and `route` can never be reached as provider names.
    for name in ["deployment", "route"] {
        let (code, text) = check(&format!(
            "[providers.{name}]\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:3\"\napi_key = {{ env = \"K\" }}"
        ));
        assert_eq!(code, Some(1), "{name}: {text}");
        assert!(text.contains(&format!("providers.{name}")), "{text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_429_from_one_account_penalizes_that_accounts_bucket_and_not_the_others() {
    let (a, a_mock) = upstream(1).await;
    // The second account always answers 429.
    let mut b = mockito::Server::new_async().await;
    let _b_mock = b
        .mock("POST", "/responses")
        .with_status(429)
        .with_header("content-type", "application/json")
        .with_body("{\"error\":{\"message\":\"slow down\"}}")
        .expect_at_least(1)
        .create_async()
        .await;
    // Both accounts have a bucket (a penalty applies to a bucket that exists).
    let extra = "[limits.providers.openai]\nrpm = 600\n[limits.providers.openai-b]\nrpm = 600\n";
    let server = start(&render(&a.url(), &b.url(), "", extra), "inst-429", &env());
    assert_eq!(ask(&server.base, "openai-b/gpt-6-luna").await, 429);
    // The first account's bucket was not penalized by the second's 429: it still serves at once.
    assert_eq!(ask(&server.base, "openai/gpt-6-luna").await, 200);
    a_mock.assert_async().await;
}

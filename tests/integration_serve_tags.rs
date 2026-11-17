//! Key tags on usage records (note 0029): a real server process, a mock upstream and
//! the usage log read back from disk.
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{completion, config, hangup, rewrite, start, tempdir, ADMIN_TOKEN, CALLER_KEY};
use serde_json::{json, Value};
use std::time::Duration;

const SECOND_KEY: &str = "second-caller-key-0123456789abcdef";

async fn upstream() -> (mockito::ServerGuard, mockito::Mock) {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect_at_least(1)
        .create_async()
        .await;
    (upstream, mock)
}

/// The harness file: key `ci` with `ci_tags` (a TOML inline table, or none), a second key
/// `plain` with no tags, an admin listener for reloads and a usage log.
fn render(upstream: &str, ci_tags: &str, log: &std::path::Path) -> String {
    config(
        upstream,
        &format!(
            "{ci_tags}\n\
             [[keys]]\nname = \"plain\"\ntenant = \"platform\"\n\
             secret = {{ env = \"SERVE_E2E_SECOND_KEY\" }}\n\
             [admin]\nport = 0\ntoken = {{ env = \"SERVE_E2E_ADMIN_TOKEN\" }}\n\
             [logging]\nusage_log = \"{}\"\n",
            log.display()
        ),
    )
}

fn env() -> [(&'static str, &'static str); 2] {
    [
        ("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN),
        ("SERVE_E2E_SECOND_KEY", SECOND_KEY),
    ]
}

async fn ask(base: &str, key: &str) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(key)
        .json(&json!({"model": "inference/test-model",
            "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

/// Wait until the log holds `count` usage records and return them.
async fn records(log: &std::path::Path, count: usize) -> Vec<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let found: Vec<Value> = std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|record: &Value| record["event"] == "usage")
            .collect();
        if found.len() >= count || std::time::Instant::now() > deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keys_tags_are_on_its_usage_records_sorted_and_an_untagged_key_has_none() {
    let (up, _mock) = upstream().await;
    let dir = tempdir::Dir::new("tags");
    let log = dir.0.join("usage.jsonl");
    let server = start(
        &render(
            &up.url(),
            r#"tags = { product = "checkout", cost_center = "cc-4417", env = "prod" }"#,
            &log,
        ),
        "tags",
        &env(),
    );
    ask(&server.base, CALLER_KEY).await;
    ask(&server.base, SECOND_KEY).await;
    let found = records(&log, 2).await;
    let of = |name: &str| found.iter().find(|r| r["key"] == name).unwrap().clone();
    assert_eq!(
        of("ci")["tags"],
        json!({"cost_center": "cc-4417", "env": "prod", "product": "checkout"})
    );
    // Sorted on the wire, not only equal as a value.
    let line = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .find(|l| l.contains("\"key\":\"ci\""))
        .unwrap()
        .to_string();
    let (a, b, c) = (
        line.find("cost_center").unwrap(),
        line.find("\"env\"").unwrap(),
        line.find("\"product\"").unwrap(),
    );
    assert!(a < b && b < c, "{line}");
    assert_eq!(of("plain")["tags"], json!({}));
}

/// A tags-only reload is not a new generation (who may call is unchanged), so wait for it by
/// asking until the newest record carries the expected tags.
async fn until_tagged(server: &harness::Server, log: &std::path::Path, expected: Value) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let before = records(log, 0).await.len();
        ask(&server.base, CALLER_KEY).await;
        let found = records(log, before + 1).await;
        if found.last().map(|r| r["tags"].clone()) == Some(expected.clone()) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "tags never became {expected}: {:?}",
            found.last()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_changes_the_tags_of_the_next_record_and_removing_them_leaves_none() {
    let (up, _mock) = upstream().await;
    let dir = tempdir::Dir::new("tags-reload");
    let log = dir.0.join("usage.jsonl");
    let server = start(
        &render(&up.url(), r#"tags = { env = "prod" }"#, &log),
        "tags-reload",
        &env(),
    );
    until_tagged(&server, &log, json!({"env": "prod"})).await;
    rewrite(
        &server,
        &render(&up.url(), r#"tags = { env = "staging", team = "a" }"#, &log),
    );
    hangup(&server);
    until_tagged(&server, &log, json!({"env": "staging", "team": "a"})).await;
    // Removing every tag yields none (`{}`), not the old tags.
    rewrite(&server, &render(&up.url(), "", &log));
    hangup(&server);
    until_tagged(&server, &log, json!({})).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_mode_has_no_key_and_so_null_tags() {
    let (up, _mock) = upstream().await;
    let dir = tempdir::Dir::new("tags-open");
    let log = dir.0.join("usage.jsonl");
    let text = format!(
        "version = 1\n[server]\nport = 0\nallow_unauthenticated = true\n\
         [state]\nreceipts_dir = \"{{DIR}}/receipts\"\n\
         [providers.inference]\nkind = \"openai-compatible\"\nbase_url = \"{}\"\n\
         api_key = {{ env = \"SERVE_E2E_UPSTREAM_KEY\" }}\n\
         [logging]\nusage_log = \"{}\"\n",
        up.url(),
        log.display()
    );
    let server = start(&text, "tags-open", &[]);
    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", server.base))
        .json(&json!({"model": "inference/test-model",
            "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let found = records(&log, 1).await;
    assert_eq!(found[0]["key"], Value::Null);
    assert_eq!(found[0]["tags"], Value::Null);
}

#[test]
fn bad_tags_are_refused_by_path() {
    use std::process::Command;
    for (tags, path) in [
        (r#"{ Env = "x" }"#, "keys[0].tags.Env"),
        (r#"{ "has-dash" = "x" }"#, "keys[0].tags.has-dash"),
        (r#"{ env = "" }"#, "keys[0].tags.env"),
        (r#"{ env = " lead" }"#, "keys[0].tags.env"),
        (r#"{ env = "café" }"#, "keys[0].tags.env"),
        (r#"{ env = "a\tb" }"#, "keys[0].tags.env"),
        (r#"{ env = "x" , n = 7 }"#, "tags"),
    ] {
        let dir = tempdir::Dir::new("tags-bad");
        let file = dir.0.join("gateway.toml");
        std::fs::write(
            &file,
            config("http://127.0.0.1:1", &format!("tags = {tags}"))
                .replace("{DIR}", dir.0.to_str().unwrap()),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
            .args(["serve", "--config", file.to_str().unwrap(), "--check"])
            .env_clear()
            .env("SERVE_E2E_UPSTREAM_KEY", "k")
            .env("SERVE_E2E_CALLER_KEY", CALLER_KEY)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stderr).to_string()
            + &String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(1), "{tags}: {text}");
        assert!(text.contains(path), "{tags}: {text}");
    }
    // Seventeen tags is one too many.
    let many: String = (0..17)
        .map(|n| format!("t{n} = \"v\""))
        .collect::<Vec<_>>()
        .join(", ");
    let dir = tempdir::Dir::new("tags-many");
    let file = dir.0.join("gateway.toml");
    std::fs::write(
        &file,
        config("http://127.0.0.1:1", &format!("tags = {{ {many} }}"))
            .replace("{DIR}", dir.0.to_str().unwrap()),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["serve", "--config", file.to_str().unwrap(), "--check"])
        .env_clear()
        .env("SERVE_E2E_UPSTREAM_KEY", "k")
        .env("SERVE_E2E_CALLER_KEY", CALLER_KEY)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("more than 16"));
}

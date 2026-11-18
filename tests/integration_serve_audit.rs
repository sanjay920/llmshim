//! The audit log on `serve` (note 0047): a real server writing a hash-chained record of its starts,
//! stops and reloads, and the real `llmshim audit verify` reading it back.
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{
    config, exit_status_within, hangup, reload_status, rewrite, run_to_exit, start, tempdir,
    terminate, wait_for_reload, Server, ADMIN_TOKEN, CALLER_KEY, UPSTREAM_KEY,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

fn digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn render(upstream: &str, audit: &Path, keys: &str) -> String {
    config(
        upstream,
        &format!(
            "[logging]\naudit_log = \"{}\"\n[admin]\nport = 0\ntoken = {{ env = \"SERVE_E2E_ADMIN_TOKEN\" }}\n{keys}",
            audit.display()
        ),
    )
}

fn records(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Wait until the log holds at least `count` records: the server prints its banner, and only then
/// writes its first record and installs its shutdown handler.
async fn wait_for_records(path: &Path, count: usize) -> Vec<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let found = records(path);
        if found.len() >= count || std::time::Instant::now() > deadline {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `llmshim audit verify <file>`: its exit status and what it printed.
fn verify(path: &Path) -> (Option<i32>, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["audit", "verify", path.to_str().unwrap()])
        .env_clear()
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr),
    )
}

/// A server is only safe to signal once it answers: its shutdown handler is installed after the
/// first audit record is written, and a SIGTERM before that takes the signal's default action.
async fn wait_until_serving(server: &Server) {
    let admin = server.admin.clone().expect("an [admin] listener");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let up = reqwest::Client::new()
            .get(format!("{admin}/health"))
            .send()
            .await
            .map(|response| response.status().is_success())
            .unwrap_or(false);
        if up || std::time::Instant::now() > deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stop(server: &mut Server) {
    wait_until_serving(server).await;
    terminate(server);
    let status = exit_status_within(server, Duration::from_secs(20)).await;
    assert!(status.is_some_and(|s| s.success()), "{status:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starts_reloads_refusals_and_stops_are_recorded_in_one_verifiable_chain_that_survives_a_restart(
) {
    let dir = tempdir::Dir::new("audit-chain");
    let log = dir.0.join("audit.jsonl");
    let extra_key = "second-caller-key-0123456789abcdef";
    let second = format!(
        "[[keys]]\nname = \"second\"\ntenant = \"other\"\nsha256 = \"{}\"\n",
        digest(extra_key)
    );
    let mut first = start(
        &render("http://127.0.0.1:9", &log, ""),
        "audit-first",
        &[("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)],
    );
    // The first record of the run: the version, the providers and the counts, and no secret.
    let started = wait_for_records(&log, 1).await;
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["event"], "server.started");
    assert_eq!(started[0]["seq"], 1);
    assert_eq!(started[0]["detail"]["providers"][0], "inference");
    assert_eq!(started[0]["prev"], "0".repeat(64));

    // A reload that adds a key: applied, with the key's NAME and what changed.
    rewrite(&first, &render("http://127.0.0.1:9", &log, &second));
    hangup(&first);
    wait_for_reload(&first, "generation 1", |s| s["generation"] == 1).await;
    // A reload the server refuses: the paths at fault, never their values.
    rewrite(
        &first,
        &render(
            "http://127.0.0.1:9",
            &log,
            "[[keys]]\nname = \"broken\"\ntenant = \"t\"\nsha256 = \"not-a-digest-SECRETISH\"\n",
        ),
    );
    hangup(&first);
    wait_for_reload(&first, "a rejection", |s| {
        s["last_reload"]["outcome"] == "rejected"
    })
    .await;

    let found = records(&log);
    let reloads: Vec<&Value> = found
        .iter()
        .filter(|r| r["event"] == "config.reloaded")
        .collect();
    assert_eq!(reloads.len(), 2, "{found:?}");
    assert_eq!(reloads[0]["outcome"], "applied");
    assert_eq!(reloads[0]["actor"], "operator");
    assert_eq!(reloads[0]["detail"]["keys_added"][0], "second");
    assert_eq!(
        reloads[0]["detail"]["keys_removed"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // A keys-only reload changes no provider, route or limit: `changes` lists those, and is empty.
    assert!(reloads[0]["detail"]["changes"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(reloads[0]["detail"]["credentials"], 2);
    assert_eq!(reloads[1]["outcome"], "rejected");
    assert!(reloads[1]["detail"]["paths"][0]
        .as_str()
        .unwrap()
        .starts_with("keys["));
    // Nothing secret anywhere in the file: not a caller key, a digest, an upstream key, the text of
    // the refused value.
    let text = std::fs::read_to_string(&log).unwrap();
    for secret in [
        CALLER_KEY,
        extra_key,
        UPSTREAM_KEY,
        &digest(extra_key),
        "SECRETISH",
        "not-a-digest",
    ] {
        assert!(!text.contains(secret), "{secret} is in the audit log");
    }
    // The admin status reports the head, and it is the file's last record.
    let status = reload_status(&first).await;
    let last = records(&log).last().cloned().unwrap();
    assert_eq!(status["audit"]["records"], last["seq"]);
    assert_eq!(status["audit"]["head"], last["hash"]);

    stop(&mut first).await;
    let found = records(&log);
    assert_eq!(found.last().unwrap()["event"], "server.stopping");
    let (code, said) = verify(&log);
    assert_eq!(code, Some(0), "{said}");
    assert!(
        said.contains(&format!("{} record(s)", found.len())),
        "{said}"
    );

    // A second run continues the same chain.
    let before = found.len();
    let mut again = start(
        &render("http://127.0.0.1:9", &log, ""),
        "audit-second",
        &[("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)],
    );
    let found = wait_for_records(&log, before + 1).await;
    assert_eq!(found.len(), before + 1);
    assert_eq!(found[before]["event"], "server.started");
    assert_eq!(found[before]["seq"], before as u64 + 1);
    assert_eq!(found[before]["prev"], found[before - 1]["hash"]);
    stop(&mut again).await;
    assert_eq!(verify(&log).0, Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tampering_is_named_by_the_verifier_and_a_damaged_log_stops_the_server_from_starting() {
    let dir = tempdir::Dir::new("audit-tamper");
    let log = dir.0.join("audit.jsonl");
    let mut server = start(
        &render("http://127.0.0.1:9", &log, ""),
        "audit-tamper",
        &[("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)],
    );
    wait_for_records(&log, 1).await;
    stop(&mut server).await;
    assert_eq!(records(&log).len(), 2);
    // An edited record is named at its line.
    let original = std::fs::read_to_string(&log).unwrap();
    std::fs::write(
        &log,
        original.replacen("\"outcome\":\"ok\"", "\"outcome\":\"hidden\"", 1),
    )
    .unwrap();
    let (code, said) = verify(&log);
    assert_eq!(code, Some(1), "{said}");
    assert!(said.contains("line 1"), "{said}");
    // The server will not start on a log whose last record was altered, and says why by path.
    std::fs::write(
        &log,
        original.replace(
            "\"event\":\"server.stopping\"",
            "\"event\":\"server.nothing\"",
        ),
    )
    .unwrap();
    let config_path = dir.0.join("gateway.toml");
    std::fs::write(
        &config_path,
        render("http://127.0.0.1:9", &log, "").replace("{DIR}", dir.0.to_str().unwrap()),
    )
    .unwrap();
    let (code, stderr) = run_to_exit(
        &config_path,
        &[
            ("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN),
            ("SERVE_E2E_CALLER_KEY", CALLER_KEY),
            ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("logging.audit_log"), "{stderr}");
}

#[test]
fn the_audit_path_must_be_an_absolute_file_that_no_other_log_uses() {
    let dir = tempdir::Dir::new("audit-config");
    let path = dir.0.join("gateway.toml");
    for (logging, fragment) in [
        ("audit_log = \"stdout\"", "absolute path"),
        ("audit_log = \"relative.jsonl\"", "absolute path"),
        (
            "audit_log = \"/var/log/same.jsonl\"\naccess_log = \"/var/log/same.jsonl\"",
            "must differ from logging.access_log",
        ),
    ] {
        std::fs::write(
            &path,
            config("http://127.0.0.1:9", &format!("[logging]\n{logging}")),
        )
        .unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
            .args([
                "serve",
                "--check",
                "--structure-only",
                "--config",
                path.to_str().unwrap(),
            ])
            .env_clear()
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stderr).to_string()
            + &String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(1), "{logging}: {text}");
        assert!(
            text.contains("logging.audit_log") && text.contains(fragment),
            "{logging}: {text}"
        );
    }
}

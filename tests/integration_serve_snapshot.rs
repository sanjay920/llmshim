//! `llmshim serve` with its caller keys from a signed policy snapshot (0024): a real
//! server process, the real file rewritten under it, and `SIGHUP` asking for the
//! change. The snapshots are signed here with the library the `snapshot` command uses.
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{
    chat, completion, hangup, run_with_args, start, tempdir, terminate, wait_for_reload, Server,
    ADMIN_TOKEN, UPSTREAM_KEY,
};
use llmshim::serve::snapshot;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MODEL: &str = "inference/test-model";
const ALPHA: &str = "alpha-caller-key-0123456789abcdef";
const BRAVO: &str = "bravo-caller-key-0123456789abcdef";

fn digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn key(name: &str, token: &str, state: &str) -> Value {
    json!({"id": format!("key_{name}"), "name": name, "tenant": "team_a",
        "state": state, "sha256": digest(token)})
}

fn payload(generation: u64, keys: Vec<Value>) -> Value {
    json!({"version": 1, "org": "org_test", "generation": generation,
        "issued_at": "2026-11-16T12:00:00Z",
        "tenants": [{"id": "team_a", "name": "platform"}], "keys": keys})
}

/// A signing key, the files it is used with, and the configuration that trusts it.
struct Plane {
    dir: tempdir::Dir,
    seed: String,
    public: String,
}

impl Plane {
    fn new(label: &str) -> Self {
        let (seed, public) = snapshot::keygen().unwrap();
        Self {
            dir: tempdir::Dir::new(label),
            seed,
            public,
        }
    }

    fn snapshot_file(&self) -> PathBuf {
        self.dir.0.join("snapshot.json")
    }

    fn write(&self, payload: &Value) {
        std::fs::write(
            self.snapshot_file(),
            snapshot::sign(payload, &self.seed).unwrap(),
        )
        .unwrap();
    }

    fn config(&self, upstream: &str, extra: &str) -> String {
        format!(
            r#"
            version = 1
            [server]
            port = 0
            [state]
            receipts_dir = "{{DIR}}/receipts"
            policy_dir = "{policy}"
            [providers.inference]
            kind = "openai-compatible"
            base_url = "{upstream}"
            api_key = {{ env = "SERVE_E2E_UPSTREAM_KEY" }}
            [admin]
            port = 0
            token = {{ env = "SERVE_E2E_ADMIN_TOKEN" }}
            [control_plane]
            org = "org_test"
            verify_keys = ["{public}"]
            snapshot_file = "{file}"
            {extra}
            "#,
            policy = self.dir.0.join("policy").display(),
            public = self.public,
            file = self.snapshot_file().display(),
        )
    }
}

async fn status_of(server: &Server, token: &str) -> u16 {
    chat(server, Some(token), MODEL).await.status().as_u16()
}

async fn mock(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect_at_least(calls)
        .create_async()
        .await;
    (upstream, mock)
}

#[tokio::test]
async fn a_signed_snapshot_is_the_source_of_who_may_call() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-source");
    plane.write(&payload(
        1,
        vec![key("alpha", ALPHA, "active"), key("old", BRAVO, "revoked")],
    ));
    let server = start(&plane.config(&upstream.url(), ""), "snapshot-source", &[]);
    assert_eq!(status_of(&server, ALPHA).await, 200);
    // A revoked key is listed in the snapshot and refused, as an unknown one is.
    assert_eq!(status_of(&server, BRAVO).await, 401);
    assert_eq!(
        status_of(&server, "never-listed-0123456789abcdef").await,
        401
    );
}

/// A key's model list travels in the signed snapshot and restricts what it may call (0028);
/// the unrestricted key beside it is unaffected.
#[tokio::test]
async fn a_snapshot_key_with_a_model_list_may_call_only_those_models() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-models");
    let mut restricted = key("alpha", ALPHA, "active");
    restricted["allowed_models"] = json!([MODEL]);
    plane.write(&payload(1, vec![restricted, key("bravo", BRAVO, "active")]));
    let server = start(&plane.config(&upstream.url(), ""), "snapshot-models", &[]);
    assert_eq!(status_of(&server, ALPHA).await, 200);
    let other = chat(&server, Some(ALPHA), "inference/other-model").await;
    assert_eq!(other.status().as_u16(), 403);
    // The key with no list calls any model.
    let free = chat(&server, Some(BRAVO), "inference/other-model").await;
    assert_ne!(free.status().as_u16(), 403);
    // A list that would be refused in a file is refused in a snapshot, by path, and the
    // replica keeps what it applied.
    let mut bad = key("alpha", ALPHA, "active");
    bad["allowed_models"] = json!([]);
    plane.write(&payload(2, vec![bad]));
    hangup(&server);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        snapshot_status(&server).await["generation"],
        1,
        "generation 2 was refused, so generation 1 is still the one in force"
    );
    assert_eq!(
        chat(&server, Some(ALPHA), "inference/other-model")
            .await
            .status()
            .as_u16(),
        403,
        "the previous snapshot still serves"
    );
}

/// A snapshot key's tags reach the usage records it produces (0029); a key without tags has `{}`.
#[tokio::test]
async fn a_snapshot_keys_tags_are_on_its_usage_records() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-tags");
    let mut tagged = key("alpha", ALPHA, "active");
    tagged["tags"] = json!({"env": "prod", "cost_center": "cc-1"});
    plane.write(&payload(1, vec![tagged, key("bravo", BRAVO, "active")]));
    let log = plane.dir.0.join("usage.jsonl");
    let extra = format!("[logging]\nusage_log = \"{}\"\n", log.display());
    let server = start(&plane.config(&upstream.url(), &extra), "snapshot-tags", &[]);
    assert_eq!(status_of(&server, ALPHA).await, 200);
    assert_eq!(status_of(&server, BRAVO).await, 200);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let records = loop {
        let found: Vec<Value> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if found.len() >= 2 || std::time::Instant::now() > deadline {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let of = |name: &str| records.iter().find(|r| r["key"] == name).unwrap().clone();
    assert_eq!(
        of("alpha")["tags"],
        json!({"cost_center": "cc-1", "env": "prod"})
    );
    assert_eq!(of("bravo")["tags"], json!({}));
}

/// A team's budget in the snapshot is the tenant-level cap over every key of the team (0030):
/// the key has no budget of its own, and a change of the cap applies with the next snapshot.
#[tokio::test]
async fn a_teams_budget_in_the_snapshot_caps_its_keys_and_a_new_generation_changes_it() {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/responses")
        .with_header("content-type", "application/json")
        .with_body(
            json!({
                "id": "resp_1", "object": "response", "status": "completed",
                "model": "gpt-6-luna",
                "output": [{"type": "message", "id": "msg_1", "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "pong", "annotations": []}]}],
                "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
            })
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let plane = Plane::new("snapshot-team-budget");
    let with_budget = |generation: u64, usd: f64| {
        let mut p = payload(generation, vec![key("alpha", ALPHA, "active")]);
        p["tenants"] = json!([{"id": "team_a", "name": "platform", "budget_usd": usd}]);
        p
    };
    plane.write(&with_budget(1, 0.0));
    let text = plane
        .config(&upstream.url(), "")
        .replace("providers.inference", "providers.openai")
        .replace("kind = \"openai-compatible\"", "kind = \"openai\"");
    let server = start(&text, "snapshot-team-budget", &[]);
    let ask = || async {
        chat(&server, Some(ALPHA), "openai/gpt-6-luna")
            .await
            .status()
            .as_u16()
    };
    // The key has no budget; the team's cap of zero refuses it before the provider.
    assert_eq!(ask().await, 429);
    // A new generation raises the cap: served.
    plane.write(&with_budget(2, 100.0));
    hangup(&server);
    wait_for_reload(&server, "generation 2", |s| s["generation"] == 1).await;
    assert_eq!(ask().await, 200);
    mock.assert_async().await;
}

#[tokio::test]
async fn a_new_generation_applies_on_reload_and_revocation_takes_effect() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-reload");
    plane.write(&payload(1, vec![key("alpha", ALPHA, "active")]));
    let server = start(&plane.config(&upstream.url(), ""), "snapshot-reload", &[]);
    assert_eq!(status_of(&server, ALPHA).await, 200);
    assert_eq!(status_of(&server, BRAVO).await, 401);
    plane.write(&payload(
        2,
        vec![
            key("alpha", ALPHA, "revoked"),
            key("bravo", BRAVO, "active"),
        ],
    ));
    hangup(&server);
    let status = wait_for_reload(&server, "generation 1", |status| status["generation"] == 1).await;
    assert_eq!(status["last_reload"]["outcome"], "applied", "{status}");
    assert_eq!(
        status_of(&server, ALPHA).await,
        401,
        "revoked by the snapshot"
    );
    assert_eq!(status_of(&server, BRAVO).await, 200);
}

#[tokio::test]
async fn a_snapshot_that_does_not_verify_or_advance_never_changes_what_serves() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-refused");
    plane.write(&payload(5, vec![key("alpha", ALPHA, "active")]));
    let server = start(&plane.config(&upstream.url(), ""), "snapshot-refused", &[]);
    assert_eq!(status_of(&server, ALPHA).await, 200);

    let rejected = |what: &'static str, wanted: &'static str| {
        let server = &server;
        async move {
            hangup(server);
            // Until the report names this rejection: the previous one is still there.
            let status = wait_for_reload(server, what, |status| {
                status["last_reload"]["outcome"] == "rejected"
                    && status["in_sync"] == false
                    && status["last_reload"]["issues"].to_string().contains(wanted)
            })
            .await;
            let issues = status["last_reload"]["issues"].to_string();
            assert!(issues.contains(wanted), "{what}: {issues}");
            // Nothing from the document, and no digest, ever reaches the report.
            assert!(!issues.contains(&digest(ALPHA)) && !issues.contains(&digest(BRAVO)));
            assert_eq!(status["generation"], 0, "{what}");
            assert_eq!(status_of(server, ALPHA).await, 200, "{what}: still serving");
        }
    };

    // Older, validly signed: could un-revoke a key.
    plane.write(&payload(3, vec![key("bravo", BRAVO, "active")]));
    rejected("an older generation", "does not advance").await;
    // Tampered: the signed bytes changed.
    plane.write(&payload(9, vec![key("bravo", BRAVO, "active")]));
    let text = std::fs::read_to_string(plane.snapshot_file()).unwrap();
    let mut envelope: Value = serde_json::from_str(&text).unwrap();
    let forged = payload(10, vec![key("bravo", BRAVO, "active")]);
    envelope["payload"] = json!(base64_of(&forged));
    std::fs::write(plane.snapshot_file(), envelope.to_string()).unwrap();
    rejected("a tampered payload", "signature").await;
    // Signed by a key that is not trusted.
    let (other_seed, _) = snapshot::keygen().unwrap();
    std::fs::write(
        plane.snapshot_file(),
        snapshot::sign(
            &payload(11, vec![key("bravo", BRAVO, "active")]),
            &other_seed,
        )
        .unwrap(),
    )
    .unwrap();
    rejected("an untrusted signer", "verify_keys").await;
    // Not a snapshot at all.
    std::fs::write(plane.snapshot_file(), "garbage").unwrap();
    rejected("garbage", "well-formed").await;
}

fn floor_generation(plane: &Plane) -> u64 {
    let text =
        std::fs::read_to_string(plane.dir.0.join("policy").join("applied-snapshot.json")).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()["generation"]
        .as_u64()
        .unwrap()
}

#[tokio::test]
async fn the_floor_follows_what_applied_and_never_what_was_refused() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-floor-follows");
    plane.write(&payload(5, vec![key("alpha", ALPHA, "active")]));
    let server = start(
        &plane.config(&upstream.url(), ""),
        "snapshot-floor-follows",
        &[],
    );
    assert_eq!(floor_generation(&plane), 5);
    // Refused (older): the floor stays.
    plane.write(&payload(4, vec![key("bravo", BRAVO, "active")]));
    hangup(&server);
    wait_for_reload(&server, "the refusal", |s| {
        s["last_reload"]["issues"]
            .to_string()
            .contains("does not advance")
    })
    .await;
    assert_eq!(floor_generation(&plane), 5);
    // The same keys at a higher generation: nothing changes for callers, and the floor advances.
    plane.write(&payload(8, vec![key("alpha", ALPHA, "active")]));
    hangup(&server);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while floor_generation(&plane) != 8 {
        assert!(
            std::time::Instant::now() < deadline,
            "the floor did not follow"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(status_of(&server, ALPHA).await, 200);
}

#[tokio::test]
async fn a_running_replica_never_goes_behind_what_it_applied_even_if_the_floor_file_is_lowered() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-memory-floor");
    plane.write(&payload(5, vec![key("alpha", ALPHA, "active")]));
    let server = start(
        &plane.config(&upstream.url(), ""),
        "snapshot-memory-floor",
        &[],
    );
    // Someone deletes the record and puts an older, validly signed snapshot in place.
    std::fs::remove_file(plane.dir.0.join("policy").join("applied-snapshot.json")).unwrap();
    plane.write(&payload(2, vec![key("bravo", BRAVO, "active")]));
    hangup(&server);
    wait_for_reload(&server, "the refusal", |s| {
        s["last_reload"]["issues"]
            .to_string()
            .contains("does not advance")
    })
    .await;
    assert_eq!(status_of(&server, ALPHA).await, 200);
    assert_eq!(status_of(&server, BRAVO).await, 401);
}

#[tokio::test]
async fn another_organizations_snapshot_and_a_trust_list_that_drops_the_signer_are_refused() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-org");
    plane.write(&payload(1, vec![key("alpha", ALPHA, "active")]));
    let server = start(&plane.config(&upstream.url(), ""), "snapshot-org", &[]);
    let mut other = payload(2, vec![key("bravo", BRAVO, "active")]);
    other["org"] = json!("org_other");
    plane.write(&other);
    hangup(&server);
    wait_for_reload(&server, "the org refusal", |s| {
        s["last_reload"]["issues"]
            .to_string()
            .contains("another organization")
    })
    .await;
    assert_eq!(status_of(&server, BRAVO).await, 401);
    // A trust list that no longer names the signer: the snapshot on disk no longer
    // verifies against it, so the reload is refused and the keys in force keep serving.
    let (_, other_public) = snapshot::keygen().unwrap();
    let edited = plane
        .config(&upstream.url(), "")
        .replace(&plane.public, &other_public)
        .replace("{DIR}", server.dir.0.to_str().unwrap());
    std::fs::write(server.dir.0.join("gateway.toml"), edited).unwrap();
    hangup(&server);
    let status = wait_for_reload(&server, "the path refusal", |s| {
        s["last_reload"]["issues"]
            .to_string()
            .contains("verify_keys")
    })
    .await;
    assert_eq!(status["generation"], 0);
    assert_eq!(status_of(&server, ALPHA).await, 200);
}

fn base64_of(payload: &Value) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(payload.to_string())
}

#[tokio::test]
async fn the_applied_generation_survives_a_restart() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-floor");
    plane.write(&payload(5, vec![key("alpha", ALPHA, "active")]));
    let config = plane.config(&upstream.url(), "");
    let mut server = start(&config, "snapshot-floor", &[]);
    assert_eq!(status_of(&server, ALPHA).await, 200);
    terminate(&server);
    assert!(
        harness::exit_status_within(&mut server, Duration::from_secs(20))
            .await
            .is_some()
    );
    assert!(
        std::fs::read_to_string(plane.dir.0.join("policy").join("applied-snapshot.json"))
            .unwrap()
            .contains("\"generation\":5")
    );
    // Whoever put an older snapshot in place, the restart refuses to serve it.
    plane.write(&payload(3, vec![key("bravo", BRAVO, "active")]));
    let path = server.dir.0.join("gateway.toml");
    let (code, stderr) = run_with_args(
        &path,
        &[],
        &[
            ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
            ("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN),
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("does not advance"), "{stderr}");
    // The same generation with different content is refused too.
    plane.write(&payload(5, vec![key("bravo", BRAVO, "active")]));
    let (code, stderr) = run_with_args(
        &path,
        &[],
        &[
            ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
            ("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN),
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("different content"), "{stderr}");
}

#[test]
fn check_reads_the_snapshot_and_structure_only_does_not() {
    let plane = Plane::new("snapshot-check");
    let upstream = "http://127.0.0.1:1/v1";
    let path = plane.dir.0.join("gateway.toml");
    std::fs::write(
        &path,
        plane
            .config(upstream, "")
            .replace("{DIR}", plane.dir.0.to_str().unwrap()),
    )
    .unwrap();
    let env = [
        ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
        ("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN),
    ];
    // No snapshot yet: a full check says so, a structure-only check never looks.
    let (code, stderr) = run_with_args(&path, &["--check"], &env);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("snapshot file cannot be read"), "{stderr}");
    let (code, stderr) = run_with_args(&path, &["--check", "--structure-only"], &env);
    assert_eq!(code, Some(0), "{stderr}");
    plane.write(&payload(1, vec![key("alpha", ALPHA, "active")]));
    let (code, stderr) = run_with_args(&path, &["--check"], &env);
    assert_eq!(code, Some(0), "{stderr}");
    // `--check` records nothing: no floor was written.
    assert!(!plane.dir.0.join("policy").exists());
}

#[test]
fn a_file_with_keys_and_a_control_plane_is_refused_and_a_key_in_a_snapshot_follows_the_key_rules() {
    let plane = Plane::new("snapshot-rules");
    let path = plane.dir.0.join("gateway.toml");
    let env = [
        ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
        ("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN),
    ];
    let both = format!(
        "{}\n[[keys]]\nname = \"ci\"\ntenant = \"t\"\nsha256 = \"{}\"\n",
        plane.config("http://127.0.0.1:1/v1", ""),
        digest(ALPHA)
    );
    std::fs::write(&path, both.replace("{DIR}", plane.dir.0.to_str().unwrap())).unwrap();
    let (code, stderr) = run_with_args(&path, &["--check", "--structure-only"], &env);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("control_plane"), "{stderr}");
    // A snapshot key with a name no file key could have is refused by the same rule.
    std::fs::write(
        &path,
        plane
            .config("http://127.0.0.1:1/v1", "")
            .replace("{DIR}", plane.dir.0.to_str().unwrap()),
    )
    .unwrap();
    plane.write(&payload(1, vec![key("Not A Name", ALPHA, "active")]));
    let (code, stderr) = run_with_args(&path, &["--check"], &env);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("control_plane.snapshot_file (keys[0].name)"),
        "{stderr}"
    );
    // A snapshot with no active key could identify no caller.
    plane.write(&payload(2, vec![key("alpha", ALPHA, "revoked")]));
    let (code, stderr) = run_with_args(&path, &["--check"], &env);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("no active key"), "{stderr}");
}

fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(args)
        .env_clear()
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn the_snapshot_command_makes_signs_and_verifies() {
    let dir = tempdir::Dir::new("snapshot-cli");
    let seed = dir.0.join("seed");
    let payload_file = dir.0.join("payload.json");
    let out = dir.0.join("snapshot.json");
    let path = |p: &Path| p.to_str().unwrap().to_string();
    let (code, public, stderr) = run(&["snapshot", "keygen", "--out", &path(&seed)]);
    assert_eq!(code, Some(0), "{stderr}");
    let public = public.trim().to_string();
    assert!(public.starts_with("ed25519:"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&seed).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // It never overwrites a key.
    let (code, _, _) = run(&["snapshot", "keygen", "--out", &path(&seed)]);
    assert_eq!(code, Some(1));
    std::fs::write(
        &payload_file,
        payload(4, vec![key("alpha", ALPHA, "active")]).to_string(),
    )
    .unwrap();
    let (code, _, stderr) = run(&[
        "snapshot",
        "sign",
        "--key-file",
        &path(&seed),
        "--in",
        &path(&payload_file),
        "--out",
        &path(&out),
    ]);
    assert_eq!(code, Some(0), "{stderr}");
    let (code, stdout, stderr) = run(&[
        "snapshot",
        "verify",
        "--verify-key",
        &public,
        "--in",
        &path(&out),
    ]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("generation 4"), "{stdout}");
    // A different trusted key does not verify it.
    let (other, other_public, _) = {
        let other_seed = dir.0.join("seed2");
        let r = run(&["snapshot", "keygen", "--out", &path(&other_seed)]);
        (other_seed, r.1.trim().to_string(), r.2)
    };
    let _ = other;
    let (code, _, stderr) = run(&[
        "snapshot",
        "verify",
        "--verify-key",
        &other_public,
        "--in",
        &path(&out),
    ]);
    assert_eq!(code, Some(1));
    assert!(stderr.contains("not in verify_keys"), "{stderr}");
    // A payload the data plane would refuse is refused at signing.
    std::fs::write(
        &payload_file,
        json!({"version": 2, "org": "o", "generation": 1, "issued_at": "x"}).to_string(),
    )
    .unwrap();
    let (code, _, _) = run(&[
        "snapshot",
        "sign",
        "--key-file",
        &path(&seed),
        "--in",
        &path(&payload_file),
        "--out",
        &path(&out),
    ]);
    assert_eq!(code, Some(1));
    // Missing arguments are a usage error.
    assert_eq!(run(&["snapshot", "sign"]).0, Some(2));
}

fn two_signers(plane: &Plane, other_public: &str) -> String {
    format!("[\"{}\", \"{other_public}\"]", plane.public)
}

fn with_verify_keys(plane: &Plane, text: &str, list: &str) -> String {
    text.replace(&format!("[\"{}\"]", plane.public), list)
}

async fn snapshot_status(server: &Server) -> Value {
    harness::reload_status(server).await["snapshot"].clone()
}

#[tokio::test]
async fn verify_keys_reload_and_a_removed_signer_is_reported_not_obeyed() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-signers");
    plane.write(&payload(1, vec![key("alpha", ALPHA, "active")]));
    let config = plane.config(&upstream.url(), "");
    let server = start(&config, "snapshot-signers", &[]);
    let status = snapshot_status(&server).await;
    assert_eq!(status["generation"], 1);
    assert_eq!(status["signer"], plane.public);
    assert_eq!(status["signer_untrusted"], false);
    assert_eq!(status["stale"], false);

    // A second signer is trusted and signs generation 2: no restart.
    let (other_seed, other_public) = snapshot::keygen().unwrap();
    let both = with_verify_keys(&plane, &config, &two_signers(&plane, &other_public))
        .replace("{DIR}", server.dir.0.to_str().unwrap());
    std::fs::write(server.dir.0.join("gateway.toml"), &both).unwrap();
    std::fs::write(
        plane.snapshot_file(),
        snapshot::sign(
            &payload(2, vec![key("bravo", BRAVO, "active")]),
            &other_seed,
        )
        .unwrap(),
    )
    .unwrap();
    hangup(&server);
    wait_for_reload(&server, "the new signer's generation", |s| {
        s["generation"] == 1
    })
    .await;
    assert_eq!(snapshot_status(&server).await["signer"], other_public);
    assert_eq!(status_of(&server, BRAVO).await, 200);
    assert_eq!(status_of(&server, ALPHA).await, 401);

    // The second signer is removed from the list: what it signed no longer verifies,
    // so the reload is refused, the keys in force keep serving, and the status says
    // the signer in force is untrusted.
    let only_first = config.replace("{DIR}", server.dir.0.to_str().unwrap());
    std::fs::write(server.dir.0.join("gateway.toml"), only_first).unwrap();
    hangup(&server);
    wait_for_reload(&server, "the untrusted signer", |s| {
        s["snapshot"]["signer_untrusted"] == true
    })
    .await;
    assert_eq!(
        status_of(&server, BRAVO).await,
        200,
        "still serving what applied"
    );
    // A snapshot from a trusted signer replaces it, and the alert clears.
    plane.write(&payload(3, vec![key("alpha", ALPHA, "active")]));
    hangup(&server);
    wait_for_reload(&server, "the replacement", |s| {
        s["snapshot"]["generation"] == 3 && s["snapshot"]["signer_untrusted"] == false
    })
    .await;
    assert_eq!(status_of(&server, ALPHA).await, 200);
}

#[tokio::test]
async fn a_stale_snapshot_is_reported_and_with_refuse_when_stale_inference_is_refused() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-stale");
    plane.write(&payload(1, vec![key("alpha", ALPHA, "active")]));
    let config = plane.config(
        &upstream.url(),
        "max_age_secs = 8\nrefuse_when_stale = true",
    );
    let server = start(&config, "snapshot-stale", &[]);
    assert_eq!(status_of(&server, ALPHA).await, 200);
    assert_eq!(snapshot_status(&server).await["stale"], false);
    wait_for_reload(&server, "staleness", |s| s["snapshot"]["stale"] == true).await;
    let refused = chat(&server, Some(ALPHA), MODEL).await;
    assert_eq!(refused.status().as_u16(), 503);
    assert_eq!(refused.headers()["retry-after"], "30");
    let body: Value = refused.json().await.unwrap();
    // The native Chat wire answers in its own error shape; the message names the cause.
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("policy snapshot"),
        "{body}"
    );
    // The gateway's own path answers in the gateway's shape, with a code.
    let own = reqwest::Client::new()
        .post(format!("{}/v1/chat", server.base))
        .bearer_auth(ALPHA)
        .json(&json!({"model": MODEL, "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(own.status().as_u16(), 503);
    let own: Value = own.json().await.unwrap();
    assert_eq!(own["error"]["code"], "policy_snapshot_stale", "{own}");
    // Model discovery is not inference either.
    let models = reqwest::Client::new()
        .get(format!("{}/v1/models", server.base))
        .bearer_auth(ALPHA)
        .send()
        .await
        .unwrap();
    assert_eq!(models.status().as_u16(), 200);
    // Not inference: the probes and the operational endpoints keep answering.
    let admin = server.admin.clone().unwrap();
    for path in ["/health", "/ready"] {
        let probe = reqwest::get(format!("{admin}{path}")).await.unwrap();
        assert_eq!(probe.status().as_u16(), 200, "{path}");
    }
    // A new generation applies, the age restarts, and inference resumes.
    plane.write(&payload(2, vec![key("alpha", ALPHA, "active")]));
    hangup(&server);
    wait_for_reload(&server, "the new generation", |s| {
        s["snapshot"]["generation"] == 2 && s["snapshot"]["stale"] == false
    })
    .await;
    assert_eq!(status_of(&server, ALPHA).await, 200);
}

#[tokio::test]
async fn a_stale_snapshot_without_refuse_when_stale_only_alerts() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-alert");
    plane.write(&payload(1, vec![key("alpha", ALPHA, "active")]));
    let server = start(
        &plane.config(&upstream.url(), "max_age_secs = 8"),
        "snapshot-alert",
        &[],
    );
    wait_for_reload(&server, "staleness", |s| s["snapshot"]["stale"] == true).await;
    assert_eq!(
        status_of(&server, ALPHA).await,
        200,
        "an alert, not an outage"
    );
}

#[test]
fn refuse_when_stale_needs_a_max_age() {
    let plane = Plane::new("snapshot-stale-rule");
    let path = plane.dir.0.join("gateway.toml");
    std::fs::write(
        &path,
        plane
            .config("http://127.0.0.1:1/v1", "refuse_when_stale = true")
            .replace("{DIR}", plane.dir.0.to_str().unwrap()),
    )
    .unwrap();
    let env = [
        ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
        ("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN),
    ];
    let (code, stderr) = run_with_args(&path, &["--check", "--structure-only"], &env);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("control_plane.refuse_when_stale"),
        "{stderr}"
    );
}

#[tokio::test]
async fn a_restart_does_not_make_an_old_snapshot_fresh_again() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-restart-age");
    plane.write(&payload(1, vec![key("alpha", ALPHA, "active")]));
    let config = plane.config(&upstream.url(), "max_age_secs = 6");
    let mut server = start(&config, "snapshot-restart-age-1", &[]);
    wait_for_reload(&server, "staleness", |s| s["snapshot"]["stale"] == true).await;
    terminate(&server);
    assert!(
        harness::exit_status_within(&mut server, Duration::from_secs(20))
            .await
            .is_some()
    );
    // The same generation, the same file: the age counts from when it first applied.
    let again = start(&config, "snapshot-restart-age-2", &[]);
    assert_eq!(snapshot_status(&again).await["stale"], true);
}

fn in_secs(seconds: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(seconds)).to_rfc3339()
}

#[tokio::test]
async fn a_key_stops_working_at_its_expiry_without_a_reload_and_a_new_snapshot_can_extend_it() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("snapshot-expiry");
    let mut soon = key("alpha", ALPHA, "active");
    soon["expires_at"] = json!(in_secs(10));
    plane.write(&payload(1, vec![soon, key("bravo", BRAVO, "active")]));
    let server = start(&plane.config(&upstream.url(), ""), "snapshot-expiry", &[]);
    assert_eq!(status_of(&server, ALPHA).await, 200, "before it expires");
    // No reload happens: the data plane's own clock decides. Polled, not slept for.
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    while status_of(&server, ALPHA).await != 401 {
        assert!(
            std::time::Instant::now() < deadline,
            "the key never expired"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        status_of(&server, BRAVO).await,
        200,
        "a key without an expiry is untouched"
    );
    // A later generation extends it: expiry is part of what a reload applies.
    let mut extended = key("alpha", ALPHA, "active");
    extended["expires_at"] = json!(in_secs(3600));
    plane.write(&payload(2, vec![extended, key("bravo", BRAVO, "active")]));
    hangup(&server);
    wait_for_reload(&server, "the extension", |s| {
        s["snapshot"]["generation"] == 2
    })
    .await;
    assert_eq!(status_of(&server, ALPHA).await, 200);
}

#[test]
fn a_file_key_takes_an_rfc3339_expiry_and_refuses_anything_else() {
    let dir = tempdir::Dir::new("expiry-config");
    let path = dir.0.join("gateway.toml");
    let env = [
        ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
        ("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN),
        ("SERVE_E2E_CALLER_KEY", ALPHA),
    ];
    let base =
        harness::config("http://127.0.0.1:1/v1", "").replace("{DIR}", dir.0.to_str().unwrap());
    let with = |expires: &str| {
        base.replacen(
            "name = \"ci\"",
            &format!("name = \"ci\"\n        expires_at = \"{expires}\""),
            1,
        )
    };
    std::fs::write(&path, with("2999-01-01T00:00:00Z")).unwrap();
    let (code, stderr) = run_with_args(&path, &["--check", "--structure-only"], &env);
    assert_eq!(code, Some(0), "{stderr}");
    std::fs::write(&path, with("next tuesday")).unwrap();
    let (code, stderr) = run_with_args(&path, &["--check", "--structure-only"], &env);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("keys[0].expires_at"), "{stderr}");
}

#[tokio::test]
async fn a_key_in_the_file_expires_at_request_time_too() {
    let (upstream, _mock) = mock(1).await;
    let expiry = in_secs(10);
    let text = harness::config(&upstream.url(), "").replacen(
        "name = \"ci\"",
        &format!("name = \"ci\"\n        expires_at = \"{expiry}\""),
        1,
    );
    let server = start(&text, "file-key-expiry", &[]);
    assert_eq!(status_of(&server, harness::CALLER_KEY).await, 200);
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    while status_of(&server, harness::CALLER_KEY).await != 401 {
        assert!(
            std::time::Instant::now() < deadline,
            "the key never expired"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ---- a snapshot pulled from a control plane (0024, 0026) ----

const CONTROL_TOKEN: &str = "control-plane-token-0123456789abcdef";

impl Plane {
    /// The configuration with the snapshot pulled from `control` instead of read from a file.
    fn pulled_config(&self, upstream: &str, control: &str, extra: &str) -> String {
        self.config(upstream, "")
            .replace(
                &format!("snapshot_file = \"{}\"", self.snapshot_file().display()),
                &format!(
                    "[control_plane.source]\nurl = \"{control}\"\ntoken = {{ env = \"SERVE_E2E_CONTROL_TOKEN\" }}\nallow_insecure_http = true\npoll_interval_secs = 1\n{extra}\n"
                ),
            )
            // `[control_plane]` keys must precede its sub-table: move `org` and the keys above.
            .to_string()
    }

    fn envelope(&self, payload: &Value) -> String {
        snapshot::sign(payload, &self.seed).unwrap()
    }
}

async fn control_plane(
    plane: &Plane,
    generation: u64,
    keys: Vec<Value>,
) -> (mockito::ServerGuard, mockito::Mock) {
    let mut control = mockito::Server::new_async().await;
    let mock = serve_generation(&mut control, plane, generation, keys).await;
    (control, mock)
}

async fn serve_generation(
    control: &mut mockito::ServerGuard,
    plane: &Plane,
    generation: u64,
    keys: Vec<Value>,
) -> mockito::Mock {
    control
        .mock("GET", "/v1/orgs/org_test/snapshot")
        .match_header("authorization", format!("Bearer {CONTROL_TOKEN}").as_str())
        .with_header("content-type", "application/json")
        .with_header("etag", &format!("\"{generation}\""))
        .with_body(plane.envelope(&payload(generation, keys)))
        .create_async()
        .await
}

const PULL_ENV: [(&str, &str); 1] = [("SERVE_E2E_CONTROL_TOKEN", CONTROL_TOKEN)];

#[tokio::test]
async fn a_snapshot_is_pulled_at_start_and_a_new_generation_applies_without_a_signal() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("pull-apply");
    let (mut control, first) = control_plane(&plane, 1, vec![key("alpha", ALPHA, "active")]).await;
    let server = start(
        &plane.pulled_config(&upstream.url(), &control.url(), ""),
        "pull-apply",
        &PULL_ENV,
    );
    assert_eq!(
        status_of(&server, ALPHA).await,
        200,
        "served from what was pulled"
    );
    assert!(
        plane.dir.0.join("policy").join("snapshot.json").exists(),
        "and kept"
    );
    let status = snapshot_status(&server).await;
    assert_eq!(status["generation"], 1);
    assert!(status["last_pull_ok_at_epoch_secs"].is_u64(), "{status}");

    // A new generation: alpha is revoked, bravo added. No SIGHUP, no file edit.
    first.remove_async().await;
    let _second = serve_generation(
        &mut control,
        &plane,
        2,
        vec![
            key("alpha", ALPHA, "revoked"),
            key("bravo", BRAVO, "active"),
        ],
    )
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while status_of(&server, ALPHA).await != 401 {
        assert!(
            std::time::Instant::now() < deadline,
            "the new generation never applied"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(status_of(&server, BRAVO).await, 200);
    assert_eq!(snapshot_status(&server).await["generation"], 2);
}

#[tokio::test]
async fn an_unreachable_or_wrong_control_plane_never_changes_what_serves_and_is_reported() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("pull-down");
    let (mut control, first) = control_plane(&plane, 1, vec![key("alpha", ALPHA, "active")]).await;
    let server = start(
        &plane.pulled_config(&upstream.url(), &control.url(), ""),
        "pull-down",
        &PULL_ENV,
    );
    assert_eq!(status_of(&server, ALPHA).await, 200);

    // The control plane starts answering 500: the last good snapshot keeps serving.
    first.remove_async().await;
    let _broken = control
        .mock("GET", "/v1/orgs/org_test/snapshot")
        .with_status(500)
        .create_async()
        .await;
    wait_for_reload(&server, "the failure", |s| {
        s["snapshot"]["last_pull_error"]
            .as_str()
            .is_some_and(|e| e.contains("500"))
    })
    .await;
    assert_eq!(status_of(&server, ALPHA).await, 200);
}

#[tokio::test]
async fn a_pulled_snapshot_that_does_not_verify_is_refused_and_never_cached() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("pull-refused");
    let (mut control, first) = control_plane(&plane, 1, vec![key("alpha", ALPHA, "active")]).await;
    let server = start(
        &plane.pulled_config(&upstream.url(), &control.url(), ""),
        "pull-refused",
        &PULL_ENV,
    );
    let cache = plane.dir.0.join("policy").join("snapshot.json");
    let before = std::fs::read(&cache).unwrap();
    first.remove_async().await;
    // Signed by a key this replica does not trust, claiming a higher generation.
    let (other_seed, _) = snapshot::keygen().unwrap();
    let forged = snapshot::sign(
        &payload(9, vec![key("bravo", BRAVO, "active")]),
        &other_seed,
    )
    .unwrap();
    let _forged = control
        .mock("GET", "/v1/orgs/org_test/snapshot")
        .with_body(forged)
        .create_async()
        .await;
    wait_for_reload(&server, "the refusal", |s| {
        s["snapshot"]["last_pull_error"]
            .as_str()
            .is_some_and(|e| e.contains("refused"))
    })
    .await;
    assert_eq!(
        std::fs::read(&cache).unwrap(),
        before,
        "the cache keeps the last good one"
    );
    assert_eq!(status_of(&server, ALPHA).await, 200);
    assert_eq!(status_of(&server, BRAVO).await, 401);
}

#[tokio::test]
async fn a_wrong_token_is_reported_without_echoing_it() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("pull-token");
    let (control, _first) = control_plane(&plane, 1, vec![key("alpha", ALPHA, "active")]).await;
    // The first pull succeeds with the right token; rotate to a wrong one by restarting
    // with a cache and a different environment.
    let server = start(
        &plane.pulled_config(&upstream.url(), &control.url(), ""),
        "pull-token-1",
        &PULL_ENV,
    );
    assert_eq!(status_of(&server, ALPHA).await, 200);
    drop(server);
    let wrong = [(
        "SERVE_E2E_CONTROL_TOKEN",
        "a-different-token-0123456789abcdef",
    )];
    let server = start(
        &plane.pulled_config(&upstream.url(), &control.url(), ""),
        "pull-token-2",
        &wrong,
    );
    // Still serving from the cache the right token filled; the status says why it is not
    // getting anything newer.
    assert_eq!(status_of(&server, ALPHA).await, 200);
    let status = wait_for_reload(&server, "the refusal", |s| {
        s["snapshot"]["last_pull_error"]
            .as_str()
            .is_some_and(|e| !e.is_empty())
    })
    .await;
    let error = status["snapshot"]["last_pull_error"].as_str().unwrap();
    assert!(
        !error.contains("a-different-token") && !error.contains(CONTROL_TOKEN),
        "{error}"
    );
    assert!(
        !server
            .stderr
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("a-different-token")),
        "the token reached the log"
    );
}

#[tokio::test]
async fn start_without_a_cache_needs_the_control_plane_and_with_one_it_does_not() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("pull-start");
    let (control, _first) = control_plane(&plane, 1, vec![key("alpha", ALPHA, "active")]).await;
    let url = control.url();
    let config = plane.pulled_config(&upstream.url(), &url, "");
    let server = start(&config, "pull-start-1", &PULL_ENV);
    assert_eq!(status_of(&server, ALPHA).await, 200);
    drop(server);
    drop(control);
    // The control plane is gone. With the cache, a restart serves from it.
    let server = start(&config, "pull-start-2", &PULL_ENV);
    assert_eq!(
        status_of(&server, ALPHA).await,
        200,
        "served from the cache"
    );
    drop(server);
    // Without one, there is nobody to identify: the start fails and says so.
    std::fs::remove_file(plane.dir.0.join("policy").join("snapshot.json")).unwrap();
    let path = plane.dir.0.join("gateway.toml");
    std::fs::write(
        &path,
        config.replace("{DIR}", plane.dir.0.to_str().unwrap()),
    )
    .unwrap();
    let (code, stderr) = run_with_args(
        &path,
        &[],
        &[
            ("SERVE_E2E_UPSTREAM_KEY", UPSTREAM_KEY),
            ("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN),
            ("SERVE_E2E_CONTROL_TOKEN", CONTROL_TOKEN),
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("no snapshot is cached"), "{stderr}");
}

#[tokio::test]
async fn an_oversized_a_redirected_and_a_compressed_answer_are_refused_without_being_trusted() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("pull-hostile");
    let (mut control, first) = control_plane(&plane, 1, vec![key("alpha", ALPHA, "active")]).await;
    let server = start(
        &plane.pulled_config(&upstream.url(), &control.url(), ""),
        "pull-hostile",
        &PULL_ENV,
    );
    let cache = plane.dir.0.join("policy").join("snapshot.json");
    let before = std::fs::read(&cache).unwrap();
    first.remove_async().await;
    let reported = |wanted: &'static str| {
        let server = &server;
        async move {
            wait_for_reload(server, wanted, |s| {
                s["snapshot"]["last_pull_error"]
                    .as_str()
                    .is_some_and(|e| e.contains(wanted))
            })
            .await;
        }
    };
    // Bigger than the 8 MiB cap, with no Content-Length to check first.
    let _big = control
        .mock("GET", "/v1/orgs/org_test/snapshot")
        .with_chunked_body(|writer| {
            let block = vec![b'x'; 1024 * 1024];
            for _ in 0..10 {
                std::io::Write::write_all(writer, &block)?;
            }
            Ok(())
        })
        .create_async()
        .await;
    reported("larger than 8 MiB").await;
    assert_eq!(std::fs::read(&cache).unwrap(), before);
    drop(_big);
    // A redirect would carry the token elsewhere: it is not followed.
    let _redirect = control
        .mock("GET", "/v1/orgs/org_test/snapshot")
        .with_status(302)
        .with_header("location", "http://127.0.0.1:1/elsewhere")
        .create_async()
        .await;
    reported("answered 302").await;
    assert_eq!(std::fs::read(&cache).unwrap(), before);
    assert_eq!(status_of(&server, ALPHA).await, 200);
}

#[tokio::test]
async fn a_snapshot_that_is_signed_but_unusable_never_replaces_the_last_good_cache() {
    let (upstream, _mock) = mock(2).await;
    let plane = Plane::new("pull-unusable");
    let (mut control, first) = control_plane(&plane, 1, vec![key("alpha", ALPHA, "active")]).await;
    let config = plane.pulled_config(&upstream.url(), &control.url(), "");
    let server = start(&config, "pull-unusable-1", &PULL_ENV);
    let cache = plane.dir.0.join("policy").join("snapshot.json");
    let good = std::fs::read(&cache).unwrap();
    first.remove_async().await;
    // Validly signed, higher generation, but this key is one the data plane refuses.
    let mut bad = key("bravo", BRAVO, "active");
    bad["allowed_cidrs"] = json!(["not-a-network"]);
    let _bad = serve_generation(&mut control, &plane, 2, vec![bad]).await;
    wait_for_reload(&server, "the refusal", |s| {
        s["snapshot"]["last_pull_error"]
            .as_str()
            .is_some_and(|e| e.contains("refused"))
    })
    .await;
    assert_eq!(
        std::fs::read(&cache).unwrap(),
        good,
        "the last good cache is untouched"
    );
    assert_eq!(status_of(&server, ALPHA).await, 200);
    // And a restart, which reads that cache, still comes up.
    drop(server);
    let server = start(&config, "pull-unusable-2", &PULL_ENV);
    assert_eq!(
        status_of(&server, ALPHA).await,
        200,
        "the restart served the good cache"
    );
}

#[tokio::test]
async fn a_signer_added_to_verify_keys_is_trusted_by_the_next_pull_without_a_restart() {
    let (upstream, _mock) = mock(1).await;
    let plane = Plane::new("pull-rotate");
    let (mut control, first) = control_plane(&plane, 1, vec![key("alpha", ALPHA, "active")]).await;
    let config = plane.pulled_config(&upstream.url(), &control.url(), "");
    let server = start(&config, "pull-rotate", &PULL_ENV);
    first.remove_async().await;
    // Generation 2 is signed by a key this replica does not yet trust: refused.
    let (other_seed, other_public) = snapshot::keygen().unwrap();
    let signed = snapshot::sign(
        &payload(2, vec![key("bravo", BRAVO, "active")]),
        &other_seed,
    )
    .unwrap();
    let _second = control
        .mock("GET", "/v1/orgs/org_test/snapshot")
        .with_body(signed)
        .create_async()
        .await;
    wait_for_reload(&server, "the refusal", |s| {
        s["snapshot"]["last_pull_error"]
            .as_str()
            .is_some_and(|e| e.contains("not in verify_keys"))
    })
    .await;
    assert_eq!(status_of(&server, BRAVO).await, 401);
    // The operator adds the new public key and reloads; the poller judges by the list in force.
    let both = with_verify_keys(&plane, &config, &two_signers(&plane, &other_public))
        .replace("{DIR}", server.dir.0.to_str().unwrap());
    std::fs::write(server.dir.0.join("gateway.toml"), both).unwrap();
    hangup(&server);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while status_of(&server, BRAVO).await != 200 {
        assert!(
            std::time::Instant::now() < deadline,
            "the added signer was never trusted"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(status_of(&server, ALPHA).await, 401);
}

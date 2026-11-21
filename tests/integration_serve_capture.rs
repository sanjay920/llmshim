//! Prompt-structure capture (note 0052, slice 5a), through a real server process: a client that opted in has its
//! prompt cut into keyed-hash segments that ride the usage export; one that did not, has none; no prompt text and
//! no secret is in anything exported or logged; and a reload turns capture on or off for a client.
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use axum::extract::State;
use axum::routing::post;
use harness::{completion, hangup, rewrite, start, tempdir, ADMIN_TOKEN, CALLER_KEY};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PLAIN_KEY: &str = "plain-key-0123456789abcdefghij";
const CAPTURE_SECRET: &str = "capture-secret-0123456789-abcdefghijklmnop";
const EXPORT_TOKEN: &str = "export-token-0123456789abcdefghij";
const SECRET_PROMPT: &str = "SECRET-PROMPT-TEXT-launch-codes";

type Batches = Arc<Mutex<Vec<Value>>>;

/// A control plane that only remembers every usage record pushed to it.
async fn control_plane() -> (String, Batches) {
    async fn ingest(
        State(seen): State<Batches>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        let records = body["records"].as_array().cloned().unwrap_or_default();
        let count = records.len();
        seen.lock().unwrap().extend(records);
        axum::Json(json!({"accepted": count, "duplicate": 0, "rejected": 0}))
    }
    let seen: Batches = Default::default();
    let app = axum::Router::new()
        .route("/v1/usage/batch", post(ingest))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

fn render(upstream: &str, control: &str, ci_capture: &str, log: &std::path::Path) -> String {
    format!(
        r#"
        version = 1
        [server]
        port = 0
        [state]
        receipts_dir = "{{DIR}}/receipts"
        [providers.inference]
        kind = "openai-compatible"
        base_url = "{upstream}"
        api_key = {{ env = "SERVE_E2E_UPSTREAM_KEY" }}
        [[keys]]
        name = "ci"
        tenant = "platform"
        secret = {{ env = "SERVE_E2E_CALLER_KEY" }}
        {ci_capture}
        [[keys]]
        name = "plain"
        tenant = "platform"
        secret = {{ env = "SERVE_E2E_PLAIN_KEY" }}
        [admin]
        port = 0
        token = {{ env = "SERVE_E2E_ADMIN_TOKEN" }}
        [capture]
        hmac_key = {{ env = "SERVE_E2E_CAPTURE_SECRET" }}
        [usage_export]
        url = "{control}"
        allow_insecure_http = true
        token = {{ env = "SERVE_E2E_EXPORT_TOKEN" }}
        flush_interval_ms = 200
        batch_size = 1
        [logging]
        usage_log = "{log}"
        "#,
        log = log.display()
    )
}

fn env() -> [(&'static str, &'static str); 3] {
    [
        ("SERVE_E2E_PLAIN_KEY", PLAIN_KEY),
        ("SERVE_E2E_CAPTURE_SECRET", CAPTURE_SECRET),
        ("SERVE_E2E_EXPORT_TOKEN", EXPORT_TOKEN),
    ]
}

async fn ask(base: &str, key: &str, prompt: &str) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(key)
        .json(&json!({"model": "inference/m", "messages": [
            {"role": "system", "content": "You are terse."},
            {"role": "user", "content": prompt}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

/// Wait until `count` records of `key` have arrived.
async fn records_of(seen: &Batches, key: &str, count: usize) -> Vec<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let found: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["key"] == key)
            .cloned()
            .collect();
        if found.len() >= count {
            return found;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "records of {key}: {found:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn upstream() -> (mockito::ServerGuard, mockito::Mock) {
    let mut up = mockito::Server::new_async().await;
    let mock = up
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect_at_least(1)
        .create_async()
        .await;
    (up, mock)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_opted_in_client_exports_keyed_fingerprints_and_nothing_else_does() {
    let (up, _mock) = upstream().await;
    let (control, seen) = control_plane().await;
    let dir = tempdir::Dir::new("capture-log");
    let log = dir.0.join("usage.jsonl");
    let server = start(
        &render(&up.url(), &control, "capture = \"structure\"", &log),
        "capture",
        &env(),
    );
    ask(&server.base, CALLER_KEY, SECRET_PROMPT).await;
    ask(&server.base, CALLER_KEY, SECRET_PROMPT).await;
    ask(&server.base, CALLER_KEY, "a different question").await;
    ask(&server.base, PLAIN_KEY, SECRET_PROMPT).await;
    let ci = records_of(&seen, "ci", 3).await;
    let plain = records_of(&seen, "plain", 1).await;
    // The client that opted in: a system segment and a user segment, keyed 16-hex fingerprints and sizes.
    let segments = |r: &Value| r["structure"].as_array().expect("structure").clone();
    let first = segments(&ci[0]);
    assert_eq!(first.len(), 2, "{first:?}");
    assert_eq!(
        (first[0]["kind"].as_str(), first[1]["kind"].as_str()),
        (Some("system"), Some("user"))
    );
    for s in &first {
        let hash = s["hash"].as_str().unwrap();
        assert!(
            hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "{hash}"
        );
        assert!(s["bytes"].as_u64().unwrap() > 0);
    }
    // The same prompt has the same fingerprints; the one segment that changed is the only one that moves.
    let by_prompt = |prompt_changed: bool| {
        ci.iter()
            .filter(|r| (segments(r)[1] != first[1]) == prompt_changed)
            .count()
    };
    assert_eq!(by_prompt(false), 2, "two identical prompts");
    assert_eq!(by_prompt(true), 1, "one different prompt");
    let different = ci.iter().find(|r| segments(r)[1] != first[1]).unwrap();
    assert_eq!(
        segments(different)[0],
        first[0],
        "the unchanged system segment is the same"
    );
    // A client that did not opt in has no structure at all, not even an empty one.
    assert!(plain[0].get("structure").is_none(), "{}", plain[0]);
    // Nothing exported or logged holds the prompt or a secret.
    let exported = serde_json::to_string(&*seen.lock().unwrap()).unwrap();
    for needle in [SECRET_PROMPT, CAPTURE_SECRET, EXPORT_TOKEN, "You are terse"] {
        assert!(!exported.contains(needle), "the export holds {needle}");
    }
    // The usage log (read by anyone with the file) never carries the structure.
    let logged = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.lines().count() >= 4 {
            break text;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        !logged.contains("structure") && !logged.contains(CAPTURE_SECRET),
        "{logged}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_turns_capture_on_and_off_for_a_client() {
    let (up, _mock) = upstream().await;
    let (control, seen) = control_plane().await;
    let dir = tempdir::Dir::new("capture-reload-log");
    let log = dir.0.join("usage.jsonl");
    let server = start(
        &render(&up.url(), &control, "capture = \"structure\"", &log),
        "capture-reload",
        &env(),
    );
    ask(&server.base, CALLER_KEY, "one").await;
    let on = records_of(&seen, "ci", 1).await;
    assert!(on[0].get("structure").is_some());
    // Off: once the signal is handled, the next request carries none. A capture change alone is not a
    // "reload" in the status (nothing about who may call changed), so ask until the answer changes.
    rewrite(
        &server,
        &render(&up.url(), &control, "capture = \"off\"", &log),
    );
    hangup(&server);
    let mut sent = 1;
    sent = ask_until(&server.base, &seen, sent, |r| r.get("structure").is_none()).await;
    // And on again.
    rewrite(
        &server,
        &render(&up.url(), &control, "capture = \"structure\"", &log),
    );
    hangup(&server);
    ask_until(&server.base, &seen, sent, |r| r.get("structure").is_some()).await;
}

/// Send requests as `ci` until the newest record satisfies `wanted`; returns how many have been sent.
async fn ask_until(
    base: &str,
    seen: &Batches,
    mut sent: usize,
    wanted: impl Fn(&Value) -> bool,
) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        sent += 1;
        ask(base, CALLER_KEY, &format!("prompt {sent}")).await;
        let records = records_of(seen, "ci", sent).await;
        if wanted(&records[sent - 1]) {
            return sent;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never changed: {}",
            records[sent - 1]
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The variables the harness gives a started server, for a process run to its end.
fn with_base_env(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    let mut all = vec![
        ("SERVE_E2E_CALLER_KEY", CALLER_KEY),
        ("SERVE_E2E_UPSTREAM_KEY", harness::UPSTREAM_KEY),
        ("SERVE_E2E_ADMIN_TOKEN", harness::ADMIN_TOKEN),
        ("LLMSHIM_CATALOG_OFFLINE", "1"),
    ];
    all.extend_from_slice(extra);
    all
}

#[test]
fn the_configuration_refuses_capture_it_cannot_honour_before_serving() {
    let (code, stderr) = {
        let dir = tempdir::Dir::new("capture-refuse");
        let path = dir.0.join("gateway.toml");
        let config = render(
            "http://127.0.0.1:1",
            "http://127.0.0.1:2",
            "capture = \"structure\"",
            &dir.0.join("u.jsonl"),
        )
        .replace(
            "[capture]\n        hmac_key = { env = \"SERVE_E2E_CAPTURE_SECRET\" }",
            "",
        )
        .replace("{DIR}", dir.0.to_str().unwrap());
        std::fs::write(&path, config).unwrap();
        harness::run_to_exit(&path, &with_base_env(&env()))
    };
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("keys[0].capture") && stderr.contains("[capture]"),
        "{stderr}"
    );
    // A secret that is too short is refused by name, never printed.
    let dir = tempdir::Dir::new("capture-short");
    let path = dir.0.join("gateway.toml");
    let config = render(
        "http://127.0.0.1:1",
        "http://127.0.0.1:2",
        "capture = \"structure\"",
        &dir.0.join("u.jsonl"),
    )
    .replace("{DIR}", dir.0.to_str().unwrap());
    std::fs::write(&path, config).unwrap();
    let mut short = with_base_env(&env());
    short.retain(|(name, _)| *name != "SERVE_E2E_CAPTURE_SECRET");
    short.push(("SERVE_E2E_CAPTURE_SECRET", "too-short-secret"));
    let (code, stderr) = harness::run_to_exit(&path, &short);
    assert_ne!(code, Some(0), "{stderr}");
    assert!(
        stderr.contains("capture.hmac_key") && !stderr.contains("too-short-secret"),
        "{stderr}"
    );
}

// ---- payload capture (note 0052, slice 5b) ------------------------------------------------------

const PAYLOAD_SECRET: &str = "payload-secret-0123456789-abcdefghijklmnopqrs";

fn payload_env() -> Vec<(&'static str, &'static str)> {
    let mut all = env().to_vec();
    all.push(("SERVE_E2E_PAYLOAD_KEY", PAYLOAD_SECRET));
    all
}

/// The structure configuration plus the payload settings and a file media store.
fn render_payload(
    upstream: &str,
    control: &str,
    ci_capture: &str,
    log: &std::path::Path,
) -> String {
    format!(
        "{}\n[media_store]\nkind = \"file\"\npath = \"{{DIR}}/media\"\n",
        render(upstream, control, ci_capture, log).replace(
            "hmac_key = { env = \"SERVE_E2E_CAPTURE_SECRET\" }",
            "hmac_key = { env = \"SERVE_E2E_CAPTURE_SECRET\" }\n        payload_key = { env = \"SERVE_E2E_PAYLOAD_KEY\" }",
        )
    )
}

/// Every file under `dir`, with its bytes.
fn files_under(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }
    out
}

async fn open_payload(
    admin: &str,
    token: Option<&str>,
    body: Value,
) -> (u16, Value, Option<String>) {
    let mut request = reqwest::Client::new()
        .post(format!("{admin}/v1/serve/payload/open"))
        .json(&body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let cache = response
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (status, response.json().await.unwrap_or(Value::Null), cache)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payload_is_sealed_on_the_data_plane_and_opens_only_through_the_admin_listener() {
    let (up, _mock) = upstream().await;
    let (control, seen) = control_plane().await;
    let dir = tempdir::Dir::new("payload-log");
    let log = dir.0.join("usage.jsonl");
    let server = start(
        &render_payload(&up.url(), &control, "capture = \"payload\"", &log),
        "payload",
        &payload_env(),
    );
    ask(&server.base, CALLER_KEY, SECRET_PROMPT).await;
    ask(&server.base, PLAIN_KEY, "another prompt").await;
    let ci = records_of(&seen, "ci", 1).await;
    let plain = records_of(&seen, "plain", 1).await;
    // A reference and a size and an expiry, and nothing of the text, in what the control plane received.
    let payload = &ci[0]["payload"];
    let reference = payload["reference"]
        .as_str()
        .expect("a reference")
        .to_owned();
    assert!(payload["bytes"].as_u64().unwrap() > 0);
    assert!(payload["expires_at"].as_str().unwrap().ends_with('Z'));
    assert!(
        ci[0].get("structure").is_some(),
        "a payload client captures structure too"
    );
    assert!(
        plain[0].get("payload").is_none(),
        "a client that did not opt in has no payload"
    );
    let exported = serde_json::to_string(&*seen.lock().unwrap()).unwrap();
    for needle in [
        SECRET_PROMPT,
        "pong",
        PAYLOAD_SECRET,
        CAPTURE_SECRET,
        "You are terse",
    ] {
        assert!(!exported.contains(needle), "the export holds {needle}");
    }
    // The object store holds ciphertext under a name that says nothing.
    let media = server.dir.0.join("media");
    let object = {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let found: Vec<_> = files_under(&media)
                .into_iter()
                .filter(|(p, _)| {
                    // The finished object, not the temporary file it is written through.
                    p.to_string_lossy().contains("capture")
                        && !p
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with(".tmp-"))
                })
                .collect();
            if let Some(file) = found.into_iter().next() {
                break file;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no sealed object appeared"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let at_rest = String::from_utf8_lossy(&object.1).into_owned();
    for needle in [SECRET_PROMPT, "pong", "You are terse", "inference"] {
        assert!(
            !at_rest.contains(needle),
            "the stored object holds {needle}"
        );
    }
    assert!(
        !object.0.to_string_lossy().to_lowercase().contains("ci"),
        "the name says nothing: {:?}",
        object.0
    );
    // Opened through the admin listener, with its token, the reference gives the request and the answer back.
    let admin = server.admin.clone().unwrap();
    let (status, body, cache) =
        open_payload(&admin, Some(ADMIN_TOKEN), json!({"reference": reference})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(cache.as_deref(), Some("no-store"));
    let document = &body["payload"];
    assert_eq!(document["wire"], "openai-chat");
    assert!(document["request"]["text"]
        .as_str()
        .unwrap()
        .contains(SECRET_PROMPT));
    assert!(document["response"]["text"]
        .as_str()
        .unwrap()
        .contains("pong"));
    // Nothing else opens it: no token, a wrong token, a reference that is not one, one that was altered.
    assert_eq!(
        open_payload(&admin, None, json!({"reference": reference}))
            .await
            .0,
        401
    );
    assert_eq!(
        open_payload(
            &admin,
            Some("not-the-admin-token-0123456789"),
            json!({"reference": reference})
        )
        .await
        .0,
        401
    );
    let (status, body, cache) =
        open_payload(&admin, Some(ADMIN_TOKEN), json!({"reference": "garbage"})).await;
    assert_eq!(
        (status, body["error"].as_str(), cache.as_deref()),
        (422, Some("bad_reference"), Some("no-store"))
    );
    assert_eq!(
        open_payload(&admin, Some(ADMIN_TOKEN), json!({})).await.0,
        422
    );
    let mut altered = reference.clone().into_bytes();
    let last = altered.len() / 2;
    altered[last] = if altered[last] == b'A' { b'B' } else { b'A' };
    assert_eq!(
        open_payload(
            &admin,
            Some(ADMIN_TOKEN),
            json!({"reference": String::from_utf8(altered).unwrap()})
        )
        .await
        .0,
        422
    );
    // The route is not on the main listener, where callers are.
    let main = reqwest::Client::new()
        .post(format!("{}/v1/serve/payload/open", server.base))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"reference": reference}))
        .send()
        .await
        .unwrap();
    assert_eq!(main.status().as_u16(), 404);
    // The usage log never carries the reference.
    let logged = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.lines().count() >= 2 {
            break text;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        !logged.contains("payload")
            && !logged.contains(&reference)
            && !logged.contains(SECRET_PROMPT),
        "{logged}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_changes_whether_a_client_captures_payloads() {
    let (up, _mock) = upstream().await;
    let (control, seen) = control_plane().await;
    let dir = tempdir::Dir::new("payload-reload-log");
    let log = dir.0.join("usage.jsonl");
    let server = start(
        &render_payload(&up.url(), &control, "capture = \"payload\"", &log),
        "payload-reload",
        &payload_env(),
    );
    ask(&server.base, CALLER_KEY, "one").await;
    assert!(records_of(&seen, "ci", 1).await[0].get("payload").is_some());
    // Structure only: the payload stops, the structure goes on.
    rewrite(
        &server,
        &render_payload(&up.url(), &control, "capture = \"structure\"", &log),
    );
    hangup(&server);
    let sent = ask_until(&server.base, &seen, 1, |r| r.get("payload").is_none()).await;
    let newest = records_of(&seen, "ci", sent).await;
    assert!(newest[sent - 1].get("structure").is_some());
    // And back.
    rewrite(
        &server,
        &render_payload(&up.url(), &control, "capture = \"payload\"", &log),
    );
    hangup(&server);
    ask_until(&server.base, &seen, sent, |r| r.get("payload").is_some()).await;
}

#[test]
fn payload_capture_is_refused_before_serving_when_it_cannot_be_honoured() {
    let run =
        |label: &str, edit: &dyn Fn(String) -> String, env: &[(&'static str, &'static str)]| {
            let dir = tempdir::Dir::new(label);
            let path = dir.0.join("gateway.toml");
            let config = edit(render_payload(
                "http://127.0.0.1:1",
                "http://127.0.0.1:2",
                "capture = \"payload\"",
                &dir.0.join("u.jsonl"),
            ))
            .replace("{DIR}", dir.0.to_str().unwrap());
            std::fs::write(&path, config).unwrap();
            harness::run_to_exit(&path, &with_base_env(env))
        };
    let (code, stderr) = run(
        "no-store",
        &|c| {
            c.replace(
                "[media_store]\nkind = \"file\"\npath = \"{DIR}/media\"\n",
                "",
            )
        },
        &payload_env(),
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("keys[0].capture") && stderr.contains("[media_store]"),
        "{stderr}"
    );
    let (code, stderr) = run(
        "no-key",
        &|c| c.replace("payload_key = { env = \"SERVE_E2E_PAYLOAD_KEY\" }", ""),
        &payload_env(),
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("payload_key"), "{stderr}");
    let (code, stderr) = run(
        "no-admin-token",
        &|c| {
            c.replace(
                "token = { env = \"SERVE_E2E_ADMIN_TOKEN\" }",
                "allow_unauthenticated = true",
            )
        },
        &payload_env(),
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("[admin]") && stderr.contains("token"),
        "{stderr}"
    );
    let (code, stderr) = run(
        "bad-ttl",
        &|c| {
            c.replace(
                "payload_key = { env",
                "ttl_secs = 5\n        payload_key = { env",
            )
        },
        &payload_env(),
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("capture.ttl_secs"), "{stderr}");
    // A short secret is refused by name and never printed.
    let mut short = payload_env();
    short.retain(|(n, _)| *n != "SERVE_E2E_PAYLOAD_KEY");
    short.push(("SERVE_E2E_PAYLOAD_KEY", "too-short"));
    let (code, stderr) = run("short-key", &|c| c, &short);
    assert_ne!(code, Some(0), "{stderr}");
    assert!(
        stderr.contains("capture.payload_key") && !stderr.contains("too-short"),
        "{stderr}"
    );
}

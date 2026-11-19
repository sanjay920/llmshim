//! `llmshim serve` as a fleet: two real server processes sharing one Redis.
//!
//! Needs a Redis instance: `LLMSHIM_TEST_REDIS_URL=redis://127.0.0.1:6379
//! cargo test --features serve-redis --test integration_serve_fleet -- --ignored`
//! The `Serve feature` CI job runs this against a Redis service container.
#![cfg(feature = "serve-redis")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{chat, completion, config, hangup, rewrite, start, wait_for_reload, CALLER_KEY};
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

    /// A string only this test's jobs carry (the provider name is unique to the test).
    fn tenant_hash_marker(&self) -> String {
        self.provider.clone()
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

#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn usage_records_name_the_tenant_whichever_replica_dispatches() {
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect_at_least(2)
        .create_async()
        .await;
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    let names = Names::new();
    let directory = harness::tempdir::Dir::new("fleet-usage");
    let log_for = |replica: &str| directory.0.join(format!("usage-{replica}.jsonl"));
    // The two replicas name the same key differently, as a rollout in progress
    // might: the name a record carries is the dispatching replica's own, and not
    // one that travelled with the job.
    let text = |replica: &str| {
        fleet_config(&upstream.url(), &names, "")
            .replace(
                "[state]",
                &format!(
                    "[logging]\nusage_log = \"{}\"\n[state]",
                    log_for(replica).display()
                ),
            )
            .replace("name = \"ci\"", &format!("name = \"ci-{replica}\""))
    };
    let a = start(&text("a"), "usage-a", &env);
    let b = start(&text("b"), "usage-b", &env);
    let read = |replica: &str| -> Vec<Value> {
        std::fs::read_to_string(log_for(replica))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };

    // Every request enters through replica A; either replica may dispatch it.
    // Keep going until both have dispatched at least one, which they do within a
    // handful of requests (they split evenly in practice).
    let mut sent = 0;
    let mut ids = Vec::new();
    let (mut from_a, mut from_b) = (Vec::new(), Vec::new());
    while sent < 40 && (from_a.is_empty() || from_b.is_empty()) {
        let response = chat(&a, Some(CALLER_KEY), &names.model()).await;
        assert_eq!(response.status(), 200);
        ids.push(
            response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned(),
        );
        sent += 1;
        (from_a, from_b) = (read("a"), read("b"));
    }
    assert!(
        !from_a.is_empty() && !from_b.is_empty(),
        "one replica never dispatched ({sent} requests): a={} b={}",
        from_a.len(),
        from_b.len()
    );
    // One record per request, each naming the tenant, including those written
    // by the replica the caller never connected to.
    assert_eq!(from_a.len() + from_b.len(), sent);
    for record in from_a.iter().chain(&from_b) {
        assert_eq!(record["tenant"], names.tenant.as_str(), "{record}");
        assert_eq!(record["provider"], names.provider.as_str());
        assert_eq!(record["outcome"], "completed");
    }
    // The key is named too, from each replica's own configuration: the job
    // carried a prefix of the credential's digest through Redis, never the name.
    for record in &from_a {
        assert_eq!(record["key"], "ci-a", "{record}");
    }
    for record in &from_b {
        assert_eq!(record["key"], "ci-b", "{record}");
    }
    // The id the caller saw on its response, on the record of the replica that
    // dispatched it, whichever replica that was. Replica A minted every one.
    let mut recorded: Vec<String> = from_a
        .iter()
        .chain(&from_b)
        .map(|record| record["request_id"].as_str().unwrap().to_owned())
        .collect();
    recorded.sort();
    ids.sort();
    assert_eq!(recorded, ids);
    drop((a, b));
}

#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn a_provider_added_by_a_reload_is_served_by_the_replica_from_then_on() {
    // 0022: the fleet's workers follow the provider set. Before the reload this
    // replica has no worker for the new provider (its job would wait in Redis);
    // after it, the replica leases the provider's jobs and answers.
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(1)
        .create_async()
        .await;
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    let names = Names::new();
    let added = format!("added-{}", uuid::Uuid::new_v4().simple());
    let admin = "\n[admin]\nport = 0\ntoken = { env = \"SERVE_E2E_ADMIN_TOKEN\" }\n";
    let before = format!("{}{admin}", fleet_config(&upstream.url(), &names, ""));
    let server = start(&before, "fleet-reload-provider", &env);
    let provider = format!(
        "\n[providers.{added}]\nkind = \"openai-compatible\"\nbase_url = \"{}\"\n\
         api_key = {{ env = \"SERVE_E2E_UPSTREAM_KEY\" }}\n",
        upstream.url()
    );
    rewrite(&server, &format!("{before}{provider}"));
    hangup(&server);
    let status = wait_for_reload(&server, "the added provider", |status| {
        status["router_generation"] == 1
    })
    .await;
    assert_eq!(status["last_reload"]["outcome"], "applied", "{status}");
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        chat(&server, Some(CALLER_KEY), &format!("{added}/test-model")),
    )
    .await
    .expect("the replica's worker for the added provider leases its job");
    assert_eq!(response.status(), 200);
    mock.assert_async().await;
}

#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn embeddings_run_through_the_fleet_and_are_answered_whichever_replica_dispatches() {
    // The job carries its operation in its scope (note 0041): whichever replica's worker takes it
    // sends an embeddings call, not a chat completion, and the origin answers its caller.
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/embeddings")
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "data": [{"index": 0, "embedding": [0.5, 0.25]}],
                "usage": {"prompt_tokens": 3, "total_tokens": 3}
            })
            .to_string(),
        )
        .expect(4)
        .create_async()
        .await;
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    let names = Names::new();
    let text = fleet_config(&upstream.url(), &names, "");
    let a = start(&text, "fleet-emb-a", &env);
    let b = start(&text, "fleet-emb-b", &env);
    // Every request is accepted by the replica it is sent to and run by whichever worker of the
    // fleet takes the job from the shared queue; what this proves is that the operation survives the
    // trip through Redis (a worker rebuilds its scope from the job's JSON, and a lost operation
    // would have called `/chat/completions`, which the mock does not serve), not which replica ran it.
    for server in [&a, &b, &a, &b] {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/embeddings", server.base))
            .bearer_auth(CALLER_KEY)
            .json(&serde_json::json!({"model": names.model(), "input": ["hello"]}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, 200, "{body}");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["data"][0]["embedding"], serde_json::json!([0.5, 0.25]));
        assert_eq!(body["usage"]["prompt_tokens"], 3);
    }
    // A batch whose answer could pass the fleet's terminal limit is refused before it is sent.
    let too_many: Vec<String> = (0..513).map(|n| format!("text {n}")).collect();
    let refused = reqwest::Client::new()
        .post(format!("{}/v1/embeddings", a.base))
        .bearer_auth(CALLER_KEY)
        .json(&serde_json::json!({"model": names.model(), "input": too_many}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
    mock.assert_async().await;
}

#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn the_big_media_routes_answer_501_naming_the_store_on_a_fleet_without_one() {
    // Images, speech and transcription carry megabytes: through the media object store (note 0050),
    // and without `[media_store]` a fleet says so by name instead of a bare refusal.
    let upstream = mockito::Server::new_async().await;
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    let names = Names::new();
    let a = start(
        &fleet_config(&upstream.url(), &names, ""),
        "fleet-501",
        &env,
    );
    for (path, body) in [
        (
            "/v1/images/generations",
            serde_json::json!({"model": names.model(), "prompt": "a cat"}),
        ),
        (
            "/v1/audio/speech",
            serde_json::json!({"model": names.model(), "input": "hi", "voice": "alloy"}),
        ),
    ] {
        let response = reqwest::Client::new()
            .post(format!("{}{path}", a.base))
            .bearer_auth(CALLER_KEY)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 501, "{path}");
        assert!(
            response.text().await.unwrap().contains("[media_store]"),
            "{path}"
        );
    }
    let upload = reqwest::multipart::Form::new()
        .text("model", names.model())
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"abc".to_vec()).file_name("a.wav"),
        );
    let response = reqwest::Client::new()
        .post(format!("{}/v1/audio/transcriptions", a.base))
        .bearer_auth(CALLER_KEY)
        .multipart(upload)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 501, "transcription");
    assert!(response.text().await.unwrap().contains("[media_store]"));
}

/// A fleet of the OpenAI kind (images, speech and transcription are its adapters') with a media store
/// on a directory the replicas share, and an admin listener for the counters.
fn media_config(upstream: &str, names: &Names, shared: &std::path::Path) -> String {
    let text = fleet_config(upstream, names, "")
        .replace("kind = \"openai-compatible\"", "kind = \"openai\"");
    format!(
        "{text}\n[admin]\nport = 0\ntoken = {{ env = \"SERVE_E2E_ADMIN_TOKEN\" }}\n\
         [media_store]\nkind = \"file\"\npath = \"{}\"\nttl_secs = 900\n",
        shared.display()
    )
}

fn files_in(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(files_in(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

async fn counter(server: &harness::Server, series: &str) -> u64 {
    let body = reqwest::Client::new()
        .get(format!("{}/metrics", server.admin.clone().unwrap()))
        .bearer_auth(harness::ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    body.lines()
        .filter(|line| line.starts_with(series))
        .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

const MARKER: &[u8] = b"caller-audio-MARKER-0123456789";

fn upload(model: &str) -> reqwest::multipart::Form {
    // 60 KB: past the 32 KiB threshold, so the base64 of it goes to the store.
    let audio: Vec<u8> = MARKER.iter().copied().cycle().take(60_000).collect();
    reqwest::multipart::Form::new()
        .text("model", model.to_owned())
        .part(
            "file",
            reqwest::multipart::Part::bytes(audio)
                .file_name("clip.wav")
                .mime_str("audio/wav")
                .unwrap(),
        )
}

#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn images_speech_and_transcription_run_through_the_fleet_over_the_media_store() {
    let big_image = "A".repeat(60_000);
    let speech: Vec<u8> = (0..100_000u32).map(|n| (n % 251) as u8).collect();
    let mut upstream = mockito::Server::new_async().await;
    let images = upstream
        .mock("POST", "/images/generations")
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({"created": 1, "data": [{"b64_json": big_image}],
                "usage": {"input_tokens": 5, "output_tokens": 100}})
            .to_string(),
        )
        .expect(2)
        .create_async()
        .await;
    let audio_out = upstream
        .mock("POST", "/audio/speech")
        .with_header("content-type", "audio/mpeg")
        .with_body(speech.clone())
        .expect(2)
        .create_async()
        .await;
    // The transcription mock checks the upload arrived whole: the worker read it back from the store.
    let transcripts = upstream
        .mock("POST", "/audio/transcriptions")
        .with_header("content-type", "application/json")
        .with_body_from_request(|request| {
            let body = request.body().unwrap();
            let found = body
                .windows(MARKER.len())
                .filter(|window| *window == MARKER)
                .count();
            serde_json::json!({"text": format!("heard {found} markers"),
                "usage": {"type": "tokens", "input_tokens": 14, "output_tokens": 5,
                    "total_tokens": 19}})
            .to_string()
            .into_bytes()
        })
        .expect(2)
        .create_async()
        .await;
    let shared = harness::tempdir::Dir::new("fleet-media-shared");
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    let names = Names::new();
    let text = media_config(&upstream.url(), &names, &shared.0);
    let a = start(&text, "fleet-media-a", &env);
    let b = start(&text, "fleet-media-b", &env);
    let client = reqwest::Client::new();
    for server in [&a, &b] {
        // Images: the answer's base64 goes through the store and comes back whole.
        let image = client
            .post(format!("{}/v1/images/generations", server.base))
            .bearer_auth(CALLER_KEY)
            .json(
                &serde_json::json!({"model": format!("{}/gpt-image-1", names.provider),
                "prompt": "a cat"}),
            )
            .send()
            .await
            .unwrap();
        let status = image.status();
        let body = image.text().await.unwrap();
        assert_eq!(status, 200, "{}", &body[..body.len().min(300)]);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["data"][0]["b64_json"].as_str().unwrap().len(), 60_000);
        // Speech: the audio is the body, byte for byte.
        let spoken = client
            .post(format!("{}/v1/audio/speech", server.base))
            .bearer_auth(CALLER_KEY)
            .json(
                &serde_json::json!({"model": format!("{}/tts-1", names.provider),
                "input": "hello", "voice": "alloy"}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(spoken.status(), 200);
        assert_eq!(spoken.bytes().await.unwrap().as_ref(), speech.as_slice());
        // Transcription: the 60 KB upload reaches the provider whole, on whichever replica runs it.
        let transcript = client
            .post(format!("{}/v1/audio/transcriptions", server.base))
            .bearer_auth(CALLER_KEY)
            .multipart(upload(&format!("{}/gpt-4o-transcribe", names.provider)))
            .send()
            .await
            .unwrap();
        let status = transcript.status();
        let body = transcript.text().await.unwrap();
        assert_eq!(status, 200, "{body}");
        let expected = 60_000 / MARKER.len();
        assert!(
            body.contains(&format!("heard {expected} markers")),
            "{body}"
        );
    }
    images.assert_async().await;
    audio_out.assert_async().await;
    transcripts.assert_async().await;
    // Nothing outlives its request: the store is empty again.
    assert_eq!(files_in(&shared.0), Vec::<std::path::PathBuf>::new());
    // And the store was used: two uploads staged, four answers put, six objects read back.
    let sum = |series: &'static str| {
        let (a, b) = (&a, &b);
        async move { counter(a, series).await + counter(b, series).await }
    };
    assert_eq!(
        sum("llmshim_gateway_media_store_total{op=\"stage\",outcome=\"ok\"}").await,
        2
    );
    assert_eq!(
        sum("llmshim_gateway_media_store_total{op=\"write\",outcome=\"ok\"}").await,
        4
    );
}

#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn an_object_in_the_store_is_ciphertext_and_is_gone_when_the_request_ends() {
    // The provider is slow, so the request is in flight while the test reads the store.
    let mut upstream = mockito::Server::new_async().await;
    let _slow = upstream
        .mock("POST", "/audio/transcriptions")
        .with_header("content-type", "application/json")
        .with_body_from_request(|_| {
            std::thread::sleep(std::time::Duration::from_secs(3));
            br#"{"text": "done", "usage": {"type": "tokens", "input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}"#.to_vec()
        })
        .create_async()
        .await;
    let shared = harness::tempdir::Dir::new("fleet-media-sealed");
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    let names = Names::new();
    let a = start(
        &media_config(&upstream.url(), &names, &shared.0),
        "fleet-sealed",
        &env,
    );
    let base = a.base.clone();
    let model = format!("{}/gpt-4o-transcribe", names.provider);
    let call = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{base}/v1/audio/transcriptions"))
            .bearer_auth(CALLER_KEY)
            .multipart(upload(&model))
            .send()
            .await
            .unwrap()
            .status()
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let object = loop {
        if let Some(path) = files_in(&shared.0).into_iter().next() {
            break path;
        }
        assert!(std::time::Instant::now() < deadline, "no object was staged");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    // The job in Redis carries references, not the audio, and is scope version 4 (so a replica that
    // cannot read references refuses it instead of sending one to a provider).
    let found = {
        let mut connection = redis::Client::open(redis_url())
            .unwrap()
            .get_connection()
            .unwrap();
        let keys: Vec<String> = redis::cmd("KEYS").arg("*").query(&mut connection).unwrap();
        let mut found = None;
        for key in keys {
            let value: Result<String, _> = redis::cmd("GET").arg(&key).query(&mut connection);
            if let Ok(value) = value {
                if value.contains("$blob") && value.contains(&names.tenant_hash_marker()) {
                    found = Some(value);
                }
            }
        }
        found
    };
    let job = found.expect("a queued job that carries a reference");
    assert!(job.contains("\"version\":4"), "{job}");
    assert!(
        !job.contains("audio_b64\":\""),
        "the audio itself is not in the job"
    );
    let bytes = std::fs::read(&object).unwrap();
    // The audio's base64 was staged, sealed: neither the audio nor its base64 is in the file.
    assert!(bytes.len() > 60_000);
    assert!(!bytes.windows(MARKER.len()).any(|w| w == MARKER));
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, MARKER);
    assert!(!bytes
        .windows(12)
        .any(|w| encoded.as_bytes().starts_with(w) && w.iter().all(u8::is_ascii_alphanumeric)));
    // The name says nothing: a date directory and 128 random bits.
    let relative = object
        .strip_prefix(&shared.0)
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(relative.len() == 8 + 1 + 32, "{relative}");
    assert_eq!(call.await.unwrap(), 200);
    assert_eq!(files_in(&shared.0), Vec::<std::path::PathBuf>::new());
}

#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn a_caller_that_disconnects_leaves_no_object_behind() {
    let mut upstream = mockito::Server::new_async().await;
    let _slow = upstream
        .mock("POST", "/audio/transcriptions")
        .with_header("content-type", "application/json")
        .with_body_from_request(|_| {
            std::thread::sleep(std::time::Duration::from_secs(4));
            br#"{"text": "late", "usage": {"type": "tokens", "input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}"#.to_vec()
        })
        .create_async()
        .await;
    let shared = harness::tempdir::Dir::new("fleet-media-gone");
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    let names = Names::new();
    let a = start(
        &media_config(&upstream.url(), &names, &shared.0),
        "fleet-gone",
        &env,
    );
    let base = a.base.clone();
    let model = format!("{}/gpt-4o-transcribe", names.provider);
    let call = tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .post(format!("{base}/v1/audio/transcriptions"))
            .bearer_auth(CALLER_KEY)
            .multipart(upload(&model))
            .send()
            .await;
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while files_in(&shared.0).is_empty() {
        assert!(std::time::Instant::now() < deadline, "no object was staged");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // The caller goes away while the provider is still thinking.
    call.abort();
    let _ = call.await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !files_in(&shared.0).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the object of a request nobody is waiting for was left in the store"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn a_store_that_cannot_take_the_upload_is_a_503_and_nothing_is_sent_to_the_provider() {
    use std::os::unix::fs::PermissionsExt;
    let mut upstream = mockito::Server::new_async().await;
    let provider = upstream
        .mock("POST", "/audio/transcriptions")
        .expect(0)
        .create_async()
        .await;
    let shared = harness::tempdir::Dir::new("fleet-media-down");
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    let names = Names::new();
    let a = start(
        &media_config(&upstream.url(), &names, &shared.0),
        "fleet-down",
        &env,
    );
    // The directory stops being writable after the server started with it.
    std::fs::set_permissions(&shared.0, std::fs::Permissions::from_mode(0o500)).unwrap();
    let response = reqwest::Client::new()
        .post(format!("{}/v1/audio/transcriptions", a.base))
        .bearer_auth(CALLER_KEY)
        .multipart(upload(&format!("{}/gpt-4o-transcribe", names.provider)))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let retry_after = response.headers().get("retry-after").cloned();
    let body = response.text().await.unwrap();
    std::fs::set_permissions(&shared.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("media_store_unavailable"), "{body}");
    assert!(retry_after.is_some());
    // No path, no class detail of the store is in the answer.
    assert!(!body.contains(&shared.0.display().to_string()), "{body}");
    provider.assert_async().await;
    assert_eq!(
        counter(
            &a,
            "llmshim_gateway_media_store_total{op=\"stage\",outcome=\"denied\"}"
        )
        .await,
        1
    );
}

#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn a_caller_cannot_name_an_object_and_a_failed_job_leaves_nothing_behind() {
    let mut upstream = mockito::Server::new_async().await;
    let _failing = upstream
        .mock("POST", "/audio/transcriptions")
        .with_status(500)
        .with_body("upstream broke")
        .expect_at_least(1)
        .create_async()
        .await;
    let shared = harness::tempdir::Dir::new("fleet-media-fail");
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    let names = Names::new();
    let a = start(
        &media_config(&upstream.url(), &names, &shared.0),
        "fleet-fail",
        &env,
    );
    // A reference in the caller's own body is refused before anything is staged.
    let forged = reqwest::Client::new()
        .post(format!("{}/v1/images/generations", a.base))
        .bearer_auth(CALLER_KEY)
        .json(&serde_json::json!({"model": format!("{}/gpt-image-1", names.provider),
            "prompt": "a cat",
            "extra": {"$blob": {"v": 1, "key": "20260101/0123456789abcdef0123456789abcdef",
                "len": 5, "k": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", "n": "AAAAAAAAAAAAAAAA"}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 400);
    assert!(files_in(&shared.0).is_empty());
    // A provider that fails: the caller gets the failure, and the staged upload is deleted.
    let failed = reqwest::Client::new()
        .post(format!("{}/v1/audio/transcriptions", a.base))
        .bearer_auth(CALLER_KEY)
        .multipart(upload(&format!("{}/gpt-4o-transcribe", names.provider)))
        .send()
        .await
        .unwrap();
    assert!(failed.status().is_server_error(), "{}", failed.status());
    assert_eq!(files_in(&shared.0), Vec::<std::path::PathBuf>::new());
}

fn stream_body() -> String {
    let chunk = |delta: &str, finish: &str, extra: &str| {
        format!(
            "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\
             \"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{delta},\
             \"finish_reason\":{finish}}}]{extra}}}"
        )
    };
    [
        chunk(r#"{"role":"assistant","content":"po"}"#, "null", ""),
        chunk(
            r#"{"content":"ng"}"#,
            r#""stop""#,
            r#","usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}"#,
        ),
        "data: [DONE]".to_string(),
    ]
    .join("\n\n")
        + "\n\n"
}

#[tokio::test]
#[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
async fn a_stream_that_fails_to_open_on_a_fleet_is_retried_on_the_next_target() {
    // On a fleet the open failure is an error item after admission has returned (note 0042); the
    // handler reads the first item of a deployment stream and retries on the next target.
    let mut east = mockito::Server::new_async().await;
    let east_mock = east
        .mock("POST", "/chat/completions")
        .with_status(502)
        .with_header("content-type", "application/json")
        .with_body("{\"error\":{\"message\":\"nope\"}}")
        .expect(2)
        .create_async()
        .await;
    let mut west = mockito::Server::new_async().await;
    let west_mock = west
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(stream_body())
        .expect(2)
        .create_async()
        .await;
    let url = redis_url();
    let env = [("SERVE_E2E_REDIS_URL", url.as_str())];
    let names = Names::new();
    let second = format!("{}-west", names.provider);
    let text = format!(
        "{}\n[providers.{second}]\nkind = \"openai-compatible\"\nbase_url = \"{}\"\n\
         [deployments.chat]\nstrategy = \"ordered\"\n\
         targets = [{{ model = \"{}\" }}, {{ model = \"{second}/test-model\" }}]\n\
         [upstream]\nmax_retries = 0\n",
        fleet_config(&east.url(), &names, ""),
        west.url(),
        names.model(),
    );
    let a = start(&text, "fleet-stream-a", &env);
    let b = start(&text, "fleet-stream-b", &env);
    for server in [&a, &b] {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", server.base))
            .bearer_auth(CALLER_KEY)
            .json(
                &serde_json::json!({"model": "deployment/chat", "stream": true,
                "messages": [{"role": "user", "content": "ping"}]}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.text().await.unwrap();
        assert!(body.contains("po") && body.contains("ng"), "{body}");
        assert!(!body.contains("error"), "{body}");
    }
    east_mock.assert_async().await;
    west_mock.assert_async().await;
}

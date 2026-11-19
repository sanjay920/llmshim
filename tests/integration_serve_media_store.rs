//! The media object store (note 0050) against a real S3-compatible server. Ignored unless
//! `LLMSHIM_TEST_S3_ENDPOINT` names one (MinIO or SeaweedFS in CI), with an existing bucket in
//! `LLMSHIM_TEST_S3_BUCKET`, `LLMSHIM_TEST_S3_KEY_ID` and `LLMSHIM_TEST_S3_SECRET`. It proves the
//! SigV4 signature, the path-style address, put/get/delete, the paged listing and the sweeper
//! against a server that really verifies the signature.
#![cfg(feature = "serve")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{run_with_args, tempdir};
use llmshim::serve::blobstore::{self, BlobError, BlobStore};
use llmshim::serve::config::MediaStore;
use serde_json::json;
use std::time::Duration;

fn setting(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn handle(secret_env: &str, prefix: &str) -> blobstore::Handle {
    let config: MediaStore = serde_json::from_value(json!({
        "kind": "s3",
        "bucket": setting("LLMSHIM_TEST_S3_BUCKET"),
        "region": "us-east-1",
        "endpoint": setting("LLMSHIM_TEST_S3_ENDPOINT"),
        "allow_insecure_http": true,
        "prefix": prefix,
        "access_key_id": {"env": "LLMSHIM_TEST_S3_KEY_ID"},
        "secret_access_key": {"env": secret_env},
    }))
    .unwrap();
    blobstore::from_config(&config, &|name| std::env::var(name).ok()).unwrap()
}

#[tokio::test]
#[ignore = "needs an S3-compatible server (LLMSHIM_TEST_S3_ENDPOINT)"]
async fn a_real_s3_server_stores_lists_sweeps_and_refuses_a_bad_signature() {
    let prefix = format!("test-{}/", uuid::Uuid::new_v4().simple());
    let good = handle("LLMSHIM_TEST_S3_SECRET", &prefix);
    let store: &dyn BlobStore = good.store.as_ref();

    // A 5 MiB sealed object makes the round trip, and the server holds ciphertext.
    let text = format!("caller audio {}", "A".repeat(5 << 20));
    let reference = blobstore::put_sealed(store, "", &text).await.unwrap();
    assert!(blobstore::valid_name(&reference.name) && !reference.name.starts_with(&prefix));
    assert_eq!(
        blobstore::get_sealed(store, &reference).await.unwrap(),
        text
    );

    // A large string in a job moves and comes back whole; the request's end deletes it.
    let original = json!({"audio_b64": "Q".repeat(100_000), "n": 1});
    let mut value = original.clone();
    let staged = blobstore::externalize(store, "", &mut value, blobstore::THRESHOLD_BYTES)
        .await
        .unwrap();
    assert_eq!(staged.names().len(), 1);
    blobstore::internalize(store, &mut value, usize::MAX)
        .await
        .unwrap();
    assert_eq!(value, original);
    let names = staged.names().to_vec();
    assert_eq!(staged.release(store).await, 0);
    assert_eq!(
        store.get(&names[0], 1 << 20).await,
        Err(BlobError::NotFound)
    );

    // The listing sees the first object (the prefix removed), and the sweeper removes it.
    let listed = store.list().await.unwrap();
    assert!(listed.iter().any(|o| o.key == reference.name), "{listed:?}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        blobstore::sweep(store, Duration::from_secs(1))
            .await
            .unwrap()
            >= 1
    );
    assert_eq!(
        blobstore::get_sealed(store, &reference).await,
        Err(BlobError::NotFound)
    );

    // A wrong secret is refused by the server, as a class and not a message.
    // SAFETY of the test: a distinct variable, set before the store reads it.
    std::env::set_var("LLMSHIM_TEST_S3_WRONG", "not-the-secret");
    let wrong = handle("LLMSHIM_TEST_S3_WRONG", &prefix);
    assert_eq!(
        blobstore::put_sealed(wrong.store.as_ref(), "", "x")
            .await
            .err(),
        Some(BlobError::Denied)
    );
}

fn check(label: &str, media: &str, env: &[(&str, &str)]) -> (Option<i32>, String, tempdir::Dir) {
    let dir = tempdir::Dir::new(label);
    let path = dir.0.join("gateway.toml");
    let text = harness::config("http://127.0.0.1:9/v1", &format!("[media_store]\n{media}"))
        .replace("{DIR}", dir.0.to_str().unwrap());
    std::fs::write(&path, text).unwrap();
    let mut all = vec![
        ("SERVE_E2E_UPSTREAM_KEY", harness::UPSTREAM_KEY),
        ("SERVE_E2E_CALLER_KEY", harness::CALLER_KEY),
    ];
    all.extend_from_slice(env);
    let (code, stderr) = run_with_args(&path, &["--check"], &all);
    (code, stderr, dir)
}

#[test]
fn check_accepts_a_file_store_and_creates_nothing() {
    let (code, stderr, dir) = check(
        "media-file",
        "kind = \"file\"\npath = \"{DIR}/media\"\nttl_secs = 900\n",
        &[],
    );
    assert_eq!(code, Some(0), "{stderr}");
    // `--check` reads and validates; it builds nothing, so the directory is made at start.
    assert!(!dir.0.join("media").exists());
}

#[test]
fn check_names_a_bad_store_by_path_and_an_absent_secret_is_status_3() {
    let s3 = "kind = \"s3\"\nbucket = \"media-bucket\"\nregion = \"us-east-1\"\n\
        endpoint = \"https://minio.invalid:9000\"\n\
        access_key_id = { env = \"MEDIA_KEY_ID\" }\nsecret_access_key = { env = \"MEDIA_SECRET\" }\n";
    // Both variables set: a store that can be built (nothing is contacted by --check).
    let env = [
        ("MEDIA_KEY_ID", "AKIDEXAMPLE"),
        ("MEDIA_SECRET", "hunter2-not-printed"),
    ];
    let (code, stderr, _dir) = check("media-s3-ok", s3, &env);
    assert_eq!(code, Some(0), "{stderr}");
    // A credential that is unset is a fact about the environment: status 3, naming the setting.
    let (code, stderr, _dir) = check("media-s3-unset", s3, &env[..1]);
    assert_eq!(code, Some(3), "{stderr}");
    assert!(stderr.contains("media_store.secret_access_key"), "{stderr}");
    // A wrong file is a defect: status 1, and no secret value is ever printed.
    let (code, stderr, _dir) = check("media-s3-bad", &s3.replace("us-east-1", "US East"), &env);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("media_store.region") && !stderr.contains("hunter2"),
        "{stderr}"
    );
    // The AWS credential chain needs the cloud build: without it a defect naming the flag, with it
    // a store that can be built (nothing is contacted by --check).
    let chain =
        "kind = \"s3\"\nbucket = \"media-bucket\"\nregion = \"us-east-1\"\ncredentials = \"aws\"\n";
    let (code, stderr, _dir) = check("media-s3-chain", chain, &[]);
    if cfg!(feature = "serve-cloud") {
        assert_eq!(code, Some(0), "{stderr}");
    } else {
        assert_eq!(code, Some(1), "{stderr}");
        assert!(
            stderr.contains("serve-cloud") && stderr.contains("media_store.credentials"),
            "{stderr}"
        );
    }
    // A path that cannot be a directory (a file in the way) is refused at start, by path.
    let dir = tempdir::Dir::new("media-file-bad");
    let blocker = dir.0.join("blocker");
    std::fs::write(&blocker, "x").unwrap();
    let config = dir.0.join("gateway.toml");
    std::fs::write(
        &config,
        harness::config(
            "http://127.0.0.1:9/v1",
            &format!(
                "[media_store]\nkind = \"file\"\npath = \"{}/sub\"\n",
                blocker.display()
            ),
        )
        .replace("{DIR}", dir.0.to_str().unwrap()),
    )
    .unwrap();
    let (code, stderr) = run_with_args(
        &config,
        &[],
        &[
            ("SERVE_E2E_UPSTREAM_KEY", harness::UPSTREAM_KEY),
            ("SERVE_E2E_CALLER_KEY", harness::CALLER_KEY),
        ],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("media_store"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_instance_keeps_its_media_in_memory_and_never_touches_the_store() {
    // `[media_store]` is for a fleet's media routes (note 0050): one process has no replica to hand
    // an object to, so the store stays unused even when it is configured.
    let big = "A".repeat(60_000);
    let mut upstream = mockito::Server::new_async().await;
    let images = upstream
        .mock("POST", "/images/generations")
        .with_header("content-type", "application/json")
        .with_body(
            json!({"created": 1, "data": [{"b64_json": big}],
            "usage": {"input_tokens": 5, "output_tokens": 100}})
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let shared = tempdir::Dir::new("media-single-store");
    let text = harness::config(
        &upstream.url(),
        "[admin]\nport = 0\ntoken = { env = \"SERVE_E2E_ADMIN_TOKEN\" }",
    )
    .replace("kind = \"openai-compatible\"", "kind = \"openai\"")
    .replace("providers.inference", "providers.openai")
        + &format!(
            "\n[media_store]\nkind = \"file\"\npath = \"{}\"\n",
            shared.0.display()
        );
    let server = harness::start(&text, "media-single", &[]);
    let answer = reqwest::Client::new()
        .post(format!("{}/v1/images/generations", server.base))
        .bearer_auth(harness::CALLER_KEY)
        .json(&json!({"model": "openai/gpt-image-1", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 200);
    let body: serde_json::Value = answer.json().await.unwrap();
    assert_eq!(
        body["data"][0]["b64_json"].as_str().map(str::len),
        Some(60_000),
        "{}",
        body.to_string().chars().take(300).collect::<String>()
    );
    images.assert_async().await;
    // A reference in a caller's body is a 400 on every backend, not a retriable 503 from a worker.
    let forged = reqwest::Client::new()
        .post(format!("{}/v1/images/generations", server.base))
        .bearer_auth(harness::CALLER_KEY)
        .json(&json!({"model": "openai/gpt-image-1", "prompt": "a cat",
            "extra": {"nested": {"$blob": 1}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 400);
    let mut entries = std::fs::read_dir(&shared.0).unwrap();
    assert!(entries.next().is_none(), "nothing was written to the store");
    let metrics = reqwest::Client::new()
        .get(format!("{}/metrics", server.admin.clone().unwrap()))
        .bearer_auth(harness::ADMIN_TOKEN)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !metrics.contains("llmshim_gateway_media_store_total"),
        "{metrics}"
    );
}

//! `llmshim serve` with the cloud transports configured from the file (0020), in a
//! build with `--features serve-cloud`. Nothing here reaches a cloud: an Azure
//! success path cannot be tested (the transport refuses a non-https endpoint and
//! there is no test trust anchor), and Vertex's token exchange is a fixed Google
//! host, so the process tests cover what is observable offline: each kind starts
//! from a file, the model list, a deployment the file does not name is refused
//! locally, and a Bedrock profile that does not exist fails the request cleanly.
#![cfg(all(feature = "serve-cloud", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{config, start, tempdir, CALLER_KEY};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const KEY_B64: &str = include_str!("../src/serve/jwt_test_rsa_2048.b64");

fn service_account(dir: &std::path::Path) -> std::path::PathBuf {
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        KEY_B64.trim()
    );
    let path = dir.join("sa.json");
    std::fs::write(
        &path,
        json!({"type": "service_account", "client_email": "sa@p.iam.example",
            "private_key": pem, "token_uri": "https://oauth2.googleapis.com/token"})
        .to_string(),
    )
    .unwrap();
    path
}

fn clouds(sa: &std::path::Path) -> String {
    format!(
        "\n[providers.azure]\nkind = \"azure\"\nendpoint = \"https://res.openai.azure.com\"\n\
         binding = \"azure-test\"\napi_key = {{ env = \"SERVE_E2E_AZURE_KEY\" }}\n\
         deployments = {{ \"chat-prod\" = \"gpt-6-sol\", \"chat-fast\" = \"gpt-6-luna\" }}\n\
         [providers.bedrock]\nkind = \"bedrock\"\nprofile = \"no-such-profile\"\nregion = \"us-east-1\"\n\
         [providers.vertex-gemini]\nkind = \"vertex-gemini\"\nproject = \"proj\"\nregion = \"us-central1\"\n\
         binding = \"vertex-test\"\nservice_account_file = \"{sa}\"\n\
         [providers.vertex-anthropic]\nkind = \"vertex-anthropic\"\nproject = \"proj\"\nregion = \"us-central1\"\n\
         binding = \"vertex-test\"\nservice_account_file = \"{sa}\"\n",
        sa = sa.display()
    )
}

const ENV: [(&str, &str); 1] = [("SERVE_E2E_AZURE_KEY", "azure-test-key-0123456789")];

async fn post(base: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_cloud_kind_starts_from_the_file_and_azure_lists_its_deployments() {
    let dir = tempdir::Dir::new("cloud-start");
    let upstream = mockito::Server::new_async().await;
    let sa = service_account(&dir.0);
    let server = start(&config(&upstream.url(), &clouds(&sa)), "cloud-start", &ENV);

    let health: Value = reqwest::get(format!("{}/health", server.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let providers = health["providers"].to_string();
    for name in [
        "azure",
        "bedrock",
        "vertex-gemini",
        "vertex-anthropic",
        "inference",
    ] {
        assert!(providers.contains(name), "{name} not registered: {health}");
    }

    let models: Value = reqwest::Client::new()
        .get(format!("{}/v1/models", server.base))
        .bearer_auth(CALLER_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    // Each configured deployment is requestable as listed; Bedrock and Vertex have
    // no enumerated catalog and list nothing.
    assert!(ids.contains(&"azure/chat-prod"), "{ids:?}");
    assert!(ids.contains(&"azure/chat-fast"), "{ids:?}");
    assert!(ids
        .iter()
        .all(|id| !id.starts_with("bedrock/") && !id.starts_with("vertex-")));
    let own = models["models"].as_array().unwrap();
    assert!(own
        .iter()
        .any(|m| m["id"] == "azure/chat-prod" && m["provider"] == "azure"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deployment_the_file_does_not_name_is_refused_locally() {
    let dir = tempdir::Dir::new("cloud-deployment");
    let upstream = mockito::Server::new_async().await;
    let sa = service_account(&dir.0);
    let server = start(
        &config(&upstream.url(), &clouds(&sa)),
        "cloud-deployment",
        &ENV,
    );
    // Refused before any request leaves the machine (the endpoint is a real
    // Azure host in this file, so reaching it would be visible as a slow answer).
    let started = Instant::now();
    let response = post(&server.base, "azure/never-configured").await;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap();
    // The transport's own local refusal (it carries status 400); the gateway maps a
    // provider-class error to 502, so the code is what identifies it.
    assert!(status >= 400, "{status}: {body}");
    assert!(body.contains("DeploymentNotConfigured"), "{body}");
    assert!(started.elapsed() < Duration::from_secs(5), "{body}");
    assert!(!body.contains("azure-test-key"), "never the key: {body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bedrock_profile_that_does_not_exist_fails_the_request_cleanly() {
    let dir = tempdir::Dir::new("cloud-bedrock");
    let mut upstream = mockito::Server::new_async().await;
    let _ok = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(harness::completion("pong"))
        .create_async()
        .await;
    let sa = service_account(&dir.0);
    let usage = dir.0.join("usage.jsonl");
    let logging = format!("\n[logging]\nusage_log = \"{}\"\n", usage.display());
    let server = start(
        &config(&upstream.url(), &(clouds(&sa) + &logging)),
        "cloud-bedrock",
        &ENV,
    );
    // The log works: an admitted attempt through another provider is recorded.
    assert_eq!(
        post(&server.base, "inference/test-model").await.status(),
        200
    );
    let lines = |path: &std::path::Path| {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while lines(&usage) < 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(lines(&usage), 1);
    let started = Instant::now();
    let response = post(&server.base, "bedrock/anthropic.claude-sonnet-5-5-v1:0").await;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap();
    // The identity cannot be resolved: an error status, promptly (no network is
    // involved before the profile is read), naming the profile and nothing else.
    assert!(status >= 400, "{status}: {body}");
    assert!(started.elapsed() < Duration::from_secs(20), "{body}");
    assert!(
        body.contains("no-such-profile"),
        "names the profile: {body}"
    );
    assert!(
        !body.contains(&dir.0.display().to_string()),
        "no path: {body}"
    );
    // The identity failed before admission, so no attempt exists to record: the
    // usage log, which holds one line per admitted attempt, gains none.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        lines(&usage),
        1,
        "the failed identity added no record: {}",
        std::fs::read_to_string(&usage).unwrap_or_default()
    );
}

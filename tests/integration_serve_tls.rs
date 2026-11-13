//! `llmshim serve` terminating TLS, and picking up a renewed certificate.
#![cfg(feature = "serve")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{completion, config, start, tempdir, CALLER_KEY};
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};

/// A self-signed certificate for 127.0.0.1 with its key, as PEM.
fn certificate() -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    (certificate.pem(), key.serialize_pem())
}

fn write_pair(directory: &Path, certificate: &str, key: &str) {
    std::fs::write(directory.join("tls.crt"), certificate).unwrap();
    std::fs::write(directory.join("tls.key"), key).unwrap();
}

/// A client that trusts exactly one certificate, and starts a fresh connection.
fn client_trusting(certificate: &str) -> reqwest::Client {
    reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_pem(certificate.as_bytes()).unwrap())
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

fn tls_config(upstream: &str, directory: &Path, extra: &str) -> String {
    let tls = format!(
        "[server.tls]\ncert_file = \"{0}/tls.crt\"\nkey_file = \"{0}/tls.key\"\nreload_interval_secs = 1\n",
        directory.display()
    );
    // `[server.tls]` is a sub-table, so it goes before the `[[keys]]` array.
    config(upstream, extra).replace("[state]", &format!("{tls}[state]"))
}

async fn post_chat(
    client: &reqwest::Client,
    base: &str,
) -> Result<reqwest::StatusCode, reqwest::Error> {
    client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"model": "inference/test-model",
            "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .map(|response| response.status())
}

#[tokio::test]
async fn the_server_speaks_https_and_refuses_plaintext() {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(1)
        .create_async()
        .await;
    let certs = tempdir::Dir::new("tls-certs");
    let (certificate, key) = certificate();
    write_pair(&certs.0, &certificate, &key);
    let server = start(&tls_config(&upstream.url(), &certs.0, ""), "tls", &[]);

    assert!(server.base.starts_with("https://"), "{}", server.base);
    let status = post_chat(&client_trusting(&certificate), &server.base)
        .await
        .unwrap();
    assert_eq!(status, 200);
    mock.assert_async().await;

    // Plain HTTP to the TLS port is not served.
    let plain = server.base.replacen("https://", "http://", 1);
    let attempt = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .get(format!("{plain}/health"))
        .send()
        .await;
    assert!(
        attempt
            .map(|response| !response.status().is_success())
            .unwrap_or(true),
        "plaintext must not reach the gateway"
    );
    // A client that does not trust the certificate cannot connect either.
    let untrusting = client_trusting(&self::certificate().0);
    assert!(post_chat(&untrusting, &server.base).await.is_err());
}

#[tokio::test]
async fn a_renewed_certificate_is_served_without_a_restart() {
    let upstream = mockito::Server::new_async().await;
    let certs = tempdir::Dir::new("tls-rotate");
    let (old_certificate, old_key) = certificate();
    write_pair(&certs.0, &old_certificate, &old_key);
    let server = start(&tls_config(&upstream.url(), &certs.0, ""), "rotate", &[]);
    let url = format!("{}/health", server.base);

    let old_client = client_trusting(&old_certificate);
    assert_eq!(old_client.get(&url).send().await.unwrap().status(), 200);

    // Renew: new key pair, same file names.
    let (new_certificate, new_key) = certificate();
    write_pair(&certs.0, &new_certificate, &new_key);

    let new_client = client_trusting(&new_certificate);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = new_client.get(&url).send().await {
            assert_eq!(response.status(), 200);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the renewed certificate was never served"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // From now on the old certificate is no longer presented.
    assert!(client_trusting(&old_certificate)
        .get(&url)
        .send()
        .await
        .is_err());
}

#[tokio::test]
async fn a_replacement_that_does_not_load_keeps_the_previous_certificate() {
    let upstream = mockito::Server::new_async().await;
    let certs = tempdir::Dir::new("tls-bad-rotation");
    let (certificate, key) = certificate();
    write_pair(&certs.0, &certificate, &key);
    let server = start(&tls_config(&upstream.url(), &certs.0, ""), "badrotate", &[]);
    let url = format!("{}/health", server.base);
    let client = client_trusting(&certificate);
    assert_eq!(client.get(&url).send().await.unwrap().status(), 200);

    // A certificate whose key does not match, as when only one file has been
    // replaced so far.
    let (other_certificate, _) = self::certificate();
    std::fs::write(certs.0.join("tls.crt"), other_certificate).unwrap();
    // Give the watcher several intervals to look at it.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        200,
        "the previous certificate must stay in service"
    );

    // Completing the renewal (matching key) then takes effect.
    let (renewed, renewed_key) = self::certificate();
    write_pair(&certs.0, &renewed, &renewed_key);
    let deadline = Instant::now() + Duration::from_secs(15);
    let new_client = client_trusting(&renewed);
    loop {
        if new_client.get(&url).send().await.is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the completed renewal was never served"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[test]
fn check_and_start_reject_a_key_that_does_not_match_the_certificate() {
    let directory = tempdir::Dir::new("tls-mismatch");
    let (certificate, _) = certificate();
    let (_, other_key) = self::certificate();
    write_pair(&directory.0, &certificate, &other_key);
    let path = directory.0.join("gateway.toml");
    let text = tls_config("http://127.0.0.1:1", &directory.0, "")
        .replace("{DIR}", directory.0.to_str().unwrap());
    std::fs::write(&path, text).unwrap();
    for extra in [vec!["--check"], vec![]] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
            .args(["serve", "--config", path.to_str().unwrap()])
            .args(&extra)
            .env_clear()
            .env("LLMSHIM_CATALOG_OFFLINE", "1")
            .env("SERVE_E2E_CALLER_KEY", CALLER_KEY)
            .env("SERVE_E2E_UPSTREAM_KEY", harness::UPSTREAM_KEY)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{extra:?}: {stderr}");
        assert!(
            stderr.contains("server.tls: cannot load the TLS certificate"),
            "{stderr}"
        );
        assert!(!stderr.contains("PRIVATE KEY"), "{stderr}");
        assert!(!stderr.contains("listening on"), "{stderr}");
    }
}

//! `llmshim serve` authenticating workloads by client certificate: a real CA, real
//! client certificates, real handshakes.
#![cfg(feature = "serve")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{completion, config, start, tempdir, CALLER_KEY};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

const CHECKOUT: &str = "spiffe://corp/payments/checkout";
const BATCH: &str = "spiffe://corp/analytics/batch";

fn new_ca() -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
}

/// A client certificate naming `uri`, signed by `ca`: `(certificate, key)` PEM.
fn client_certificate(ca: &CertifiedIssuer<'static, KeyPair>, uri: &str) -> (String, String) {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.subject_alt_names = vec![SanType::URI(uri.try_into().unwrap())];
    let key = KeyPair::generate().unwrap();
    let certificate = params.signed_by(&key, ca).unwrap();
    (certificate.pem(), key.serialize_pem())
}

fn server_certificate() -> (String, String) {
    let key = KeyPair::generate().unwrap();
    let certificate = CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    (certificate.pem(), key.serialize_pem())
}

struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
    server_certificate: String,
}

impl Pki {
    /// Writes the server pair and the CA bundle into `directory`.
    fn install(directory: &Path) -> Self {
        let ca = new_ca();
        let (certificate, key) = server_certificate();
        std::fs::write(directory.join("tls.crt"), &certificate).unwrap();
        std::fs::write(directory.join("tls.key"), key).unwrap();
        std::fs::write(directory.join("ca.crt"), ca.pem()).unwrap();
        Self {
            ca,
            server_certificate: certificate,
        }
    }

    /// A client that trusts the server and presents `identity` (certificate and
    /// key PEM), or none.
    fn client(&self, identity: Option<(String, String)>) -> reqwest::Client {
        let mut builder = reqwest::Client::builder()
            .tls_built_in_root_certs(false)
            .add_root_certificate(
                reqwest::Certificate::from_pem(self.server_certificate.as_bytes()).unwrap(),
            )
            .pool_max_idle_per_host(0)
            .timeout(Duration::from_secs(5));
        if let Some((certificate, key)) = identity {
            let pem = format!("{certificate}\n{key}");
            builder = builder.identity(reqwest::Identity::from_pem(pem.as_bytes()).unwrap());
        }
        builder.build().unwrap()
    }
}

/// The harness configuration with TLS that requires client certificates, two
/// workloads that authenticate by certificate, and an access log.
fn mtls_config(upstream: &str, directory: &Path) -> String {
    let tls = format!(
        "[server.tls]\ncert_file = \"{0}/tls.crt\"\nkey_file = \"{0}/tls.key\"\nclient_ca_file = \"{0}/ca.crt\"\nreload_interval_secs = 1\n",
        directory.display()
    );
    let extra = format!(
        r#"
        [logging]
        access_log = "{log}"

        [[keys]]
        name = "checkout"
        tenant = "payments"
        client_san = "uri:{CHECKOUT}"

        [[keys]]
        name = "batch"
        tenant = "analytics"
        client_san = "uri:{BATCH}"
        "#,
        log = directory.join("access.jsonl").display()
    );
    config(upstream, &extra).replace("[state]", &format!("{tls}[state]"))
}

async fn chat_as(
    client: &reqwest::Client,
    base: &str,
    bearer: Option<&str>,
    spoof: Option<&str>,
) -> Result<reqwest::StatusCode, reqwest::Error> {
    let mut request = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "inference/test-model",
            "messages": [{"role": "user", "content": "ping"}]}));
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    if let Some(name) = spoof {
        request = request.header("x-llmshim-peer-san", name);
    }
    request.send().await.map(|response| response.status())
}

fn tenants(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["tenant"].clone())
        .collect()
}

#[tokio::test]
async fn a_workload_is_identified_by_its_certificate_alone() {
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(2)
        .create_async()
        .await;
    let directory = tempdir::Dir::new("mtls-ok");
    let pki = Pki::install(&directory.0);
    let server = start(&mtls_config(&upstream.url(), &directory.0), "mtls-ok", &[]);

    let checkout = pki.client(Some(client_certificate(&pki.ca, CHECKOUT)));
    let batch = pki.client(Some(client_certificate(&pki.ca, BATCH)));
    assert_eq!(
        chat_as(&checkout, &server.base, None, None).await.unwrap(),
        200
    );
    assert_eq!(
        chat_as(&batch, &server.base, None, None).await.unwrap(),
        200
    );

    // The access log, which resolves callers with the key store the gateway uses,
    // attributes each request to the workload's tenant.
    assert_eq!(
        tenants(&directory.0.join("access.jsonl")),
        vec![json!("payments"), json!("analytics")]
    );
}

#[tokio::test]
async fn connections_without_an_accepted_certificate_never_get_a_response() {
    let upstream = mockito::Server::new_async().await;
    let directory = tempdir::Dir::new("mtls-refused");
    let pki = Pki::install(&directory.0);
    let server = start(
        &mtls_config(&upstream.url(), &directory.0),
        "mtls-refused",
        &[],
    );

    // No certificate at all, a certificate from an authority the server does not
    // trust, and a self-signed certificate: the handshake fails for each.
    let stranger = new_ca();
    let self_signed = {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.subject_alt_names = vec![SanType::URI(CHECKOUT.try_into().unwrap())];
        (params.self_signed(&key).unwrap().pem(), key.serialize_pem())
    };
    for (label, identity) in [
        ("no certificate", None),
        (
            "an untrusted authority",
            Some(client_certificate(&stranger, CHECKOUT)),
        ),
        ("a self-signed certificate", Some(self_signed)),
    ] {
        let attempt = chat_as(&pki.client(identity), &server.base, Some(CALLER_KEY), None).await;
        assert!(attempt.is_err(), "{label} must not complete a request");
    }
    // Even a valid bearer token does not substitute for the certificate.
    assert!(tenants(&directory.0.join("access.jsonl")).is_empty());
}

#[tokio::test]
async fn an_unknown_workload_and_a_forged_header_both_fail_closed() {
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(1)
        .create_async()
        .await;
    let directory = tempdir::Dir::new("mtls-spoof");
    let pki = Pki::install(&directory.0);
    let server = start(
        &mtls_config(&upstream.url(), &directory.0),
        "mtls-spoof",
        &[],
    );

    // A certificate the CA signed, naming a workload nobody configured: the
    // handshake succeeds and the gateway refuses to identify it.
    let unknown = pki.client(Some(client_certificate(
        &pki.ca,
        "spiffe://corp/other/service",
    )));
    assert_eq!(
        chat_as(&unknown, &server.base, None, None).await.unwrap(),
        401
    );

    // The same unknown workload claims, in the internal header, to be the
    // checkout service. The server removes that header from every request.
    let forged = Some(format!("uri:{CHECKOUT}"));
    assert_eq!(
        chat_as(&unknown, &server.base, None, forged.as_deref())
            .await
            .unwrap(),
        401,
        "a caller must not be able to name its own identity"
    );

    // A genuine checkout certificate claiming to be the analytics batch job stays
    // checkout: the header is replaced from the handshake, not trusted.
    let checkout = pki.client(Some(client_certificate(&pki.ca, CHECKOUT)));
    let claiming_batch = Some(format!("uri:{BATCH}"));
    assert_eq!(
        chat_as(&checkout, &server.base, None, claiming_batch.as_deref())
            .await
            .unwrap(),
        200
    );
    assert_eq!(
        tenants(&directory.0.join("access.jsonl")).last(),
        Some(&json!("payments"))
    );
}

#[tokio::test]
async fn a_bearer_token_decides_alone_and_a_certificate_never_rescues_a_wrong_one() {
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(1)
        .create_async()
        .await;
    let directory = tempdir::Dir::new("mtls-bearer");
    let pki = Pki::install(&directory.0);
    let server = start(
        &mtls_config(&upstream.url(), &directory.0),
        "mtls-bearer",
        &[],
    );
    let checkout = pki.client(Some(client_certificate(&pki.ca, CHECKOUT)));

    // A valid certificate with a wrong token is refused: the token decides.
    assert_eq!(
        chat_as(
            &checkout,
            &server.base,
            Some("a-wrong-token-0123456789"),
            None
        )
        .await
        .unwrap(),
        401
    );
    // A valid certificate with the platform key is the platform tenant.
    assert_eq!(
        chat_as(&checkout, &server.base, Some(CALLER_KEY), None)
            .await
            .unwrap(),
        200
    );
    assert_eq!(
        tenants(&directory.0.join("access.jsonl")),
        vec![json!(null), json!("platform")]
    );
}

#[test]
fn a_certificate_key_needs_client_certificates_to_be_required() {
    let directory = tempdir::Dir::new("mtls-config");
    let path = directory.0.join("gateway.toml");
    let text = config(
        "http://127.0.0.1:1",
        &format!("[[keys]]\nname = \"w\"\ntenant = \"t\"\nclient_san = \"uri:{CHECKOUT}\""),
    )
    .replace("{DIR}", directory.0.to_str().unwrap());
    std::fs::write(&path, text).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["serve", "--config", path.to_str().unwrap(), "--check"])
        .env_clear()
        .env("SERVE_E2E_CALLER_KEY", CALLER_KEY)
        .env("SERVE_E2E_UPSTREAM_KEY", harness::UPSTREAM_KEY)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("client_san: needs server.tls.client_ca_file"),
        "{stderr}"
    );
}

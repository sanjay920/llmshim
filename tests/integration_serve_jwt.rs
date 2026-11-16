//! `llmshim serve` authenticating workload tokens (0017): a real server process,
//! a local issuer serving a JWKS, a mock upstream.
#![cfg(all(feature = "serve", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use harness::{completion, config, start, tempdir, ADMIN_TOKEN, CALLER_KEY};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const ISSUER: &str = "https://idp.example";
const AUDIENCE: &str = "llmshim";
const SUBJECT: &str = "system:serviceaccount:jobs:runner";

struct Issuer {
    pair: Ed25519KeyPair,
    /// What the JWKS endpoint answers: a status and a body.
    answer: Arc<Mutex<(u16, String)>>,
    url: String,
}

impl Issuer {
    async fn start() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let jwks = json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "kid": "k1",
            "x": URL_SAFE_NO_PAD.encode(pair.public_key().as_ref())}]});
        let answer = Arc::new(Mutex::new((200u16, jwks.to_string())));
        let served = answer.clone();
        let app = axum::Router::new().route(
            "/keys",
            axum::routing::get(move || {
                let (status, body) = served.lock().unwrap().clone();
                async move {
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        [("content-type", "application/json")],
                        body,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/keys", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { pair, answer, url }
    }

    fn token(&self, claims: Value) -> String {
        self.token_with_kid("k1", claims)
    }

    fn token_with_kid(&self, kid: &str, claims: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(json!({"alg": "EdDSA", "kid": kid}).to_string());
        let body = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signed = format!("{header}.{body}");
        let signature = URL_SAFE_NO_PAD.encode(self.pair.sign(signed.as_bytes()).as_ref());
        format!("{signed}.{signature}")
    }

    fn claims(&self, sub: &str) -> Value {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        json!({"iss": ISSUER, "aud": AUDIENCE, "sub": sub, "exp": now + 600, "iat": now})
    }

    fn good(&self) -> String {
        self.token(self.claims(SUBJECT))
    }
}

fn render(upstream: &str, jwks_url: &str, key_extra: &str, log: &std::path::Path) -> String {
    config(
        upstream,
        &format!(
            "\n[[keys]]\nname = \"runner\"\ntenant = \"jobs\"\n\
             jwt = {{ table = \"idp\", subject = \"{SUBJECT}\" }}\n{key_extra}\n\
             [[auth.jwt]]\nname = \"idp\"\nissuer = \"{ISSUER}\"\njwks_url = \"{jwks_url}\"\n\
             audience = \"{AUDIENCE}\"\nalgorithms = [\"EdDSA\"]\n\
             [logging]\naccess_log = \"{}\"\n\
             [admin]\nport = 0\ntoken = {{ env = \"SERVE_E2E_ADMIN_TOKEN\" }}\n",
            log.display()
        ),
    )
}

fn env() -> [(&'static str, &'static str); 1] {
    [("SERVE_E2E_ADMIN_TOKEN", ADMIN_TOKEN)]
}

async fn call(base: &str, path: &str, headers: &[(&str, String)]) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .json(&json!({"model": "inference/test-model",
            "messages": [{"role": "user", "content": "ping"}]}));
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    request.send().await.unwrap()
}

async fn chat(base: &str, token: &str) -> reqwest::Response {
    call(
        base,
        "/v1/chat/completions",
        &[("authorization", format!("Bearer {token}"))],
    )
    .await
}

async fn lines(path: &std::path::Path, expected: usize) -> Vec<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let found: Vec<Value> = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if found.len() >= expected || std::time::Instant::now() > deadline {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            return std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn upstream(calls: usize) -> (mockito::ServerGuard, mockito::Mock) {
    let mut upstream = mockito::Server::new_async().await;
    let mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(calls)
        .create_async()
        .await;
    (upstream, mock)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verified_token_is_the_key_it_is_bound_to() {
    let issuer = Issuer::start().await;
    let (up, mock) = upstream(2).await;
    let dir = tempdir::Dir::new("jwt-ok");
    let log = dir.0.join("access.jsonl");
    let server = start(&render(&up.url(), &issuer.url, "", &log), "jwt-ok", &env());

    let token = issuer.good();
    let response = chat(&server.base, &token).await;
    assert_eq!(response.status(), 200);
    // The native routes read the credential from `x-api-key` too; a token there
    // is verified the same way.
    let native = call(
        &server.base,
        "/v1/chat/completions",
        &[("x-api-key", token.clone())],
    )
    .await;
    assert_eq!(native.status(), 200);
    mock.assert_async().await;

    let found = lines(&log, 2).await;
    assert_eq!(found.len(), 2, "{found:?}");
    for line in &found {
        assert_eq!(line["key"], "runner");
        assert_eq!(line["tenant"], "jobs");
        assert!(line["auth_reason"].is_null());
        // The log carries a fingerprint, never the token.
        assert_eq!(line["credential"].as_str().unwrap().len(), 8);
        assert!(!line.to_string().contains(&token));
    }

    // The issuer's key state is on the admin listener.
    let status = harness::reload_status(&server).await;
    assert_eq!(status["jwks"]["idp"]["loaded"], true);
    assert_eq!(status["jwks"]["idp"]["keys"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokens_that_do_not_verify_are_refused_before_the_upstream_with_a_reason() {
    let issuer = Issuer::start().await;
    let (up, mock) = upstream(0).await;
    let dir = tempdir::Dir::new("jwt-refused");
    let log = dir.0.join("access.jsonl");
    let server = start(
        &render(&up.url(), &issuer.url, "", &log),
        "jwt-refused",
        &env(),
    );

    let other = Issuer::start().await;
    let mut wrong_audience = issuer.claims(SUBJECT);
    wrong_audience["aud"] = json!("someone-else");
    let mut wrong_issuer = issuer.claims(SUBJECT);
    wrong_issuer["iss"] = json!("https://other.example");
    let mut expired = issuer.claims(SUBJECT);
    expired["exp"] = json!(1_000_000);
    let cases: Vec<(&str, String, &str)> = vec![
        ("audience", issuer.token(wrong_audience), "jwt_invalid"),
        ("issuer", issuer.token(wrong_issuer), "jwt_invalid"),
        ("expired", issuer.token(expired), "jwt_invalid"),
        (
            "signature",
            other.token(issuer.claims(SUBJECT)),
            "jwt_invalid",
        ),
        (
            "kid",
            issuer.token_with_kid("not-published", issuer.claims(SUBJECT)),
            "jwt_unknown_key",
        ),
        (
            "subject",
            issuer.token(issuer.claims("system:serviceaccount:other:thing")),
            "jwt_unknown_subject",
        ),
    ];
    for (name, token, _) in &cases {
        let response = chat(&server.base, token).await;
        assert_eq!(response.status(), 401, "{name}");
        let body = response.text().await.unwrap();
        assert!(!body.contains(token.as_str()), "{name}");
    }
    mock.assert_async().await;

    let found = lines(&log, cases.len()).await;
    assert_eq!(found.len(), cases.len(), "{found:?}");
    for ((name, _, reason), line) in cases.iter().zip(&found) {
        assert_eq!(line["status"], 401, "{name}");
        assert_eq!(line["auth_reason"], *reason, "{name}");
        assert!(line["key"].is_null() && line["tenant"].is_null(), "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_issuer_fails_its_tokens_only_and_never_a_bearer_key() {
    let issuer = Issuer::start().await;
    *issuer.answer.lock().unwrap() = (500, "down".into());
    let (up, mock) = upstream(1).await;
    let dir = tempdir::Dir::new("jwt-down");
    let log = dir.0.join("access.jsonl");
    let server = start(
        &render(&up.url(), &issuer.url, "", &log),
        "jwt-down",
        &env(),
    );

    let refused = chat(&server.base, &issuer.good()).await;
    assert_eq!(refused.status(), 503);
    assert!(refused.headers().contains_key("retry-after"));
    // An ordinary key is unaffected, and so are the probes.
    assert_eq!(chat(&server.base, CALLER_KEY).await.status(), 200);
    let ready = reqwest::get(format!("{}/ready", server.admin.clone().unwrap()))
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);
    mock.assert_async().await;

    let found = lines(&log, 2).await;
    assert!(found.iter().any(|l| l["auth_reason"] == "jwks_unavailable"));
    let status = harness::reload_status(&server).await;
    assert_eq!(status["jwks"]["idp"]["loaded"], false);
    assert_eq!(status["jwks"]["idp"]["last_error"], "status");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_keys_keep_verifying_while_the_issuer_goes_away() {
    let issuer = Issuer::start().await;
    let (up, mock) = upstream(2).await;
    let dir = tempdir::Dir::new("jwt-away");
    let log = dir.0.join("access.jsonl");
    let server = start(
        &render(&up.url(), &issuer.url, "", &log),
        "jwt-away",
        &env(),
    );
    assert_eq!(chat(&server.base, &issuer.good()).await.status(), 200);
    *issuer.answer.lock().unwrap() = (500, "down".into());
    assert_eq!(chat(&server.base, &issuer.good()).await.status(), 200);
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_is_bound_to_its_keys_network_like_any_credential() {
    let issuer = Issuer::start().await;
    let (up, mock) = upstream(0).await;
    let dir = tempdir::Dir::new("jwt-net");
    let log = dir.0.join("access.jsonl");
    let server = start(
        &render(
            &up.url(),
            &issuer.url,
            "allowed_cidrs = [\"10.0.0.0/8\"]",
            &log,
        ),
        "jwt-net",
        &env(),
    );
    assert_eq!(chat(&server.base, &issuer.good()).await.status(), 403);
    mock.assert_async().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_cannot_borrow_the_certificate_or_peer_header_of_another_caller() {
    // The internal header is the key store's channel for workload names; a caller
    // that sends it directly, with or without a token, gets nothing from it.
    let issuer = Issuer::start().await;
    let (up, mock) = upstream(0).await;
    let dir = tempdir::Dir::new("jwt-forge");
    let log = dir.0.join("access.jsonl");
    let server = start(
        &render(&up.url(), &issuer.url, "", &log),
        "jwt-forge",
        &env(),
    );
    let forged = format!("jwt:idp:{SUBJECT}");
    let response = call(
        &server.base,
        "/v1/chat/completions",
        &[("x-llmshim-peer-san", forged.clone())],
    )
    .await;
    assert_eq!(response.status(), 401);
    let with_junk = call(
        &server.base,
        "/v1/chat/completions",
        &[
            ("x-llmshim-peer-san", forged),
            ("authorization", "Bearer not-a-known-key-0123456789".into()),
        ],
    )
    .await;
    assert_eq!(with_junk.status(), 401);
    mock.assert_async().await;
}

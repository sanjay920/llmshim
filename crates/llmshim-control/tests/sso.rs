//! Single sign-on for the control plane (note 0049), against a mock issuer that signs real
//! Ed25519 tokens: each role gets exactly its routes, the actor is the verified subject and never
//! what the caller typed, break-glass is an admin that can be turned off, and a token that does
//! not verify is refused and never retried as the shared token.

mod common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use http_body_util::BodyExt;
use llmshim::serve::jwks::Fetch;
use llmshim_control::api::{router, AppState, INGEST_ROUTES, ROLES, ROUTES};
use llmshim_control::sso::{Role, Sso, SsoConfig};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

const SHARED: &str = "break-glass-token-0123456789";
const ISSUER: &str = "https://idp.example.test";
const AUDIENCE: &str = "llmshim-control";

struct Issuer {
    pair: Ed25519KeyPair,
}

impl Issuer {
    fn new() -> Self {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        Self {
            pair: Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap(),
        }
    }

    fn jwks(&self) -> Vec<u8> {
        json!({"keys": [{"kty": "OKP", "crv": "Ed25519", "kid": "k1",
            "x": URL_SAFE_NO_PAD.encode(self.pair.public_key().as_ref())}]})
        .to_string()
        .into_bytes()
    }

    fn sign(&self, header: Value, claims: Value) -> String {
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = self.pair.sign(signed.as_bytes());
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }

    fn token(&self, subject: &str, roles: Value) -> String {
        self.sign(
            json!({"alg": "EdDSA", "kid": "k1", "typ": "JWT"}),
            claims(subject, roles),
        )
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn claims(subject: &str, roles: Value) -> Value {
    json!({"iss": ISSUER, "aud": AUDIENCE, "sub": subject, "roles": roles,
        "exp": now() + 600, "iat": now()})
}

struct Keys {
    document: Vec<u8>,
    up: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Fetch for Keys {
    async fn fetch(&self) -> Result<Vec<u8>, &'static str> {
        if self.up.load(Ordering::SeqCst) {
            Ok(self.document.clone())
        } else {
            Err("network")
        }
    }
}

fn config(break_glass: bool) -> SsoConfig {
    serde_json::from_value(json!({
        "issuer": ISSUER, "audience": AUDIENCE,
        "jwks_url": "https://idp.example.test/jwks.json",
        "algorithms": ["EdDSA"], "break_glass": break_glass,
    }))
    .unwrap()
}

struct Api {
    app: axum::Router,
    issuer: Issuer,
    up: Arc<AtomicBool>,
}

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
}

impl Api {
    fn new(break_glass: bool) -> Self {
        Self::with_shared(break_glass, SHARED)
    }

    fn with_shared(break_glass: bool, shared: &str) -> Self {
        let issuer = Issuer::new();
        let up = Arc::new(AtomicBool::new(true));
        let sso = Sso::with_fetch(
            &config(break_glass),
            Arc::new(Keys {
                document: issuer.jwks(),
                up: up.clone(),
            }),
        )
        .unwrap();
        let state = AppState::with_sso(common::store(), Some(shared), None, sso);
        Self {
            app: router(state),
            issuer,
            up,
        }
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        extra: &[(&str, &str)],
        body: Option<Value>,
    ) -> Answer {
        let mut request = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(path);
        if let Some(bearer) = bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        Answer {
            status: parts.status,
            headers: parts.headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn get(&self, path: &str, bearer: &str) -> Answer {
        self.call("GET", path, Some(bearer), &[], None).await
    }
}

fn concrete(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|part| if part.starts_with('{') { "x" } else { part })
        .collect::<Vec<_>>()
        .join("/")
}

#[tokio::test]
async fn every_route_has_a_required_role_and_only_routes_have_one() {
    for (method, path) in ROUTES {
        let role = ROLES.iter().any(|(m, p, _)| m == method && p == path);
        let ingest = INGEST_ROUTES.contains(&(*method, *path));
        // A route is a role's or the ingest token's, never both and never neither.
        assert!(
            role != ingest,
            "{method} {path}: role={role} ingest={ingest}"
        );
    }
    for (method, path) in INGEST_ROUTES {
        assert!(
            ROUTES.contains(&(*method, *path)),
            "{method} {path} is not a route"
        );
    }
    for (method, path, _) in ROLES {
        assert!(
            ROUTES.iter().any(|(m, p)| m == method && p == path),
            "{method} {path} is not a route"
        );
    }
}

#[tokio::test]
async fn each_role_gets_exactly_its_routes() {
    let api = Api::new(true);
    // Every GET is a read route except the history chain check, which scans the whole history: admin only.
    let read_routes: Vec<_> = ROUTES
        .iter()
        .filter(|(m, p)| *m == "get" && *p != "/v1/history/verify")
        .collect();
    let key_routes = [
        ("post", "/v1/projects/{project}/keys"),
        ("post", "/v1/keys/{key}/revoke"),
        ("post", "/v1/keys/{key}/rotate"),
        ("patch", "/v1/keys/{key}/limits"),
    ];
    for (role, value) in [
        (Role::Read, "control:read"),
        (Role::KeyIssuer, "control:key-issuer"),
        (Role::Admin, "control:admin"),
    ] {
        let token = api.issuer.token("alice", json!([value]));
        for (method, pattern) in ROUTES.iter().filter(|r| !INGEST_ROUTES.contains(*r)) {
            let allowed = if read_routes
                .iter()
                .any(|r| r.1 == *pattern && r.0 == *method)
            {
                true
            } else if key_routes.contains(&(*method, *pattern)) {
                role >= Role::KeyIssuer
            } else {
                role == Role::Admin
            };
            let answer = api
                .call(
                    &method.to_uppercase(),
                    &concrete(pattern),
                    Some(&token),
                    &[],
                    (*method != "get" && *method != "delete").then(|| json!({})),
                )
                .await;
            let allowed = allowed || *pattern == "/v1/whoami";
            assert_eq!(
                answer.status == StatusCode::FORBIDDEN,
                !allowed,
                "{} {method} {pattern}: {}",
                role.name(),
                answer.status
            );
            assert_ne!(
                answer.status,
                StatusCode::UNAUTHORIZED,
                "{method} {pattern}"
            );
        }
    }
}

#[tokio::test]
async fn a_token_with_no_role_of_ours_is_authenticated_and_refused_everything_but_whoami() {
    let api = Api::new(true);
    for roles in [
        json!([]),
        json!(["admin", "other:control:admin"]),
        json!(null),
    ] {
        let token = api.issuer.token("alice", roles.clone());
        for (method, pattern) in ROUTES.iter().filter(|r| !INGEST_ROUTES.contains(*r)) {
            let answer = api
                .call(
                    &method.to_uppercase(),
                    &concrete(pattern),
                    Some(&token),
                    &[],
                    None,
                )
                .await;
            if *pattern == "/v1/whoami" {
                // The one route any authenticated caller may use: it says the role is none.
                assert_eq!(answer.status, StatusCode::OK, "{roles}");
                assert_eq!(answer.body["role"], Value::Null);
                continue;
            }
            assert_eq!(
                answer.status,
                StatusCode::FORBIDDEN,
                "{roles} {method} {pattern}"
            );
            assert_eq!(answer.body["error"]["code"], "forbidden");
        }
    }
    // The message names the role needed, never the roles held.
    let token = api.issuer.token("alice", json!(["control:read"]));
    let denied = api
        .call(
            "POST",
            "/v1/orgs",
            Some(&token),
            &[],
            Some(json!({"name": "A"})),
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert!(denied.body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("admin"));
}

#[tokio::test]
async fn the_actor_is_the_verified_subject_and_never_what_the_caller_typed() {
    let api = Api::new(true);
    let token = api
        .issuer
        .token("alice@example.test", json!("control:admin"));
    let made = api
        .call(
            "POST",
            "/v1/orgs",
            Some(&token),
            &[("x-llmshim-actor", "mallory")],
            Some(json!({"name": "Acme"})),
        )
        .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    let history = api.get("/v1/history", &token).await;
    let row = &history.body["history"][0];
    assert_eq!(row["action"], "org.created");
    assert_eq!(row["actor"], "oidc:alice@example.test");
    assert!(!history.body.to_string().contains("mallory"));
    // An unusual subject is a digest, not raw text.
    let odd = api.issuer.token("has space", json!(["control:admin"]));
    let who = api.get("/v1/whoami", &odd).await;
    assert!(who.body["actor"]
        .as_str()
        .unwrap()
        .starts_with("oidc-hash:"));
}

#[tokio::test]
async fn a_head_needs_what_the_get_needs() {
    let api = Api::new(true);
    let read = api.issuer.token("r", json!(["control:read"]));
    let none = api.issuer.token("n", json!([]));
    let head = |token: String| {
        let api = &api;
        async move {
            api.call("HEAD", "/v1/orgs", Some(&token), &[], None)
                .await
                .status
        }
    };
    assert_ne!(head(read).await, StatusCode::FORBIDDEN);
    assert_eq!(head(none).await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn whoami_reports_the_identity_and_role() {
    let api = Api::new(true);
    let token = api.issuer.token("bob", json!(["control:key-issuer"]));
    let who = api.get("/v1/whoami", &token).await;
    assert_eq!(
        who.body,
        json!({"actor": "oidc:bob", "role": "key-issuer", "via": "oidc"})
    );
    let shared = api.get("/v1/whoami", SHARED).await;
    assert_eq!(
        shared.body,
        json!({"actor": "break-glass", "role": "admin", "via": "break_glass"})
    );
}

#[tokio::test]
async fn break_glass_is_an_admin_that_ignores_the_actor_header_and_can_be_turned_off() {
    let api = Api::new(true);
    let made = api
        .call(
            "POST",
            "/v1/orgs",
            Some(SHARED),
            &[("x-llmshim-actor", "mallory")],
            Some(json!({"name": "Acme"})),
        )
        .await;
    assert_eq!(made.status, StatusCode::CREATED);
    let history = api.get("/v1/history", SHARED).await;
    assert_eq!(history.body["history"][0]["actor"], "break-glass");

    let off = Api::new(false);
    let refused = off.get("/v1/orgs", SHARED).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    // A token still works.
    let token = off.issuer.token("alice", json!(["control:read"]));
    assert_eq!(off.get("/v1/orgs", &token).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_token_that_does_not_verify_is_refused_and_never_tried_as_the_shared_token() {
    let api = Api::new(true);
    let other = Issuer::new();
    let good = |extra: Value| {
        let mut c = claims("alice", json!(["control:admin"]));
        c.as_object_mut()
            .unwrap()
            .extend(extra.as_object().cloned().unwrap());
        c
    };
    let eddsa = json!({"alg": "EdDSA", "kid": "k1"});
    let bad = [
        // Signed by a key the issuer does not publish.
        other.token("alice", json!(["control:admin"])),
        api.issuer
            .sign(eddsa.clone(), good(json!({"aud": "someone-else"}))),
        api.issuer
            .sign(eddsa.clone(), good(json!({"iss": "https://evil.example"}))),
        api.issuer
            .sign(eddsa.clone(), good(json!({"exp": now() - 600}))),
        api.issuer
            .sign(eddsa.clone(), good(json!({"exp": now() + 400_000}))),
        api.issuer.sign(json!({"alg": "none"}), good(json!({}))),
        api.issuer
            .sign(json!({"alg": "HS256", "kid": "k1"}), good(json!({}))),
        api.issuer.sign(eddsa.clone(), good(json!({"sub": ""}))),
    ];
    for token in &bad {
        let answer = api.get("/v1/orgs", token).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{token}");
        assert_eq!(
            answer.headers.get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer"
        );
    }
    // A shared token that is itself shaped like a token is verified as one and refused: a
    // JWT-shaped value is never compared with the shared token.
    let shaped = format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"EdDSA"}"#),
        URL_SAFE_NO_PAD.encode(r#"{"iss":"x"}"#),
        "sig-0123456789"
    );
    let api = Api::with_shared(true, &shaped);
    assert_eq!(
        api.get("/v1/orgs", &shaped).await.status,
        StatusCode::UNAUTHORIZED
    );
    // No credential at all.
    assert_eq!(
        api.call("GET", "/v1/orgs", None, &[], None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn keys_that_cannot_be_fetched_are_a_503_never_a_pass() {
    let api = Api::new(true);
    api.up.store(false, Ordering::SeqCst);
    let token = api.issuer.token("alice", json!(["control:admin"]));
    let answer = api.get("/v1/orgs", &token).await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.headers.get(header::RETRY_AFTER).unwrap(), "30");
    assert_eq!(answer.body["error"]["code"], "unavailable");
    // The shared token still works while the identity provider is away.
    assert_eq!(api.get("/v1/orgs", SHARED).await.status, StatusCode::OK);
}

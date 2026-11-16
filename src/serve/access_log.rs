//! The access log: one JSON line per request on the main listener.
//!
//! It is a middleware around the whole application, so it needs nothing from
//! the gateway's internals and sees every outcome, including requests refused
//! before they reach a handler. A line holds:
//!
//! - `ts`: RFC 3339 time the response headers were ready, UTC;
//! - `request_id`: the request's id, settled here (outermost) and put on the
//!   response, so every line has one;
//! - `method`, `path`: the request line without its query string;
//! - `status`: the HTTP status sent in the response headers, `null` when the
//!   client went away (or the request was cancelled) before a response existed.
//!   A stream that fails after its headers is still logged as the status it
//!   began with; the usage log carries the outcome of the attempt;
//! - `latency_ms`: until the response headers, so a stream's duration is not
//!   included;
//! - `tenant`: the caller's tenant, `null` when the credential was missing or
//!   not recognised. The credential is read as the gateway reads it, so on the
//!   native routes an `x-api-key` or `x-goog-api-key` counts;
//! - `key`: the name (`keys[].name`) of the caller key the credential belongs
//!   to, `null` when it was missing or not recognised;
//! - `credential`: the first eight hex characters of the SHA-256 of the bearer
//!   token, `null` without one. It tells repeated use of one credential apart
//!   without revealing it, including a wrong one being tried again and again.
//!
//! `path` comes from the caller, so it is cut to 256 characters (a caller's own
//! `request_id` is at most 128 by the rule that settles it). No token, header
//! value, query string, body or model output is ever written.

use super::jsonl::JsonLines;
use crate::gateway::auth::KeyStore;
use axum::extract::Request;
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Serialize, Debug, PartialEq)]
pub(crate) struct Record {
    /// Always `"access"`, so lines can share a stream with other records.
    pub event: &'static str,
    pub ts: String,
    pub request_id: Option<String>,
    pub method: String,
    pub path: String,
    pub status: Option<u16>,
    pub latency_ms: u64,
    pub tenant: Option<String>,
    /// The name (`keys[].name`) of the caller key the credential belongs to.
    /// `null` when the caller sent none or an unknown one.
    pub key: Option<String>,
    pub credential: Option<String>,
    /// Why a token was refused before the gateway saw the request
    /// (`jwt_invalid`, `jwt_unknown_key`, `jwt_unknown_subject`,
    /// `jwks_unavailable`), `null` otherwise. A class, never the token or the
    /// claim that failed.
    pub auth_reason: Option<&'static str>,
}

/// The first eight hex characters of the SHA-256 of the presented bearer token.
fn fingerprint(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?
        .trim();
    (!token.is_empty()).then(|| format!("{:x}", Sha256::digest(token.as_bytes()))[..8].to_string())
}

/// At most this many characters of a caller-controlled value.
const MAX_FIELD_CHARS: usize = 256;

fn bounded(text: &str) -> String {
    text.chars().take(MAX_FIELD_CHARS).collect()
}

/// A request the log owes a line for. The line is written when the response
/// headers are ready, or, if the future is dropped first (the client went away,
/// a deadline cancelled it), when this is dropped, so an abandoned request that
/// may already have been billed upstream is not missing from the audit trail.
struct Pending {
    log: Option<Arc<JsonLines>>,
    span: Option<super::telemetry::RequestSpan>,
    started: std::time::Instant,
    request_id: Option<String>,
    method: String,
    path: String,
    tenant: Option<String>,
    key: Option<String>,
    credential: Option<String>,
    auth_reason: Option<&'static str>,
    written: bool,
}

impl Pending {
    fn write(&mut self, status: Option<u16>) {
        self.written = true;
        if let Some(span) = self.span.take() {
            span.end(status);
        }
        let Some(log) = &self.log else { return };
        log.write(&Record {
            event: "access",
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            request_id: self.request_id.take(),
            method: std::mem::take(&mut self.method),
            path: std::mem::take(&mut self.path),
            status,
            latency_ms: self.started.elapsed().as_millis() as u64,
            tenant: self.tenant.take(),
            key: self.key.take(),
            credential: self.credential.take(),
            auth_reason: self.auth_reason.take(),
        });
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.written {
            self.write(None);
        }
    }
}

pub(crate) async fn record_request(
    log: Option<Arc<JsonLines>>,
    telemetry: Option<Arc<super::telemetry::Telemetry>>,
    keys: Arc<KeyStore>,
    names: crate::gateway::usage::KeyNames,
    jwt: Option<super::jwt_auth::JwtAuth>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    // Settle the request's id here, outermost, so its line carries one whatever
    // becomes of the request: refused by a layer outside the gateway's own,
    // dropped before any response. The gateway's layer keeps this one.
    let id = crate::gateway::http::settle_request_id(request.headers());
    if let Ok(value) = axum::http::HeaderValue::from_str(&id) {
        request.headers_mut().insert("x-request-id", value);
    }
    request
        .extensions_mut()
        .insert(crate::gateway::http::SettledRequestId(id.clone()));
    // A token, if there is one, is verified here, inside the id and the log and
    // before the caller is resolved, so that a refusal is logged with its reason
    // and a verified token resolves to the key it is bound to. The fingerprint is
    // of what was presented, taken before the token is replaced by the workload.
    let presented = {
        let mut original = request.headers().clone();
        if crate::proxy::wire::native_route(&path).is_some() {
            crate::proxy::wire::normalize_auth(&mut original);
        }
        fingerprint(&original)
    };
    let mut refusal = None;
    if let Some(jwt) = &jwt {
        if let Err(failed) = jwt.authenticate(&mut request).await {
            refusal = Some(failed);
        }
    }
    // Resolve the caller before the request is consumed, from the headers as the
    // gateway reads them: on the native routes an `x-api-key` or
    // `x-goog-api-key` is the credential. The same store the gateway
    // authenticates with decides, so the log cannot disagree with it.
    let mut headers = request.headers().clone();
    if crate::proxy::wire::native_route(&path).is_some() {
        crate::proxy::wire::normalize_auth(&mut headers);
    }
    let caller = keys.identify_caller(&headers).ok();
    let tenant = caller.as_ref().map(|caller| caller.identity.tenant.clone());
    let key = caller
        .as_ref()
        .and_then(|caller| names.name(&caller.credential_scope));
    // An inbound traceparent is honored only from a caller the key store
    // recognises (0016, decision 7).
    let span = telemetry.as_ref().map(|telemetry| {
        telemetry.request_span(
            request.method().as_str(),
            &bounded(&path),
            &id,
            &super::telemetry::SpanCaller {
                tenant: tenant.clone(),
                key: key.clone(),
            },
            caller.is_some().then_some(&headers),
        )
    });
    let mut pending = Pending {
        log,
        span,
        started: std::time::Instant::now(),
        request_id: Some(id.clone()),
        method: request.method().to_string(),
        path: bounded(&path),
        tenant,
        key,
        credential: presented,
        auth_reason: refusal.as_ref().map(|failed| failed.reason),
        written: false,
    };
    let mut response = match refusal {
        Some(failed) => failed.into_response(),
        None => next.run(request).await,
    };
    // A response made by a layer outside the gateway's own (the origin check, the
    // hidden admin path) carries no id yet; the caller gets the one the log has.
    if !response.headers().contains_key("x-request-id") {
        if let Ok(value) = axum::http::HeaderValue::from_str(&id) {
            response.headers_mut().insert("x-request-id", value);
        }
    }
    pending.write(Some(response.status().as_u16()));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::io::Write;
    use std::sync::Mutex;

    #[test]
    fn the_fingerprint_is_a_digest_prefix_never_the_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer sk-secret-token-value"),
        );
        let print = fingerprint(&headers).unwrap();
        assert_eq!(print.len(), 8);
        assert!(print.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!print.contains("secret"));
        // A known SHA-256 vector (FIPS 180-2: "abc"), not the implementation's
        // own formula recomputed.
        let mut abc = HeaderMap::new();
        abc.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(fingerprint(&abc).as_deref(), Some("ba7816bf"));
        // Without a bearer token there is nothing to fingerprint.
        for value in ["Basic abc", "Bearer ", "bearer"] {
            let mut other = HeaderMap::new();
            other.insert(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_str(value).unwrap(),
            );
            assert_eq!(fingerprint(&other), None, "{value}");
        }
        assert_eq!(fingerprint(&HeaderMap::new()), None);
    }

    #[test]
    fn a_record_is_one_line_of_json_with_null_for_the_unknown() {
        let record = Record {
            event: "access",
            ts: "2026-11-13T00:00:00.000Z".into(),
            request_id: None,
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            status: Some(401),
            latency_ms: 3,
            tenant: None,
            key: None,
            credential: Some("0a1b2c3d".into()),
            auth_reason: None,
        };
        let line = serde_json::to_string(&record).unwrap();
        assert!(!line.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(value["tenant"].is_null() && value["request_id"].is_null());
        assert!(value["key"].is_null());
        assert_eq!(value["status"], 401);
        assert_eq!(value["credential"], "0a1b2c3d");
        assert!(value["auth_reason"].is_null());
    }

    /// A writer the test can read back.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn lines(&self) -> Vec<serde_json::Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    const TOKEN: &str = "caller-token-0123456789";

    fn store() -> Arc<KeyStore> {
        let digest = format!("{:x}", Sha256::digest(TOKEN.as_bytes()));
        Arc::new(KeyStore::from_digests(std::collections::HashMap::from([(
            digest,
            crate::gateway::auth::Identity {
                tenant: "payments".into(),
                tier: 0,
                rpm: None,
                tpm: None,
                budget_usd: None,
                budget_window_secs: None,
                budget_allow_unpriced: false,
            },
        )])))
    }

    /// What the replica's configuration names: the one key, by its credential
    /// scope (the digest of its token).
    fn names() -> crate::gateway::usage::KeyNames {
        crate::gateway::usage::KeyNames::new([(
            format!("{:x}", Sha256::digest(TOKEN.as_bytes())),
            "checkout-service".to_string(),
        )])
    }

    fn app(log: Arc<JsonLines>) -> axum::Router {
        use axum::routing::{get, post};
        let keys = store();
        axum::Router::new()
            .route("/v1/messages", post(|| async { "ok" }))
            .route("/v1/other", post(|| async { "ok" }))
            .route(
                "/slow",
                get(|| async {
                    std::future::pending::<()>().await;
                }),
            )
            .layer(axum::middleware::from_fn(move |request, next| {
                let log = log.clone();
                let keys = keys.clone();
                let names = names();
                async move { record_request(Some(log), None, keys, names, None, request, next).await }
            }))
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        header: Option<(&str, &str)>,
    ) -> u16 {
        use tower::ServiceExt;
        let mut request = axum::http::Request::builder().method(method).uri(path);
        if let Some((name, value)) = header {
            request = request.header(name, value);
        }
        app.clone()
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    #[tokio::test]
    async fn the_native_routes_credential_headers_attribute_the_request() {
        let out = Shared::default();
        let log = Arc::new(JsonLines::to_writer(Box::new(out.clone())));
        let app = app(log.clone());
        // Anthropic and Gemini clients send their key in these headers; the
        // gateway reads them on the native routes, so the log must too.
        send(&app, "POST", "/v1/messages", Some(("x-api-key", TOKEN))).await;
        send(
            &app,
            "POST",
            "/v1/messages",
            Some(("x-goog-api-key", TOKEN)),
        )
        .await;
        // A wrong key is still fingerprinted, with no tenant.
        send(
            &app,
            "POST",
            "/v1/messages",
            Some(("x-api-key", "wrong-key")),
        )
        .await;
        // Off the native routes the gateway does not read x-api-key, so neither
        // does the log.
        send(&app, "POST", "/v1/other", Some(("x-api-key", TOKEN))).await;
        log.sync();
        let lines = out.lines();
        assert_eq!(lines[0]["tenant"], "payments", "{lines:?}");
        assert_eq!(lines[1]["tenant"], "payments", "{lines:?}");
        assert!(lines[2]["tenant"].is_null() && lines[2]["credential"].is_string());
        assert!(lines[3]["tenant"].is_null() && lines[3]["credential"].is_null());
        // The key is named beside the tenant; an unknown or absent credential
        // names no key.
        assert_eq!(lines[0]["key"], "checkout-service", "{lines:?}");
        assert_eq!(lines[1]["key"], "checkout-service", "{lines:?}");
        assert!(lines[2]["key"].is_null() && lines[3]["key"].is_null());
    }

    #[tokio::test]
    async fn every_line_has_the_id_the_caller_is_given_even_from_an_outer_layer() {
        // This app has no request-id layer inside the log, like a response made by
        // a layer outside the gateway's own (the origin check, a hidden admin
        // path). The log settles the id itself and puts it on the response.
        use tower::ServiceExt;
        let out = Shared::default();
        let log = Arc::new(JsonLines::to_writer(Box::new(out.clone())));
        let app = app(log.clone());
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/other")
                    .header("x-request-id", "run-9.step-1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()["x-request-id"], "run-9.step-1");
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v1/other")
                    .header("x-request-id", "gw-0123456789ab-1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let minted = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            minted.starts_with("gw-") && minted != "gw-0123456789ab-1",
            "{minted}"
        );
        log.sync();
        let lines = out.lines();
        assert_eq!(lines[0]["request_id"], "run-9.step-1");
        assert_eq!(lines[1]["request_id"], minted.as_str());
    }

    #[tokio::test]
    async fn a_request_cancelled_before_its_response_still_gets_a_line() {
        let out = Shared::default();
        let log = Arc::new(JsonLines::to_writer(Box::new(out.clone())));
        let app = app(log.clone());
        // The handler never answers; the deadline drops the future, as a client
        // disconnect does.
        let abandoned = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            send(&app, "GET", "/slow", None),
        )
        .await;
        assert!(abandoned.is_err(), "the request should have been abandoned");
        log.sync();
        let lines = out.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0]["status"].is_null(), "{lines:?}");
        assert_eq!(lines[0]["path"], "/slow");
        // The abandoned request still has its id, so it joins to the usage record
        // of any attempt it had already started.
        assert!(
            lines[0]["request_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("gw-")),
            "{lines:?}"
        );
        // A finished request writes exactly one line, not two.
        send(&app, "POST", "/v1/other", None).await;
        log.sync();
        assert_eq!(out.lines().len(), 2);
    }

    #[tokio::test]
    async fn caller_controlled_fields_are_cut_and_every_line_ends_in_one_newline() {
        let out = Shared::default();
        let log = Arc::new(JsonLines::to_writer(Box::new(out.clone())));
        let app = app(log.clone());
        let long = format!("/{}", "a".repeat(5000));
        send(&app, "GET", &long, None).await;
        log.sync();
        let raw = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert!(raw.ends_with('\n') && raw.matches('\n').count() == 1);
        let line = &out.lines()[0];
        assert_eq!(line["path"].as_str().unwrap().chars().count(), 256);
        assert!(raw.len() < 1024, "{}", raw.len());
        // A non-ASCII path is cut on a character boundary, not in the middle of
        // one.
        assert_eq!(bounded(&"é".repeat(300)).chars().count(), 256);
    }
}

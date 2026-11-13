//! The access log: one JSON line per request on the main listener.
//!
//! It is a middleware around the whole application, so it needs nothing from
//! the gateway's internals and sees every outcome, including requests refused
//! before they reach a handler. A line holds:
//!
//! - `ts`: RFC 3339 time the response headers were ready, UTC;
//! - `request_id`: the `x-request-id` the gateway put on the response;
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
//! - `credential`: the first eight hex characters of the SHA-256 of the bearer
//!   token, `null` without one. It tells repeated use of one credential apart
//!   without revealing it, including a wrong one being tried again and again.
//!
//! `path` and `request_id` come from the caller, so each is cut to 256
//! characters. No token, header value, query string, body or model output is
//! ever written.

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
    pub credential: Option<String>,
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
    log: Arc<JsonLines>,
    started: std::time::Instant,
    method: String,
    path: String,
    tenant: Option<String>,
    credential: Option<String>,
    written: bool,
}

impl Pending {
    fn write(&mut self, status: Option<u16>, request_id: Option<String>) {
        self.written = true;
        self.log.write(&Record {
            event: "access",
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            request_id,
            method: std::mem::take(&mut self.method),
            path: std::mem::take(&mut self.path),
            status,
            latency_ms: self.started.elapsed().as_millis() as u64,
            tenant: self.tenant.take(),
            credential: self.credential.take(),
        });
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.written {
            self.write(None, None);
        }
    }
}

pub(crate) async fn record_request(
    log: Arc<JsonLines>,
    keys: Arc<KeyStore>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    // Resolve the caller before the request is consumed, from the headers as the
    // gateway reads them: on the native routes an `x-api-key` or
    // `x-goog-api-key` is the credential. The same store the gateway
    // authenticates with decides, so the log cannot disagree with it.
    let mut headers = request.headers().clone();
    if crate::proxy::wire::native_route(&path).is_some() {
        crate::proxy::wire::normalize_auth(&mut headers);
    }
    let mut pending = Pending {
        log,
        started: std::time::Instant::now(),
        method: request.method().to_string(),
        path: bounded(&path),
        tenant: keys.identify(&headers).ok().map(|identity| identity.tenant),
        credential: fingerprint(&headers),
        written: false,
    };
    let response = next.run(request).await;
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(bounded);
    pending.write(Some(response.status().as_u16()), request_id);
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
            credential: Some("0a1b2c3d".into()),
        };
        let line = serde_json::to_string(&record).unwrap();
        assert!(!line.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(value["tenant"].is_null() && value["request_id"].is_null());
        assert_eq!(value["status"], 401);
        assert_eq!(value["credential"], "0a1b2c3d");
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
                async move { record_request(log, keys, request, next).await }
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
        let app = app(Arc::new(JsonLines::to_writer(Box::new(out.clone()))));
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
        let lines = out.lines();
        assert_eq!(lines[0]["tenant"], "payments", "{lines:?}");
        assert_eq!(lines[1]["tenant"], "payments", "{lines:?}");
        assert!(lines[2]["tenant"].is_null() && lines[2]["credential"].is_string());
        assert!(lines[3]["tenant"].is_null() && lines[3]["credential"].is_null());
    }

    #[tokio::test]
    async fn a_request_cancelled_before_its_response_still_gets_a_line() {
        let out = Shared::default();
        let app = app(Arc::new(JsonLines::to_writer(Box::new(out.clone()))));
        // The handler never answers; the deadline drops the future, as a client
        // disconnect does.
        let abandoned = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            send(&app, "GET", "/slow", None),
        )
        .await;
        assert!(abandoned.is_err(), "the request should have been abandoned");
        let lines = out.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0]["status"].is_null(), "{lines:?}");
        assert_eq!(lines[0]["path"], "/slow");
        // A finished request writes exactly one line, not two.
        send(&app, "POST", "/v1/other", None).await;
        assert_eq!(out.lines().len(), 2);
    }

    #[tokio::test]
    async fn caller_controlled_fields_are_cut_and_every_line_ends_in_one_newline() {
        let out = Shared::default();
        let app = app(Arc::new(JsonLines::to_writer(Box::new(out.clone()))));
        let long = format!("/{}", "a".repeat(5000));
        send(&app, "GET", &long, None).await;
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

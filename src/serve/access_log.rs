//! The access log: one JSON line per request on the main listener.
//!
//! It is a middleware around the whole application, so it needs nothing from
//! the gateway's internals and sees every outcome, including requests refused
//! before they reach a handler. A line holds:
//!
//! - `event`: always `"access"`;
//! - `ts`: RFC 3339 time the response headers were ready, UTC;
//! - `request_id`: the `x-request-id` the gateway put on the response;
//! - `method`, `path`: the request line without its query string;
//! - `status`: the HTTP status;
//! - `latency_ms`: until the response headers, so a stream's duration is not
//!   included;
//! - `tenant`: the caller's tenant, `null` when the credential was missing or
//!   not recognised;
//! - `credential`: the first eight hex characters of the SHA-256 of the bearer
//!   token, `null` without one. It tells repeated use of one credential apart
//!   without revealing it, including a wrong one being tried again and again.
//!
//! No token, header value, query string, body or model output is ever written.

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
    pub status: u16,
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

pub(crate) async fn record_request(
    log: Arc<JsonLines>,
    keys: Arc<KeyStore>,
    request: Request,
    next: Next,
) -> Response {
    let started = std::time::Instant::now();
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    // Resolve the caller before the request is consumed. The same store the
    // gateway authenticates with decides, so the log cannot disagree with it.
    let credential = fingerprint(request.headers());
    let tenant = keys
        .identify(request.headers())
        .ok()
        .map(|identity| identity.tenant);
    let response = next.run(request).await;
    log.write(&Record {
        event: "access",
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        request_id: response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        method,
        path,
        status: response.status().as_u16(),
        latency_ms: started.elapsed().as_millis() as u64,
        tenant,
        credential,
    });
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

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
        assert_eq!(
            print,
            format!("{:x}", Sha256::digest(b"sk-secret-token-value"))[..8]
        );
        assert!(!print.contains("secret"));
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
            status: 401,
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
}

//! Workload tokens as caller credentials (0017), for the main listener.
//!
//! A request whose bearer value is shaped like a JWT (and only then, and only when
//! an issuer is configured) is verified as one: the issuer's rules, keys from its
//! JWKS, claims after the signature. A verified `(issuer table, sub)` that a
//! `[[keys]]` entry binds becomes that key's identity the same way a client
//! certificate does: the token is removed from the request and the workload name
//! is placed in the internal header the key store reads (`x-llmshim-peer-san`),
//! which the outermost layer strips from everything a caller sends. A token that
//! does not verify is refused here and never looked up as a key.

use super::jwks::{HttpFetch, Jwks, Settings, Status};
use super::jwt::{self, Failure, Policy};
use crate::gateway::auth::{KeyStore, PEER_SAN_HEADER};
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

struct Issuer {
    name: String,
    policy: Policy,
    jwks: Arc<Jwks>,
}

/// Why a request was refused before the gateway saw it, as the access log names
/// it (`auth_reason`): a class, never the token or the claim that failed.
pub(crate) struct Refusal {
    status: StatusCode,
    pub reason: &'static str,
    retry_after: Option<u64>,
    wire: Option<crate::proxy::wire::Wire>,
}

impl Refusal {
    pub fn into_response(self) -> Response {
        let message = if self.status == StatusCode::SERVICE_UNAVAILABLE {
            "Authentication is temporarily unavailable"
        } else {
            "Missing or invalid API key"
        };
        let mut response = match self.wire {
            Some(wire) => crate::proxy::wire::fail(wire, self.status, message),
            None => (
                self.status,
                axum::Json(serde_json::json!({
                    "error": {
                        "code": if self.status == StatusCode::SERVICE_UNAVAILABLE {
                            "unavailable"
                        } else {
                            "unauthorized"
                        },
                        "message": message
                    }
                })),
            )
                .into_response(),
        };
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from(seconds));
        }
        response
    }
}

/// The trusted issuers, by the `iss` they put in a token.
#[derive(Clone)]
pub(crate) struct JwtAuth {
    issuers: Arc<HashMap<String, Arc<Issuer>>>,
    keys: Arc<KeyStore>,
}

impl JwtAuth {
    /// `None` when no issuer is configured, so nothing changes for a file
    /// without `[[auth.jwt]]`.
    pub fn new(
        config: &super::config::ServeConfig,
        env: &dyn Fn(&str) -> Option<String>,
        keys: Arc<KeyStore>,
    ) -> Result<Option<Self>, Vec<super::config::Issue>> {
        if config.auth.jwt.is_empty() {
            return Ok(None);
        }
        let mut issuers = HashMap::new();
        let mut problems = Vec::new();
        for (index, table) in config.auth.jwt.iter().enumerate() {
            let at = format!("auth.jwt[{index}]");
            let mut headers = Vec::new();
            for (name, secret) in &table.headers {
                match super::config::read_secret(secret, env) {
                    Ok(value) => headers.push((name.clone(), value.trim().to_owned())),
                    Err(message) => problems.push(super::config::Issue {
                        path: format!("{at}.headers.{name}"),
                        message,
                    }),
                }
            }
            let ca = match &table.ca_file {
                Some(path) => match std::fs::read(path) {
                    Ok(bytes) => Some(bytes),
                    Err(error) => {
                        problems.push(super::config::Issue {
                            path: format!("{at}.ca_file"),
                            message: format!("cannot read {}: {}", path.display(), error.kind()),
                        });
                        continue;
                    }
                },
                None => None,
            };
            let fetch = match HttpFetch::new(&table.jwks_url, ca.as_deref(), headers) {
                Ok(fetch) => fetch,
                Err(message) => {
                    problems.push(super::config::Issue {
                        path: format!("{at}.jwks_url"),
                        message,
                    });
                    continue;
                }
            };
            let policy = Policy {
                issuer: table.issuer.clone(),
                audience: table.audience.clone(),
                algorithms: table
                    .algorithms
                    .iter()
                    .filter_map(|name| jwt::Alg::parse(name).ok())
                    .collect(),
                skew_secs: table.clock_skew_secs.unwrap_or(30),
            };
            let settings = Settings {
                refresh: Duration::from_secs(table.jwks_refresh_secs.unwrap_or(300)),
                max_stale: Duration::from_secs(table.jwks_max_stale_secs.unwrap_or(3600)),
            };
            issuers.insert(
                table.issuer.clone(),
                Arc::new(Issuer {
                    name: table.name.clone(),
                    policy,
                    jwks: Arc::new(Jwks::new(&table.name, Arc::new(fetch), settings)),
                }),
            );
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        Ok(Some(Self {
            issuers: Arc::new(issuers),
            keys,
        }))
    }

    /// Each issuer's key state, for `/v1/serve/config`.
    pub fn statuses(&self) -> BTreeMap<String, Status> {
        self.issuers
            .values()
            .map(|issuer| (issuer.name.clone(), issuer.jwks.status()))
            .collect()
    }

    /// Authenticate `request` if it carries a token. `Ok` means the request goes
    /// on: untouched when it carries no token, or rewritten to carry the bound
    /// workload instead of the token.
    pub async fn authenticate(&self, request: &mut Request) -> Result<(), Refusal> {
        let path = request.uri().path().to_owned();
        if crate::gateway::http::ADMIN_PATHS.contains(&path.as_str()) {
            return Ok(());
        }
        let native = crate::proxy::wire::native_route(&path).map(|route| route.wire);
        // As the gateway reads the credential: on the native routes `x-api-key`
        // and `x-goog-api-key` count.
        let mut headers = request.headers().clone();
        if native.is_some() {
            crate::proxy::wire::normalize_auth(&mut headers);
        }
        let Some(token) = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
            })
            .map(str::trim)
            .filter(|token| jwt::is_jwt_shaped(token))
        else {
            return Ok(());
        };
        let refuse = |status: StatusCode, reason: &'static str, retry_after: Option<u64>| Refusal {
            status,
            reason,
            retry_after,
            wire: native,
        };
        let invalid = || refuse(StatusCode::UNAUTHORIZED, "jwt_invalid", None);
        // The unverified `iss` only picks the issuer's rules; no match means no
        // network activity at all.
        let issuer = jwt::peek_issuer(token)
            .and_then(|iss| self.issuers.get(&iss))
            .ok_or_else(invalid)?;
        let unavailable = || {
            refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "jwks_unavailable",
                Some(30),
            )
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let keys = issuer.jwks.keys().await.map_err(|_| unavailable())?;
        let mut outcome = jwt::verify(token, &keys, &issuer.policy, now);
        if outcome == Err(Failure::Key) {
            // An unknown `kid` earns at most one refresh per interval; if the
            // keys did not change the same verdict stands, as a retryable one.
            let fresh = issuer
                .jwks
                .keys_for_unknown_kid()
                .await
                .map_err(|_| unavailable())?;
            if !Arc::ptr_eq(&keys, &fresh) {
                outcome = jwt::verify(token, &fresh, &issuer.policy, now);
            }
            if outcome == Err(Failure::Key) {
                return Err(refuse(
                    StatusCode::UNAUTHORIZED,
                    "jwt_unknown_key",
                    Some(30),
                ));
            }
        }
        let verified = outcome.map_err(|_| invalid())?;
        let workload = super::keys::jwt_workload(&issuer.name, &verified.subject);
        let value = HeaderValue::from_str(&workload).map_err(|_| invalid())?;
        if !self.keys.has_peer(&workload) {
            return Err(refuse(
                StatusCode::UNAUTHORIZED,
                "jwt_unknown_subject",
                None,
            ));
        }
        let headers = request.headers_mut();
        headers.remove(axum::http::header::AUTHORIZATION);
        headers.remove("x-api-key");
        headers.remove("x-goog-api-key");
        headers.insert(PEER_SAN_HEADER, value);
        Ok(())
    }
}

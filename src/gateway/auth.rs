//! Gateway authentication and per-key identity (tenant, tier, per-tenant quotas).
//!
//! The priority tier must come from an **authenticated** identity, not a
//! client-supplied header — otherwise any caller sets `x-llmshim-priority: 255`
//! and jumps the queue. When an API-key file is configured, the gateway requires
//! `Authorization: Bearer <key>` and derives tier + tenant from the key (the
//! header is ignored). When no keys are configured it stays **open** for local
//! dev and honors the header, so nothing breaks out of the box.

use std::collections::HashMap;

use axum::http::HeaderMap;
use serde::Deserialize;

fn anonymous() -> String {
    "anonymous".to_string()
}

/// Who a request belongs to, resolved from its API key (or the header in open
/// mode). `rpm`/`tpm` are optional per-tenant limits enforced by the gateway.
#[derive(Clone, Debug, Deserialize)]
pub struct Identity {
    #[serde(default = "anonymous")]
    pub tenant: String,
    #[serde(default)]
    pub tier: u8,
    #[serde(default)]
    pub rpm: Option<u32>,
    #[serde(default)]
    pub tpm: Option<u32>,
}

/// Authentication failure — both map to HTTP 401.
#[derive(Debug)]
pub enum AuthError {
    MissingKey,
    InvalidKey,
}

/// API-key registry. `Open` = no auth (dev); `Enforced` = Bearer required.
pub enum KeyStore {
    Open,
    Enforced(HashMap<String, Identity>),
}

impl KeyStore {
    /// Load from `LLMSHIM_GATEWAY_KEYS_FILE` (a JSON object mapping API key →
    /// identity). Unset → open mode. A set-but-unreadable/invalid file is a boot
    /// error and **exits** rather than silently running unauthenticated.
    pub fn from_env() -> Self {
        match std::env::var("LLMSHIM_GATEWAY_KEYS_FILE") {
            Ok(path) if !path.trim().is_empty() => {
                let data = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                    eprintln!("gateway: cannot read LLMSHIM_GATEWAY_KEYS_FILE {path}: {e}");
                    std::process::exit(1);
                });
                let map: HashMap<String, Identity> =
                    serde_json::from_str(&data).unwrap_or_else(|e| {
                        eprintln!("gateway: invalid keys file {path}: {e}");
                        std::process::exit(1);
                    });
                eprintln!(
                    "  Auth: ENFORCED ({} API key(s); tier from key, x-llmshim-priority ignored)",
                    map.len()
                );
                KeyStore::Enforced(map)
            }
            _ => {
                eprintln!(
                    "  Auth: OPEN (no LLMSHIM_GATEWAY_KEYS_FILE; x-llmshim-priority trusted)"
                );
                KeyStore::Open
            }
        }
    }

    /// Build an enforced store directly (tests / embedding).
    pub fn enforced(keys: HashMap<String, Identity>) -> Self {
        KeyStore::Enforced(keys)
    }

    /// Whether Bearer auth is required.
    pub fn is_enforced(&self) -> bool {
        matches!(self, KeyStore::Enforced(_))
    }

    /// Resolve the caller's identity. Open mode reads the tier from the client
    /// header; enforced mode requires a valid Bearer key and takes tier + tenant
    /// from it (header ignored, so callers can't self-escalate).
    pub fn identify(&self, headers: &HeaderMap) -> Result<Identity, AuthError> {
        match self {
            KeyStore::Open => Ok(Identity {
                tenant: anonymous(),
                tier: header_tier(headers),
                rpm: None,
                tpm: None,
            }),
            KeyStore::Enforced(map) => {
                let key = bearer_token(headers).ok_or(AuthError::MissingKey)?;
                map.get(key).cloned().ok_or(AuthError::InvalidKey)
            }
        }
    }
}

/// Parse the client `x-llmshim-priority` header into a tier (default `0`).
pub fn header_tier(headers: &HeaderMap) -> u8 {
    headers
        .get("x-llmshim-priority")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u8>().ok())
        .unwrap_or(0)
}

/// Extract the `Authorization: Bearer <token>` value.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let h = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    h.strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    fn hdrs(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn open_mode_trusts_the_priority_header() {
        let store = KeyStore::Open;
        let id = store
            .identify(&hdrs(&[("x-llmshim-priority", "7")]))
            .unwrap();
        assert_eq!(id.tier, 7);
        assert_eq!(id.tenant, "anonymous");
    }

    #[test]
    fn enforced_mode_ignores_the_header_and_uses_the_key() {
        let mut keys = HashMap::new();
        keys.insert(
            "sk-paid".to_string(),
            Identity {
                tenant: "acme".into(),
                tier: 5,
                rpm: Some(100),
                tpm: None,
            },
        );
        let store = KeyStore::enforced(keys);

        // A valid key → its tier, regardless of the (spoofed) header.
        let id = store
            .identify(&hdrs(&[
                ("authorization", "Bearer sk-paid"),
                ("x-llmshim-priority", "255"),
            ]))
            .unwrap();
        assert_eq!(id.tier, 5, "tier must come from the key, not the header");
        assert_eq!(id.tenant, "acme");

        // Missing / bad key → rejected.
        assert!(matches!(
            store.identify(&hdrs(&[("x-llmshim-priority", "255")])),
            Err(AuthError::MissingKey)
        ));
        assert!(matches!(
            store.identify(&hdrs(&[("authorization", "Bearer nope")])),
            Err(AuthError::InvalidKey)
        ));
    }

    #[test]
    fn identity_json_defaults() {
        let id: Identity = serde_json::from_str(r#"{"tenant":"t"}"#).unwrap();
        assert_eq!(id.tier, 0);
        assert!(id.rpm.is_none());
        let full: Identity =
            serde_json::from_str(r#"{"tenant":"t","tier":3,"rpm":50,"tpm":9000}"#).unwrap();
        assert_eq!(full.tier, 3);
        assert_eq!(full.rpm, Some(50));
        assert_eq!(full.tpm, Some(9000));
    }
}

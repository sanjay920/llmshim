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
use sha2::{Digest, Sha256};

fn anonymous() -> String {
    "anonymous".to_string()
}

/// Who a request belongs to, resolved from its API key (or the header in open
/// mode). `rpm`/`tpm` are optional per-tenant limits enforced by the gateway;
/// omitted means unlimited and an explicit zero denies every request.
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
    /// Optional USD spend cap per window, reserved and settled per actual
    /// provider attempt alongside the rate buckets.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// Window the cap applies to, in seconds. Defaults to one day.
    #[serde(default)]
    pub budget_window_secs: Option<u64>,
    /// Permit requests for which a finite reservation cannot be derived while a
    /// budget is set.
    ///
    /// Defaults to **false**, which refuses them. Setting this to `true` accepts
    /// the unknown pre-send liability; known usage is still recorded and later
    /// attempts are rejected once known spend reaches the cap.
    #[serde(default)]
    pub budget_allow_unpriced: bool,
}

/// Authentication failure — both map to HTTP 401.
#[derive(Debug)]
pub enum AuthError {
    MissingKey,
    InvalidKey,
}

/// Authenticated identity plus a non-secret owner scope for request-bound state.
pub(crate) struct IdentifiedCaller {
    pub identity: Identity,
    pub credential_scope: String,
}

/// API-key registry. `Open` = no auth (dev); `Enforced` = Bearer required.
/// `Hashed` is `Enforced` for a store that holds only SHA-256 digests of the
/// keys, so the store itself never keeps a caller key's plaintext. (A token
/// that came from an environment variable is still in the environment.)
pub enum KeyStore {
    Open,
    Enforced(HashMap<String, Identity>),
    /// Identities keyed by the lower-case hex SHA-256 of the bearer token.
    /// Only in a `serve` build, so the `gateway` feature's public enum is what
    /// it was.
    #[cfg(feature = "serve")]
    Hashed(HashMap<String, Identity>),
    /// Bearer tokens as in [`Self::Hashed`], and identities for workloads that
    /// authenticate with a client certificate. The certificate's name reaches
    /// this store in [`PEER_SAN_HEADER`], which the server sets from the
    /// verified TLS handshake and strips from every inbound request, so a caller
    /// can never supply it. Only in a `serve` build.
    #[cfg(feature = "serve")]
    Mutual {
        tokens: HashMap<String, Identity>,
        peers: HashMap<String, Identity>,
    },
}

/// The header through which a verified client certificate's matched name
/// reaches the key store. It is internal: the server removes it from every
/// inbound request before setting it, so only the TLS layer can.
#[cfg(feature = "serve")]
pub const PEER_SAN_HEADER: &str = "x-llmshim-peer-san";

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
                    serde_json::from_str(&data).unwrap_or_else(|error| {
                        eprintln!(
                            "gateway: invalid keys file {path} at line {}, column {}; credentials were not loaded",
                            error.line(),
                            error.column(),
                        );
                        std::process::exit(1);
                    });
                eprintln!(
                    "  Auth: ENFORCED ({} API key(s); tier from key, x-llmshim-priority ignored)",
                    map.len()
                );
                KeyStore::Enforced(map)
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                eprintln!(
                    "gateway: LLMSHIM_GATEWAY_KEYS_FILE is not valid UTF-8; refusing to start"
                );
                std::process::exit(1);
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

    /// Build an enforced store that holds only key digests. Each map key is the
    /// lower-case hex SHA-256 of a bearer token. Requests are authenticated and
    /// scoped exactly as with [`Self::enforced`] given the same tokens.
    #[cfg(feature = "serve")]
    pub fn from_digests(digests: HashMap<String, Identity>) -> Self {
        KeyStore::Hashed(digests)
    }

    /// An enforced store that also accepts client certificates. `tokens` is keyed
    /// by the SHA-256 digest of a bearer token as for [`Self::from_digests`];
    /// `peers` by the canonical name (`dns:<lower-case>` or `uri:<uri>`) a
    /// verified certificate carries.
    #[cfg(feature = "serve")]
    pub fn mutual(tokens: HashMap<String, Identity>, peers: HashMap<String, Identity>) -> Self {
        KeyStore::Mutual { tokens, peers }
    }

    /// Whether Bearer auth is required.
    pub fn is_enforced(&self) -> bool {
        match self {
            KeyStore::Open => false,
            KeyStore::Enforced(_) => true,
            #[cfg(feature = "serve")]
            KeyStore::Hashed(_) | KeyStore::Mutual { .. } => true,
        }
    }

    /// Resolve the caller's identity. Open mode reads the tier from the client
    /// header; enforced mode requires a valid Bearer key and takes tier + tenant
    /// from it (header ignored, so callers can't self-escalate).
    pub fn identify(&self, headers: &HeaderMap) -> Result<Identity, AuthError> {
        self.identify_caller(headers).map(|caller| caller.identity)
    }

    /// Resolve the identity and a stable credential-specific scope. Distinct API
    /// keys remain separate even when an operator assigns them the same tenant.
    pub(crate) fn identify_caller(
        &self,
        headers: &HeaderMap,
    ) -> Result<IdentifiedCaller, AuthError> {
        match self {
            KeyStore::Open => Ok(IdentifiedCaller {
                identity: Identity {
                    tenant: anonymous(),
                    tier: header_tier(headers),
                    rpm: None,
                    tpm: None,
                    budget_usd: None,
                    budget_window_secs: None,
                    budget_allow_unpriced: false,
                },
                credential_scope: "open-development-mode".to_string(),
            }),
            KeyStore::Enforced(map) => {
                let key = bearer_token(headers).ok_or(AuthError::MissingKey)?;
                let identity = map.get(key).cloned().ok_or(AuthError::InvalidKey)?;
                let credential_scope = format!("{:x}", Sha256::digest(key.as_bytes()));
                Ok(IdentifiedCaller {
                    identity,
                    credential_scope,
                })
            }
            #[cfg(feature = "serve")]
            KeyStore::Mutual { tokens, peers } => match bearer_token(headers) {
                // A bearer token decides on its own; a certificate never rescues a
                // wrong token.
                Some(key) => {
                    let credential_scope = format!("{:x}", Sha256::digest(key.as_bytes()));
                    let identity = tokens
                        .get(&credential_scope)
                        .cloned()
                        .ok_or(AuthError::InvalidKey)?;
                    Ok(IdentifiedCaller {
                        identity,
                        credential_scope,
                    })
                }
                None => {
                    let name = headers
                        .get(PEER_SAN_HEADER)
                        .and_then(|value| value.to_str().ok())
                        .ok_or(AuthError::MissingKey)?;
                    let identity = peers.get(name).cloned().ok_or(AuthError::InvalidKey)?;
                    // Scoped to the workload name, in a space tokens cannot reach.
                    let credential_scope = format!("{:x}", Sha256::digest(format!("mtls:{name}")));
                    Ok(IdentifiedCaller {
                        identity,
                        credential_scope,
                    })
                }
            },
            #[cfg(feature = "serve")]
            KeyStore::Hashed(map) => {
                let key = bearer_token(headers).ok_or(AuthError::MissingKey)?;
                // The digest is the credential scope in both modes, so state
                // owned by a key survives moving it from plaintext to hashed.
                let credential_scope = format!("{:x}", Sha256::digest(key.as_bytes()));
                let identity = map
                    .get(&credential_scope)
                    .cloned()
                    .ok_or(AuthError::InvalidKey)?;
                Ok(IdentifiedCaller {
                    identity,
                    credential_scope,
                })
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
                budget_usd: None,
                budget_window_secs: None,
                budget_allow_unpriced: false,
            },
        );
        keys.insert(
            "sk-paid-rotated".to_string(),
            Identity {
                tenant: "acme".into(),
                tier: 5,
                rpm: Some(100),
                tpm: None,
                budget_usd: None,
                budget_window_secs: None,
                budget_allow_unpriced: false,
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

        let first_caller = store
            .identify_caller(&hdrs(&[("authorization", "Bearer sk-paid")]))
            .unwrap();
        let rotated_caller = store
            .identify_caller(&hdrs(&[("authorization", "Bearer sk-paid-rotated")]))
            .unwrap();
        assert_ne!(
            first_caller.credential_scope, rotated_caller.credential_scope,
            "keys for the same tenant need separate state ownership"
        );
        assert!(!first_caller.credential_scope.contains("sk-paid"));

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

    #[cfg(feature = "serve")]
    fn digest(token: &str) -> String {
        format!("{:x}", Sha256::digest(token.as_bytes()))
    }

    #[cfg(feature = "serve")]
    fn identity(tenant: &str, tier: u8) -> Identity {
        Identity {
            tenant: tenant.into(),
            tier,
            rpm: None,
            tpm: None,
            budget_usd: None,
            budget_window_secs: None,
            budget_allow_unpriced: false,
        }
    }

    #[cfg(feature = "serve")]
    #[test]
    fn a_hashed_store_authenticates_and_scopes_like_a_plaintext_store() {
        let plain = KeyStore::enforced(HashMap::from([
            ("sk-paid".to_string(), identity("acme", 5)),
            ("sk-other".to_string(), identity("acme", 1)),
        ]));
        let hashed = KeyStore::from_digests(HashMap::from([
            (digest("sk-paid"), identity("acme", 5)),
            (digest("sk-other"), identity("acme", 1)),
        ]));
        assert!(hashed.is_enforced());
        for token in ["sk-paid", "sk-other"] {
            let header = format!("Bearer {token}");
            let spoof = [
                ("authorization", header.as_str()),
                ("x-llmshim-priority", "255"),
            ];
            let (a, b) = (
                plain.identify_caller(&hdrs(&spoof)).unwrap(),
                hashed.identify_caller(&hdrs(&spoof)).unwrap(),
            );
            assert_eq!(
                (a.identity.tenant, a.identity.tier),
                (b.identity.tenant, b.identity.tier)
            );
            assert_eq!(
                a.credential_scope, b.credential_scope,
                "moving a key to hashed storage must not orphan state it owns"
            );
        }
    }

    #[cfg(feature = "serve")]
    #[test]
    fn a_hashed_store_rejects_missing_wrong_and_digest_as_token() {
        let hashed =
            KeyStore::from_digests(HashMap::from([(digest("sk-paid"), identity("acme", 5))]));
        assert!(matches!(
            hashed.identify(&hdrs(&[])),
            Err(AuthError::MissingKey)
        ));
        assert!(matches!(
            hashed.identify(&hdrs(&[("authorization", "Bearer sk-nope")])),
            Err(AuthError::InvalidKey)
        ));
        // The digest is not the credential: presenting it must not authenticate.
        let as_token = format!("Bearer {}", digest("sk-paid"));
        assert!(matches!(
            hashed.identify(&hdrs(&[("authorization", as_token.as_str())])),
            Err(AuthError::InvalidKey)
        ));
    }

    #[cfg(feature = "serve")]
    fn mutual() -> KeyStore {
        KeyStore::mutual(
            HashMap::from([(digest("sk-paid"), identity("platform", 1))]),
            HashMap::from([
                (
                    "uri:spiffe://corp/payments/checkout".to_string(),
                    identity("payments", 2),
                ),
                ("dns:batch.corp".to_string(), identity("analytics", 0)),
            ]),
        )
    }

    #[cfg(feature = "serve")]
    #[test]
    fn a_mutual_store_identifies_a_workload_by_its_bound_name_and_scopes_it_apart() {
        let store = mutual();
        assert!(store.is_enforced());
        let bound = [(PEER_SAN_HEADER, "uri:spiffe://corp/payments/checkout")];
        let caller = store.identify_caller(&hdrs(&bound)).unwrap();
        assert_eq!(
            (caller.identity.tenant.as_str(), caller.identity.tier),
            ("payments", 2)
        );
        // The workload's scope cannot collide with any token's scope.
        let token = store
            .identify_caller(&hdrs(&[("authorization", "Bearer sk-paid")]))
            .unwrap();
        assert_ne!(caller.credential_scope, token.credential_scope);
        assert!(!caller.credential_scope.contains("payments"));
        // An unknown name, or none, is not an identity.
        assert!(matches!(
            store.identify(&hdrs(&[(PEER_SAN_HEADER, "uri:spiffe://corp/other")])),
            Err(AuthError::InvalidKey)
        ));
        assert!(matches!(
            store.identify(&hdrs(&[])),
            Err(AuthError::MissingKey)
        ));
    }

    #[cfg(feature = "serve")]
    #[test]
    fn in_a_mutual_store_a_bearer_token_decides_alone() {
        let store = mutual();
        let both = |token: &str| {
            let value = format!("Bearer {token}");
            store.identify(&hdrs(&[
                ("authorization", value.as_str()),
                (PEER_SAN_HEADER, "uri:spiffe://corp/payments/checkout"),
            ]))
        };
        assert_eq!(both("sk-paid").unwrap().tenant, "platform");
        // A valid certificate does not rescue a wrong token.
        assert!(matches!(both("sk-wrong"), Err(AuthError::InvalidKey)));
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
        assert!(id.budget_usd.is_none(), "no budget unless the key sets one");
        let capped: Identity =
            serde_json::from_str(r#"{"tenant":"t","budget_usd":25.0,"budget_window_secs":3600}"#)
                .unwrap();
        assert_eq!(capped.budget_usd, Some(25.0));
        assert_eq!(capped.budget_window_secs, Some(3600));
    }
}

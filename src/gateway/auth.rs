//! Gateway authentication and per-key identity (tenant, tier, per-tenant quotas).
//!
//! The priority tier must come from an **authenticated** identity, not a
//! client-supplied header — otherwise any caller sets `x-llmshim-priority: 255`
//! and jumps the queue. When an API-key file is configured, the gateway requires
//! `Authorization: Bearer <key>` and derives tier + tenant from the key (the
//! header is ignored). When no keys are configured it stays **open** for local
//! dev and honors the header, so nothing breaks out of the box.

use std::collections::HashMap;
#[cfg(feature = "serve")]
use std::sync::{Arc, PoisonError, RwLock};

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
    /// A store that can be replaced while the server runs. Every clone of the
    /// handle sees a replacement at the same moment, and each request is
    /// authenticated against exactly one version. Only in a `serve` build.
    #[cfg(feature = "serve")]
    Live(LiveKeys),
}

/// A shared, replaceable [`KeyStore`]. Replacing it is how a configuration
/// reload revokes, adds or re-limits a caller key without a restart.
#[cfg(feature = "serve")]
#[derive(Clone)]
pub struct LiveKeys(Arc<RwLock<Arc<KeyStore>>>);

#[cfg(feature = "serve")]
impl LiveKeys {
    /// Start with `initial`. A store that is itself live contributes its
    /// current contents, so handles never nest.
    pub fn new(initial: KeyStore) -> Self {
        Self(Arc::new(RwLock::new(Self::settled(initial))))
    }

    fn settled(store: KeyStore) -> Arc<KeyStore> {
        match store {
            KeyStore::Live(live) => live.current(),
            other => Arc::new(other),
        }
    }

    /// The version in force now. Hold it for the duration of one decision.
    pub fn current(&self) -> Arc<KeyStore> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Make `next` the version in force. Requests already decided keep their
    /// answer; the next one is decided by `next`.
    pub fn replace(&self, next: KeyStore) {
        let next = Self::settled(next);
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = next;
    }
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
            #[cfg(feature = "serve")]
            KeyStore::Live(live) => live.current().is_enforced(),
        }
    }

    /// Whether `name` (`dns:<lower-case>` or `uri:<uri>`) is a workload that a
    /// client certificate can authenticate as.
    #[cfg(feature = "serve")]
    pub(crate) fn has_peer(&self, name: &str) -> bool {
        match self {
            KeyStore::Mutual { peers, .. } => peers.contains_key(name),
            KeyStore::Live(live) => live.current().has_peer(name),
            _ => false,
        }
    }

    /// How many credentials the store holds.
    #[cfg(feature = "serve")]
    pub fn credential_count(&self) -> usize {
        match self {
            KeyStore::Open => 0,
            KeyStore::Enforced(map) | KeyStore::Hashed(map) => map.len(),
            KeyStore::Mutual { tokens, peers } => tokens.len() + peers.len(),
            KeyStore::Live(live) => live.current().credential_count(),
        }
    }

    /// A digest of everything that decides who a request is: the credentials
    /// (themselves hashed first, so a plaintext key never reaches the input) and
    /// the identity each carries. Two stores with the same fingerprint authenticate
    /// every request the same way.
    #[cfg(feature = "serve")]
    pub(crate) fn fingerprint(&self) -> String {
        fn entries(kind: &str, map: &HashMap<String, Identity>, out: &mut Vec<String>) {
            for (credential, identity) in map {
                out.push(format!(
                    "{kind}|{:x}|{identity:?}",
                    Sha256::digest(credential.as_bytes())
                ));
            }
        }
        let mut lines = Vec::new();
        match self {
            KeyStore::Open => lines.push("open".to_string()),
            KeyStore::Enforced(map) => entries("plain", map, &mut lines),
            KeyStore::Hashed(map) => entries("digest", map, &mut lines),
            KeyStore::Mutual { tokens, peers } => {
                entries("digest", tokens, &mut lines);
                entries("peer", peers, &mut lines);
            }
            KeyStore::Live(live) => return live.current().fingerprint(),
        }
        lines.sort();
        format!("{:x}", Sha256::digest(lines.join("\n").as_bytes()))
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
            #[cfg(feature = "serve")]
            KeyStore::Live(live) => live.current().identify_caller(headers),
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
    #[cfg(feature = "serve")]
    #[test]
    fn replacing_a_live_store_revokes_adds_and_relimits_for_every_holder() {
        let live = LiveKeys::new(KeyStore::from_digests(HashMap::from([(
            digest("sk-old"),
            identity("acme", 1),
        )])));
        let first = KeyStore::Live(live.clone());
        let second = KeyStore::Live(live.clone());
        let old = hdrs(&[("authorization", "Bearer sk-old")]);
        let new = hdrs(&[("authorization", "Bearer sk-new")]);
        assert_eq!(first.identify(&old).unwrap().tier, 1);
        assert!(second.identify(&new).is_err());

        live.replace(KeyStore::from_digests(HashMap::from([
            (digest("sk-old"), identity("acme", 9)),
            (digest("sk-new"), identity("globex", 2)),
        ])));
        // Every holder of the handle sees it, and a re-limited key carries its new
        // identity.
        for store in [&first, &second] {
            assert_eq!(store.identify(&old).unwrap().tier, 9);
            assert_eq!(store.identify(&new).unwrap().tenant, "globex");
        }

        live.replace(KeyStore::from_digests(HashMap::from([(
            digest("sk-new"),
            identity("globex", 2),
        )])));
        for store in [&first, &second] {
            assert!(matches!(store.identify(&old), Err(AuthError::InvalidKey)));
            assert!(store.identify(&new).is_ok());
        }
        assert!(first.is_enforced());
        assert_eq!(first.credential_count(), 1);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn live_handles_never_nest() {
        let inner = LiveKeys::new(KeyStore::from_digests(HashMap::from([(
            digest("sk-a"),
            identity("a", 0),
        )])));
        let outer = LiveKeys::new(KeyStore::Live(inner.clone()));
        // The outer handle holds what the inner one held at that moment; later
        // replacing the inner one does not reach it, and nothing can loop.
        inner.replace(KeyStore::Open);
        let headers = hdrs(&[("authorization", "Bearer sk-a")]);
        // `inner` was replaced by an open store after `outer` took its contents; the
        // tenant proves `outer` still holds the enforced store it began with (an
        // open store would answer "anonymous" to anyone).
        let held = KeyStore::Live(outer.clone()).identify(&headers).unwrap();
        assert_eq!(held.tenant, "a");
        assert!(KeyStore::Live(outer.clone())
            .identify(&hdrs(&[("authorization", "Bearer sk-b")]))
            .is_err());
        outer.replace(KeyStore::Live(outer.clone()));
        assert_eq!(
            KeyStore::Live(outer).identify(&headers).unwrap().tenant,
            "a"
        );
    }

    #[cfg(feature = "serve")]
    #[test]
    fn a_fingerprint_follows_what_decides_a_request_and_nothing_else() {
        let a = KeyStore::from_digests(HashMap::from([
            (digest("sk-1"), identity("acme", 1)),
            (digest("sk-2"), identity("globex", 2)),
        ]));
        let same_in_another_order = KeyStore::from_digests(HashMap::from([
            (digest("sk-2"), identity("globex", 2)),
            (digest("sk-1"), identity("acme", 1)),
        ]));
        assert_eq!(a.fingerprint(), same_in_another_order.fingerprint());
        for changed in [
            // a different tier, tenant, token or a removed key
            HashMap::from([
                (digest("sk-1"), identity("acme", 7)),
                (digest("sk-2"), identity("globex", 2)),
            ]),
            HashMap::from([
                (digest("sk-1"), identity("initech", 1)),
                (digest("sk-2"), identity("globex", 2)),
            ]),
            HashMap::from([
                (digest("sk-1"), identity("acme", 1)),
                (digest("sk-3"), identity("globex", 2)),
            ]),
            HashMap::from([(digest("sk-1"), identity("acme", 1))]),
        ] {
            assert_ne!(
                a.fingerprint(),
                KeyStore::from_digests(changed).fingerprint()
            );
        }
        // A handle reports the fingerprint of what it holds now.
        let live = LiveKeys::new(a);
        assert_eq!(
            KeyStore::Live(live.clone()).fingerprint(),
            same_in_another_order.fingerprint()
        );
        // The fingerprint holds no credential: it is a digest of digests.
        assert!(!same_in_another_order.fingerprint().contains("sk-1"));
    }

    #[cfg(feature = "serve")]
    #[test]
    fn only_a_mutual_store_knows_workload_names() {
        let store = mutual();
        assert!(store.has_peer("uri:spiffe://corp/payments/checkout"));
        assert!(!store.has_peer("uri:spiffe://corp/other"));
        assert!(!KeyStore::Open.has_peer("uri:spiffe://corp/payments/checkout"));
        assert!(
            KeyStore::Live(LiveKeys::new(store)).has_peer("uri:spiffe://corp/payments/checkout")
        );
    }
}

//! Single sign-on for the control plane (design note 0049): OIDC token verification, the verified
//! subject as the actor of a change, and three fixed roles read from the token's `roles` claim.
//!
//! The verification is the data plane's: `llmshim::serve::jwt` and `jwks` (note 0017). This module
//! adds the issuer file, the role mapping and the actor, and nothing that signs or issues a token.

use llmshim::serve::jwks::{Fetch, HttpFetch, Jwks, Settings};
use llmshim::serve::jwt::{self, Alg, Failure, Policy};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The issuer file given to `--sso-config`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SsoConfig {
    pub issuer: String,
    pub audience: String,
    pub jwks_url: String,
    /// `RS256`, `ES256` or `EdDSA`; at least one.
    pub algorithms: Vec<String>,
    /// PEM certificates to trust for the JWKS fetch instead of the web PKI roots.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    #[serde(default)]
    pub clock_skew_secs: Option<u64>,
    #[serde(default)]
    pub jwks_refresh_secs: Option<u64>,
    #[serde(default)]
    pub jwks_max_stale_secs: Option<u64>,
    /// Whether the shared `--token-file` token is still accepted (as admin, actor `break-glass`).
    #[serde(default = "yes")]
    pub break_glass: bool,
    /// The admin UI's browser sign-in (note 0051): a public client's id and the provider's endpoints.
    #[serde(default)]
    pub ui: Option<crate::ui::UiOidc>,
}

fn yes() -> bool {
    true
}

/// A role, lowest first: a higher role allows everything a lower one does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Read,
    KeyIssuer,
    Admin,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::KeyIssuer => "key-issuer",
            Self::Admin => "admin",
        }
    }

    /// A value of the `roles` claim; anything else (another application's role) is no role here.
    fn from_claim(value: &str) -> Option<Self> {
        match value {
            "control:read" => Some(Self::Read),
            "control:key-issuer" => Some(Self::KeyIssuer),
            "control:admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

/// Who a request is, once authenticated.
#[derive(Debug, Clone)]
pub struct Principal {
    /// What the change history records.
    pub actor: String,
    /// `None`: authenticated, but the token carried no role of this application.
    pub role: Option<Role>,
    /// `oidc` or `break_glass`.
    pub via: &'static str,
}

/// Why a token was not accepted. Only these two reach the caller: 401 and 503.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Unauthorized,
    Unavailable,
}

/// The one trusted issuer.
pub struct Sso {
    policy: Policy,
    jwks: Arc<Jwks>,
    break_glass: bool,
    ui: Option<crate::ui::UiOidc>,
}

impl Sso {
    /// From the file's content. Every problem is reported by field, never with a value that could be
    /// a secret.
    pub fn from_config(config: &SsoConfig) -> Result<Self, String> {
        let ca =
            match &config.ca_file {
                Some(path) => Some(std::fs::read(path).map_err(|e| {
                    format!("ca_file: cannot read {}: {}", path.display(), e.kind())
                })?),
                None => None,
            };
        let fetch = HttpFetch::new(&config.jwks_url, ca.as_deref(), Vec::new())
            .map_err(|message| format!("jwks_url: {message}"))?;
        Self::with_fetch(config, Arc::new(fetch))
    }

    /// As [`Sso::from_config`] with the keys from `fetch`: how a test supplies a mock issuer's.
    pub fn with_fetch(config: &SsoConfig, fetch: Arc<dyn Fetch>) -> Result<Self, String> {
        if config.issuer.is_empty() {
            return Err("issuer: must not be empty".into());
        }
        if config.audience.is_empty() {
            return Err("audience: must not be empty".into());
        }
        if config.algorithms.is_empty() {
            return Err("algorithms: name at least one of RS256, ES256, EdDSA".into());
        }
        let algorithms = config
            .algorithms
            .iter()
            .map(|name| Alg::parse(name).map_err(|message| format!("algorithms: {message}")))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(ui) = &config.ui {
            ui.validate()?;
        }
        let settings = Settings {
            refresh: Duration::from_secs(config.jwks_refresh_secs.unwrap_or(300)),
            max_stale: Duration::from_secs(config.jwks_max_stale_secs.unwrap_or(3600)),
        };
        Ok(Self {
            policy: Policy {
                issuer: config.issuer.clone(),
                audience: config.audience.clone(),
                algorithms,
                skew_secs: config.clock_skew_secs.unwrap_or(30),
            },
            jwks: Arc::new(Jwks::new("control-sso", fetch, settings)),
            break_glass: config.break_glass,
            ui: config.ui.clone(),
        })
    }

    /// The browser sign-in the admin UI offers, if the issuer file sets one.
    pub fn ui(&self) -> Option<&crate::ui::UiOidc> {
        self.ui.as_ref()
    }

    /// Whether the shared token is still accepted.
    pub fn break_glass(&self) -> bool {
        self.break_glass
    }

    /// Verify a JWT-shaped bearer value. A token that does not verify is `Unauthorized`; keys that
    /// cannot be had are `Unavailable`; neither says why.
    pub async fn authenticate(&self, token: &str) -> Result<Principal, Refusal> {
        // The unverified `iss` only decides whether this is a token of ours at all.
        if jwt::peek_issuer(token).as_deref() != Some(self.policy.issuer.as_str()) {
            return Err(Refusal::Unauthorized);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let keys = self.jwks.keys().await.map_err(|_| Refusal::Unavailable)?;
        let mut outcome = jwt::verify_claims(token, &keys, &self.policy, now);
        if matches!(outcome, Err(Failure::Key)) {
            // An unknown `kid` earns at most one refresh per interval.
            let fresh = self
                .jwks
                .keys_for_unknown_kid()
                .await
                .map_err(|_| Refusal::Unavailable)?;
            if !Arc::ptr_eq(&keys, &fresh) {
                outcome = jwt::verify_claims(token, &fresh, &self.policy, now);
            }
        }
        let (verified, claims) = outcome.map_err(|_| Refusal::Unauthorized)?;
        Ok(Principal {
            actor: oidc_actor(&verified.subject),
            role: role_of(claims.get("roles")),
            via: "oidc",
        })
    }
}

/// The highest role among the values of a `roles` claim (an array of strings, or one string of
/// space-separated values); an unrecognised value is ignored.
fn role_of(claim: Option<&Value>) -> Option<Role> {
    let values: Vec<&str> = match claim {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(text)) => text.split_whitespace().collect(),
        _ => Vec::new(),
    };
    values.into_iter().filter_map(Role::from_claim).max()
}

/// `oidc:<sub>` when the subject is a short (at most 100) visible-ASCII string, else `oidc-hash:` and
/// 16 hex digits of its digest: a row is never unreadable, a subject can never carry a control
/// character into a header or a log line, and a hashed actor can never equal a plain one (`oidc:` and
/// `oidc-hash:` differ at the fifth character).
fn oidc_actor(subject: &str) -> String {
    let plain =
        (1..=100).contains(&subject.len()) && subject.bytes().all(|b| (b'!'..=b'~').contains(&b));
    if plain {
        format!("oidc:{subject}")
    } else {
        let digest = format!("{:x}", Sha256::digest(subject.as_bytes()));
        format!("oidc-hash:{}", &digest[..16])
    }
}

/// The actor of the shared token in SSO mode.
pub const BREAK_GLASS_ACTOR: &str = "break-glass";

/// Whether a bearer value is to be verified as a token (and only as one).
pub fn is_token(value: &str) -> bool {
    jwt::is_jwt_shaped(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_highest_recognised_role_wins_and_other_applications_roles_grant_nothing() {
        assert_eq!(role_of(Some(&json!(["control:read"]))), Some(Role::Read));
        assert_eq!(
            role_of(Some(&json!(["x", "control:admin", "control:read"]))),
            Some(Role::Admin)
        );
        assert_eq!(
            role_of(Some(&json!("control:read control:key-issuer"))),
            Some(Role::KeyIssuer)
        );
        for none in [
            json!(["admin", "read", "control:ADMIN", "other:control:admin"]),
            json!([1, null, {"control:admin": true}]),
            json!(true),
            json!("control:admin,control:read"),
        ] {
            assert_eq!(role_of(Some(&none)), None, "{none}");
        }
        assert_eq!(role_of(None), None);
        assert!(Role::Admin > Role::KeyIssuer && Role::KeyIssuer > Role::Read);
    }

    #[test]
    fn the_actor_is_readable_or_a_digest_never_raw_text() {
        assert_eq!(oidc_actor("alice@example.com"), "oidc:alice@example.com");
        for odd in ["", "has space", "line\nbreak", "ünïcode", &"a".repeat(101)] {
            let actor = oidc_actor(odd);
            assert!(
                actor.starts_with("oidc-hash:") && actor.len() == 10 + 16,
                "{actor}"
            );
        }
        assert_ne!(oidc_actor("a b"), oidc_actor("a c"));
        // A subject that spells a hashed actor is still a plain one, and a different string.
        assert!(oidc_actor("sha256:0123456789abcdef").starts_with("oidc:sha256:"));
    }
}

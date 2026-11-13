//! Building the gateway's caller-key store from a validated configuration.
//!
//! Every caller key ends up as a SHA-256 digest: keys given as `secret`
//! references are read once, digested, and dropped, and keys given as `sha256`
//! are used as they are. The process therefore never retains a caller token.
//! Errors are returned, never printed-and-exited, and never contain a token.

use super::config::{read_secret, Issue, ServeConfig};
use crate::gateway::auth::{Identity, KeyStore};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Shortest caller token accepted, in characters. A configuration cannot choose
/// a weaker one; generate tokens with at least 128 bits of randomness.
pub const MIN_TOKEN_LEN: usize = 16;

/// Build the caller-key store.
///
/// With `server.allow_unauthenticated` the store is [`KeyStore::Open`], which is
/// what the gateway already does when no keys file is configured. Otherwise it
/// is a hashed, enforced store. `env` resolves `{ env = ... }` references, so a
/// caller (or test) decides what the environment is.
pub fn build_key_store(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<KeyStore, Vec<Issue>> {
    if config.server.allow_unauthenticated {
        return Ok(KeyStore::Open);
    }
    let mut issues = Vec::new();
    let mut identities: HashMap<String, Identity> = HashMap::new();
    let mut peers: HashMap<String, Identity> = HashMap::new();
    for (index, key) in config.keys.iter().enumerate() {
        let at = format!("keys[{index}]");
        if let Some(san) = &key.client_san {
            let identity = Identity {
                tenant: key.tenant.clone(),
                tier: key.tier,
                rpm: key.rpm,
                tpm: key.tpm,
                budget_usd: key.budget_usd,
                budget_window_secs: key.budget_window_secs,
                budget_allow_unpriced: key.budget_allow_unpriced,
            };
            if peers
                .insert(super::config::canonical_san(san), identity)
                .is_some()
            {
                issues.push(Issue {
                    path: at,
                    message: "has the same client_san as another key".into(),
                });
            }
            continue;
        }
        let digest = match (&key.secret, &key.sha256) {
            (Some(secret), None) => match read_secret(secret, env) {
                Ok(token) if token.chars().any(char::is_whitespace) => {
                    issues.push(Issue {
                        path: format!("{at}.secret"),
                        message: "must not contain whitespace".into(),
                    });
                    continue;
                }
                Ok(token) if token.chars().count() < MIN_TOKEN_LEN => {
                    issues.push(Issue {
                        path: format!("{at}.secret"),
                        message: format!("must be at least {MIN_TOKEN_LEN} characters"),
                    });
                    continue;
                }
                Ok(token) => format!("{:x}", Sha256::digest(token.as_bytes())),
                Err(message) => {
                    issues.push(Issue {
                        path: format!("{at}.secret"),
                        message,
                    });
                    continue;
                }
            },
            (None, Some(digest)) => digest.clone(),
            // Rejected by validation; unreachable for a loaded configuration.
            _ => continue,
        };
        let identity = Identity {
            tenant: key.tenant.clone(),
            tier: key.tier,
            rpm: key.rpm,
            tpm: key.tpm,
            budget_usd: key.budget_usd,
            budget_window_secs: key.budget_window_secs,
            budget_allow_unpriced: key.budget_allow_unpriced,
        };
        if identities.insert(digest, identity).is_some() {
            issues.push(Issue {
                path: at,
                message: "has the same value as another caller key".into(),
            });
        }
    }
    if issues.is_empty() {
        Ok(if peers.is_empty() {
            KeyStore::from_digests(identities)
        } else {
            KeyStore::mutual(identities, peers)
        })
    } else {
        Err(issues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::config::Format;
    use axum::http::{HeaderMap, HeaderValue};

    const TOKEN: &str = "ci-token-0123456789abcdef";

    fn config(extra_keys: &str) -> ServeConfig {
        let text = format!(
            r#"
            version = 1
            [providers.openai]
            kind = "openai"
            api_key = {{ env = "UPSTREAM_KEY" }}
            [[keys]]
            name = "ci"
            tenant = "platform"
            tier = 3
            rpm = 60
            budget_usd = 25.0
            budget_window_secs = 3600
            secret = {{ env = "CI_KEY" }}
            {extra_keys}
            "#
        );
        ServeConfig::parse(&text, Format::Toml).unwrap()
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    fn env_with(value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |name| (name == "CI_KEY").then(|| value.to_string())
    }

    #[test]
    fn a_secret_key_authenticates_with_the_identity_from_the_file() {
        let store = build_key_store(&config(""), &env_with(TOKEN)).unwrap();
        assert!(store.is_enforced());
        let caller = store.identify_caller(&bearer(TOKEN)).unwrap();
        assert_eq!(caller.identity.tenant, "platform");
        assert_eq!(caller.identity.tier, 3);
        assert_eq!(caller.identity.rpm, Some(60));
        assert_eq!(caller.identity.budget_usd, Some(25.0));
        assert_eq!(caller.identity.budget_window_secs, Some(3600));
        assert!(store.identify(&bearer("some-other-token-value")).is_err());
        assert!(store.identify(&HeaderMap::new()).is_err());
    }

    #[test]
    fn the_scope_matches_the_plaintext_store_for_the_same_token() {
        let from_file = build_key_store(&config(""), &env_with(TOKEN)).unwrap();
        let plain = KeyStore::enforced(HashMap::from([(
            TOKEN.to_string(),
            from_file.identify(&bearer(TOKEN)).unwrap(),
        )]));
        assert_eq!(
            from_file
                .identify_caller(&bearer(TOKEN))
                .unwrap()
                .credential_scope,
            plain
                .identify_caller(&bearer(TOKEN))
                .unwrap()
                .credential_scope
        );
    }

    #[test]
    fn a_digest_key_needs_no_secret_in_the_environment() {
        let digest = format!("{:x}", Sha256::digest(b"batch-token-0123456789"));
        let extra =
            format!("[[keys]]\nname = \"batch\"\ntenant = \"analytics\"\nsha256 = \"{digest}\"");
        let store = build_key_store(&config(&extra), &env_with(TOKEN)).unwrap();
        let caller = store.identify(&bearer("batch-token-0123456789")).unwrap();
        assert_eq!(caller.tenant, "analytics");
        assert_eq!(caller.tier, 0);
    }

    #[test]
    fn problems_are_reported_by_path_and_never_contain_a_token() {
        let unset = build_key_store(&config(""), &|_| None).err().unwrap();
        assert_eq!(unset[0].path, "keys[0].secret");
        assert!(unset[0].message.contains("CI_KEY is not set"));
        for (value, expected) in [
            ("short-token", "at least 16 characters"),
            ("has a space inside it 0123", "must not contain whitespace"),
        ] {
            let issues = build_key_store(&config(""), &env_with(value))
                .err()
                .unwrap();
            assert!(issues[0].message.contains(expected), "{issues:?}");
            assert!(!issues[0].message.contains(value), "{issues:?}");
        }
    }

    #[test]
    fn two_keys_with_the_same_token_are_refused() {
        let digest = format!("{:x}", Sha256::digest(TOKEN.as_bytes()));
        let extra = format!("[[keys]]\nname = \"copy\"\ntenant = \"other\"\nsha256 = \"{digest}\"");
        let issues = build_key_store(&config(&extra), &env_with(TOKEN))
            .err()
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0]
            .message
            .contains("same value as another caller key"));
    }

    #[test]
    fn an_explicitly_unauthenticated_file_gives_an_open_store() {
        let text = r#"
            version = 1
            [server]
            allow_unauthenticated = true
            [providers.openai]
            kind = "openai"
            api_key = { env = "UPSTREAM_KEY" }
        "#;
        let config = ServeConfig::parse(text, Format::Toml).unwrap();
        let store = build_key_store(&config, &|_| None).unwrap();
        assert!(!store.is_enforced());
    }
}

//! Building the gateway's caller-key store from a validated configuration.
//!
//! Every caller key ends up as a SHA-256 digest: keys given as `secret`
//! references are read once, digested, and dropped, and keys given as `sha256`
//! are used as they are. The store therefore holds only digests. A `secret`
//! given as `{ env = ... }` still leaves the token in the process environment;
//! a `{ file = ... }` or `sha256` key does not. Errors are returned, never
//! printed-and-exited, and never contain a token.

use super::config::{read_secret, Issue, ServeConfig};
use crate::gateway::auth::{Identity, KeyStore};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Shortest caller token accepted, in characters. A configuration cannot choose
/// a weaker one. 22 characters is 128 bits in the densest common encoding
/// (base64url), so it is the shortest length that can hold 128 bits of
/// randomness. Length is a floor, not a measure of randomness: generate tokens
/// with a tool (`openssl rand -hex 32`), not by hand.
pub const MIN_TOKEN_LEN: usize = 22;

/// Why `token` cannot serve as a bearer token, if it cannot. Shared by caller
/// keys and the admin token so the two rules cannot drift apart.
///
/// The gateway reads the `Authorization` header as visible ASCII, so a token
/// with anything else (a space, a byte-order mark left by an editor, a control
/// or non-ASCII character) could never match.
pub(crate) fn token_problem(token: &str) -> Option<String> {
    if !token.chars().all(|c| ('!'..='~').contains(&c)) {
        return Some(
            "must contain only visible ASCII characters \
             (no whitespace, control or non-ASCII characters)"
                .into(),
        );
    }
    (token.chars().count() < MIN_TOKEN_LEN)
        .then(|| format!("must be at least {MIN_TOKEN_LEN} characters"))
}

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
        // Validation refuses this combination for a parsed file; a configuration
        // assembled by hand must not turn configured keys into an open server.
        if !config.keys.is_empty() {
            return Err(vec![Issue {
                path: "keys".into(),
                message: "caller keys are configured but server.allow_unauthenticated is true"
                    .into(),
            }]);
        }
        return Ok(KeyStore::Open);
    }
    let mut issues = Vec::new();
    let mut identities: HashMap<String, Identity> = HashMap::new();
    let mut peers: HashMap<String, Identity> = HashMap::new();
    for (index, key) in config.keys.iter().enumerate() {
        let at = format!("keys[{index}]");
        if let Some(binding) = &key.jwt {
            let identity = Identity {
                tenant: key.tenant.clone(),
                tier: key.tier,
                rpm: key.rpm,
                tpm: key.tpm,
                budget_usd: key.budget_usd,
                budget_window_secs: key.budget_window_secs,
                budget_allow_unpriced: key.budget_allow_unpriced,
                allowed_models: key.allowed_models.clone(),
                key_name: key_name_for_cap(config, key),
                tenant_budget: tenant_budget(config, &key.tenant),
            };
            if peers
                .insert(jwt_workload(&binding.table, &binding.subject), identity)
                .is_some()
            {
                issues.push(Issue {
                    path: at,
                    message: "has the same jwt table and subject as another key".into(),
                });
            }
            continue;
        }
        if let Some(san) = &key.client_san {
            let identity = Identity {
                tenant: key.tenant.clone(),
                tier: key.tier,
                rpm: key.rpm,
                tpm: key.tpm,
                budget_usd: key.budget_usd,
                budget_window_secs: key.budget_window_secs,
                budget_allow_unpriced: key.budget_allow_unpriced,
                allowed_models: key.allowed_models.clone(),
                key_name: key_name_for_cap(config, key),
                tenant_budget: tenant_budget(config, &key.tenant),
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
                Ok(token) => match token_problem(&token) {
                    Some(message) => {
                        issues.push(Issue {
                            path: format!("{at}.secret"),
                            message,
                        });
                        continue;
                    }
                    None => format!("{:x}", Sha256::digest(token.as_bytes())),
                },
                Err(message) => {
                    issues.push(Issue {
                        path: format!("{at}.secret"),
                        message,
                    });
                    continue;
                }
            },
            (None, Some(digest)) => digest.clone(),
            // Rejected by validation for a parsed file; a configuration assembled
            // by hand must not silently drop a key.
            _ => {
                issues.push(Issue {
                    path: at,
                    message: "must set exactly one of secret and sha256".into(),
                });
                continue;
            }
        };
        let identity = Identity {
            tenant: key.tenant.clone(),
            tier: key.tier,
            rpm: key.rpm,
            tpm: key.tpm,
            budget_usd: key.budget_usd,
            budget_window_secs: key.budget_window_secs,
            budget_allow_unpriced: key.budget_allow_unpriced,
            allowed_models: key.allowed_models.clone(),
            key_name: key_name_for_cap(config, key),
            tenant_budget: tenant_budget(config, &key.tenant),
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

/// The credential scope of each configured key, with the key's name: what a
/// policy scope carries (the digest that scopes the credential's state) and what
/// a replica needs to name the key in a log line. A token key's scope is the
/// SHA-256 of its token; a certificate workload's is the SHA-256 of
/// `mtls:<name>`, in a space tokens cannot reach (see `KeyStore::identify_caller`).
///
/// Best effort: a key whose secret cannot be read is left out here, because
/// `build_key_store` reports it. The names are labels, never permissions.
pub fn key_names(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    config
        .keys
        .iter()
        .filter_map(|key| Some((credential_scope(key, env)?, key.name.clone())))
        .collect()
}

/// The key's name, only where a tenant table makes it the name of a counter: a rename is
/// otherwise not a change to anything a request is judged by (0012), and must not look like one.
fn key_name_for_cap(config: &ServeConfig, key: &super::config::GatewayKey) -> Option<String> {
    config
        .tenants
        .contains_key(&key.tenant)
        .then(|| key.name.clone())
}

/// The tenant-level budget of a key's tenant, if the file has one (note 0030).
fn tenant_budget(config: &ServeConfig, tenant: &str) -> Option<crate::gateway::auth::TenantBudget> {
    config
        .tenants
        .get(tenant)
        .map(|budget| crate::gateway::auth::TenantBudget {
            budget_usd: budget.budget_usd,
            budget_window_secs: budget.budget_window_secs,
            budget_allow_unpriced: budget.budget_allow_unpriced,
        })
}

/// `(credential scope, tags)` of each key that can be named (note 0029). Labels only.
pub fn key_tags(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, std::collections::BTreeMap<String, String>)> {
    config
        .keys
        .iter()
        .filter_map(|key| Some((credential_scope(key, env)?, key.tags.clone())))
        .collect()
}

/// The workload name a verified token is bound to, in the space the key store
/// keeps certificate workloads in (`dns:`/`uri:` names), under its own prefix: a
/// table name has no `:`, so two bindings can never produce the same name.
pub(crate) fn jwt_workload(table: &str, subject: &str) -> String {
    format!("jwt:{table}:{subject}")
}

fn credential_scope(
    key: &super::config::GatewayKey,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    Some(if let Some(binding) = &key.jwt {
        format!(
            "{:x}",
            Sha256::digest(format!(
                "mtls:{}",
                jwt_workload(&binding.table, &binding.subject)
            ))
        )
    } else if let Some(san) = &key.client_san {
        format!(
            "{:x}",
            Sha256::digest(format!("mtls:{}", super::config::canonical_san(san)))
        )
    } else {
        match (&key.secret, &key.sha256) {
            (Some(secret), None) => {
                let token = read_secret(secret, env).ok()?;
                format!("{:x}", Sha256::digest(token.as_bytes()))
            }
            (None, Some(digest)) => digest.clone(),
            _ => return None,
        }
    })
}

/// The network allowlist and expiry of each key that has one, by the key's credential scope.
/// A key whose secret cannot be read is left out here, and `build_key_store`
/// reports it (such a key cannot authenticate either, so nothing is left open).
pub fn allowlists(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> std::collections::HashMap<String, super::allowlist::Rule> {
    config
        .keys
        .iter()
        .filter_map(|key| {
            let nets: Option<std::sync::Arc<[super::allowlist::Cidr]>> =
                key.allowed_cidrs.as_ref().map(|cidrs| {
                    cidrs
                        .iter()
                        .filter_map(|text| super::allowlist::Cidr::parse(text).ok())
                        .collect::<Vec<_>>()
                        .into()
                });
            let expires_at = key.expires_at.as_ref().map(|text| {
                // Validated already; a value that somehow is not a time fails closed
                // (expired), never open.
                chrono::DateTime::parse_from_rfc3339(text).map_or(i64::MIN, |at| at.timestamp())
            });
            if nets.is_none() && expires_at.is_none() {
                return None;
            }
            Some((
                credential_scope(key, env)?,
                super::allowlist::Rule { nets, expires_at },
            ))
        })
        .collect()
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
        // A file holding only a digest key resolves without any environment.
        let text = format!(
            "version = 1\n[providers.openai]\nkind = \"openai\"\napi_key = {{ env = \"UPSTREAM_KEY\" }}\n{extra}"
        );
        let config = ServeConfig::parse(&text, Format::Toml).unwrap();
        let store = build_key_store(&config, &|_| None).unwrap();
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
            ("short-token", "at least 22 characters"),
            ("has a space inside it 0123", "visible ASCII"),
            // A byte-order mark survives `trim`; the shared secret reader refuses it.
            ("\u{feff}ci-token-0123456789abcdef", "not a single line"),
            ("caf\u{e9}-token-0123456789abcd", "visible ASCII"),
            // A control character is refused earlier, by the shared secret reader (note 0033).
            ("tab\tseparated-0123456789abc", "not a single line"),
        ] {
            let issues = build_key_store(&config(""), &env_with(value))
                .err()
                .unwrap();
            assert!(issues[0].message.contains(expected), "{issues:?}");
            assert!(!issues[0].message.contains(value), "{issues:?}");
        }
    }

    #[test]
    fn the_length_floor_is_exactly_twenty_two_characters() {
        // 22 characters is 128 bits in base64url, the shortest that can hold what
        // the notes advise; `openssl rand -hex 32` (64) clears it easily.
        let twenty_one = "abcdefghijklmnopqrstu";
        let twenty_two = "abcdefghijklmnopqrstuv";
        assert_eq!(twenty_one.chars().count(), 21);
        assert_eq!(twenty_two.chars().count(), 22);
        assert!(build_key_store(&config(""), &env_with(twenty_one)).is_err());
        let store = build_key_store(&config(""), &env_with(twenty_two)).unwrap();
        assert!(store.identify(&bearer(twenty_two)).is_ok());
    }

    #[test]
    fn the_names_table_resolves_what_the_store_reports_for_every_kind_of_key() {
        // A token key, a key held only as a digest, and a certificate workload:
        // each name must be found under the credential scope the store itself
        // reports for that caller, or a log line would name the wrong key or none.
        let digest = format!("{:x}", Sha256::digest(b"batch-token-0123456789"));
        let text = format!(
            r#"
            version = 1
            [server.tls]
            cert_file = "/etc/tls/server.pem"
            key_file = "/etc/tls/server.key"
            client_ca_file = "/etc/tls/clients.pem"
            [providers.openai]
            kind = "openai"
            api_key = {{ env = "UPSTREAM_KEY" }}
            [[keys]]
            name = "ci"
            tenant = "platform"
            secret = {{ env = "CI_KEY" }}
            [[keys]]
            name = "batch"
            tenant = "analytics"
            sha256 = "{digest}"
            [[keys]]
            name = "checkout-workload"
            tenant = "payments"
            client_san = "uri:spiffe://corp/payments/checkout"
            "#
        );
        let config = ServeConfig::parse(&text, Format::Toml).unwrap();
        let env = env_with(TOKEN);
        let store = build_key_store(&config, &env).unwrap();
        let names = crate::gateway::usage::KeyNames::new(key_names(&config, &env));

        let by_token = store.identify_caller(&bearer(TOKEN)).unwrap();
        assert_eq!(
            names.name(&by_token.credential_scope).as_deref(),
            Some("ci")
        );
        let by_digest = store
            .identify_caller(&bearer("batch-token-0123456789"))
            .unwrap();
        assert_eq!(
            names.name(&by_digest.credential_scope).as_deref(),
            Some("batch")
        );
        let mut peer = HeaderMap::new();
        peer.insert(
            crate::gateway::auth::PEER_SAN_HEADER,
            HeaderValue::from_static("uri:spiffe://corp/payments/checkout"),
        );
        let by_certificate = store.identify_caller(&peer).unwrap();
        assert_eq!(
            names.name(&by_certificate.credential_scope).as_deref(),
            Some("checkout-workload")
        );
        // A DNS name is matched case-insensitively (RFC 4343): the table must
        // find the key under the canonical form the store uses.
        let mixed = text.replace(
            "uri:spiffe://corp/payments/checkout",
            "dns:Checkout.Corp.Example",
        );
        let mixed_config = ServeConfig::parse(&mixed, Format::Toml).unwrap();
        let mixed_store = build_key_store(&mixed_config, &env).unwrap();
        let mixed_names = crate::gateway::usage::KeyNames::new(key_names(&mixed_config, &env));
        let mut dns = HeaderMap::new();
        dns.insert(
            crate::gateway::auth::PEER_SAN_HEADER,
            HeaderValue::from_static("dns:checkout.corp.example"),
        );
        let by_dns = mixed_store.identify_caller(&dns).unwrap();
        assert_eq!(
            mixed_names.name(&by_dns.credential_scope).as_deref(),
            Some("checkout-workload")
        );
        // Three keys, three scopes, and an unknown credential names nothing.
        assert_eq!(key_names(&config, &env).len(), 3);
        assert_eq!(names.name("not-a-credential-scope"), None);
    }

    #[test]
    fn a_key_whose_secret_cannot_be_read_is_left_out_of_the_names() {
        // build_key_store reports it; the table is best effort.
        let config = config("");
        assert!(key_names(&config, &|_| None).is_empty());
    }

    #[test]
    fn configured_keys_never_yield_an_open_store() {
        let mut open = config("");
        open.server.allow_unauthenticated = true;
        let issues = build_key_store(&open, &env_with(TOKEN)).err().unwrap();
        assert_eq!(issues[0].path, "keys");
        let mut neither = config("");
        neither.keys[0].secret = None;
        let issues = build_key_store(&neither, &env_with(TOKEN)).err().unwrap();
        assert_eq!(issues[0].path, "keys[0]");
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

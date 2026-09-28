//! Where a provider key comes from when the process environment does not carry one.
//!
//! llmshim has always read provider keys from the environment, with `~/.llmshim/config.toml`
//! copied into it first ([`crate::env::load_all`]). That is right for a proxy an operator
//! starts from a shell, and wrong for an embedder that holds a saved login of its own: it would
//! have to mutate the process environment to hand a key over, which is `unsafe` under the 2024
//! edition, leaks the secret to every child process the embedder later spawns, and makes the
//! credential a property of the process rather than of the call.
//!
//! So this is the seam. An embedder implements [`StoredCredentials`], keyed by **provider name**
//! — never by an environment-variable spelling — and [`crate::router::Router::from_credentials_without_catalog_refresh`]
//! asks it for each name in [`CREDENTIALED_PROVIDERS`]. The caller therefore needs to know no
//! provider-specific fact at all, which is the point: a harness that had to write
//! `if provider == "anthropic"` to store a key has moved the provider abstraction up into
//! itself.
//!
//! **The environment still wins.** A stored login is a fallback, not an override, so exporting a
//! key for one run does what it has always done and a saved login never silently shadows it.
//!
//! A stored login may also carry an address ([`StoredCredentials::base_url_for`]). **A stored
//! address goes only with the stored key; an exported key is never sent to it.**

/// The providers whose credential is a single secret this seam can supply, in the order the
/// router asks for them.
///
/// Two registrations are deliberately outside it. `chatgpt` is an OAuth login with refresh-token
/// rotation, not a secret, and is registered from its own auth file; the self-hosted `vllm` and
/// `sglang` entries are selected by a base URL rather than by a key, and their key is optional.
/// Neither is something an embedder can supply as a string, so neither is asked for here.
pub const CREDENTIALED_PROVIDERS: [&str; 5] =
    ["openai", "anthropic", "gemini", "xai", "openrouter"];

/// A saved login an embedder holds, asked by provider name.
pub trait StoredCredentials {
    /// The secret for `provider`, or `None` when the embedder holds none. `provider` is always
    /// one of [`CREDENTIALED_PROVIDERS`].
    fn secret_for(&self, provider: &str) -> Option<String>;

    /// Where `provider`'s stored key is sent, when the embedder stored an address for it. `None`
    /// is the provider's own address. It is asked only for a key that came from `secret_for`, so
    /// a stored address goes only with the stored key and an exported key is never sent to it.
    fn base_url_for(&self, _provider: &str) -> Option<String> {
        None
    }
}

/// The historical behaviour: every key comes from the environment and nothing is stored.
pub struct EnvironmentOnly;

impl StoredCredentials for EnvironmentOnly {
    fn secret_for(&self, _provider: &str) -> Option<String> {
        None
    }
}

/// Which of the two sources supplied the key that would register `provider`.
pub(crate) enum KeySource {
    /// `provider`'s own environment exported a key.
    Environment(String),
    /// The embedder's saved login supplied the key.
    Stored(String),
}

impl KeySource {
    pub(crate) fn into_key(self) -> String {
        match self {
            KeySource::Environment(key) | KeySource::Stored(key) => key,
        }
    }
}

/// The key to register `provider` with, and where it came from, or `None` to leave it
/// unregistered.
///
/// `environment` is passed in rather than read here so the precedence is testable without
/// mutating the process environment — which two tests running at once cannot do safely.
/// An empty variable counts as unset: a provider registered with an empty key answers every
/// request with a 401, and worse, it would shadow a stored login that would have worked.
///
/// The source is an enum, not a bool: whether a key came from the environment decides whether
/// the embedder's stored address may be used, and that rule reads as a match at the call site.
pub(crate) fn key_source(
    provider: &str,
    environment: &dyn Fn(&str) -> Option<String>,
    stored: &dyn StoredCredentials,
) -> Option<KeySource> {
    if let Some(key) = environment(&format!("{}_API_KEY", provider.to_ascii_uppercase()))
        .filter(|key| !key.trim().is_empty())
    {
        return Some(KeySource::Environment(key));
    }
    stored
        .secret_for(provider)
        .filter(|key| !key.trim().is_empty())
        .map(KeySource::Stored)
}

/// The key to register `provider` with, or `None` to leave it unregistered.
///
/// Kept as the historical, source-erasing form for the precedence tests; the router asks
/// [`key_source`] because it must tell an exported key from a stored one.
#[cfg(test)]
pub(crate) fn key_for(
    provider: &str,
    environment: &dyn Fn(&str) -> Option<String>,
    stored: &dyn StoredCredentials,
) -> Option<String> {
    key_source(provider, environment, stored).map(KeySource::into_key)
}

/// The stored address to send a stored key to, or `None` when the stored value is not a
/// plain `http`/`https` URL.
///
/// A stored address is the owner saying "the key goes here and nowhere else". Anything
/// unusual — a user name or password in it, a query, a fragment, no host at all, or a scheme
/// other than http/https — is refused rather than quietly replaced by the provider's own
/// address, because the key was minted for that custom host and the default host was never
/// meant to see it.
pub(crate) fn stored_base_url(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw).ok()?;
    let plain = matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    plain.then(|| raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Saved(HashMap<&'static str, &'static str>);

    impl StoredCredentials for Saved {
        fn secret_for(&self, provider: &str) -> Option<String> {
            self.0.get(provider).map(|secret| secret.to_string())
        }
    }

    /// The precedence is the whole contract: an exported key wins, a stored login fills the gap,
    /// and an empty variable is not a key — it used to register a provider that could only 401,
    /// and with a saved login present it would have hidden one that works.
    #[test]
    fn the_environment_wins_and_an_empty_variable_is_not_a_key() {
        let stored = Saved(HashMap::from([
            ("openai", "stored-openai"),
            ("openrouter", "stored-openrouter"),
        ]));
        let environment = |name: &str| match name {
            "OPENAI_API_KEY" => Some("exported-openai".to_string()),
            "OPENROUTER_API_KEY" => Some("   ".to_string()),
            "ANTHROPIC_API_KEY" => Some("exported-anthropic".to_string()),
            _ => None,
        };
        assert_eq!(
            key_for("openai", &environment, &stored).as_deref(),
            Some("exported-openai")
        );
        assert_eq!(
            key_for("openrouter", &environment, &stored).as_deref(),
            Some("stored-openrouter")
        );
        assert_eq!(
            key_for("anthropic", &environment, &stored).as_deref(),
            Some("exported-anthropic")
        );
        assert_eq!(key_for("gemini", &environment, &stored), None);
    }

    /// The embedder is asked by provider name and never by an environment-variable spelling, so
    /// nothing above this crate has to know one.
    #[test]
    fn a_stored_login_is_asked_for_by_provider_name() {
        let asked = std::cell::RefCell::new(Vec::new());
        struct Recording<'a>(&'a std::cell::RefCell<Vec<String>>);
        impl StoredCredentials for Recording<'_> {
            fn secret_for(&self, provider: &str) -> Option<String> {
                self.0.borrow_mut().push(provider.to_string());
                None
            }
        }
        for provider in CREDENTIALED_PROVIDERS {
            key_for(provider, &|_| None, &Recording(&asked));
        }
        assert_eq!(
            asked.into_inner(),
            vec!["openai", "anthropic", "gemini", "xai", "openrouter"]
        );
    }

    /// `stored_base_url` is the gate between a stored address and the wire. Every near miss
    /// — a wrong scheme, credentials in the URL, a query, a fragment, no host, or plain
    /// garbage — must be refused, because the fallback is the provider's own host, which the
    /// key was never meant for.
    #[test]
    fn only_a_plain_http_url_is_a_stored_address() {
        let cases = [
            ("https://api.example.com/v1", true),
            ("http://localhost:8000/v1", true),
            ("https://api.example.com", true),
            ("HTTPS://API.EXAMPLE.COM/v1", true),
            ("ftp://api.example.com/v1", false),
            ("mailto:owner@example.com", false),
            ("file:///etc/hosts", false),
            ("https://user:pass@api.example.com/v1", false),
            ("https://user@api.example.com/v1", false),
            ("https://api.example.com/v1?token=1", false),
            ("https://api.example.com/v1#frag", false),
            ("https://", false),
            ("api.example.com/v1", false),
            ("not a url", false),
            ("", false),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                stored_base_url(raw).is_some(),
                expected,
                "stored_base_url({raw:?})"
            );
        }
    }
}

//! A stored key may carry its own address, and that address goes only with
//! that key. These tests sit at the boundary an embedder sees: build a router
//! from stored credentials, ask the registered provider to prepare the request
//! it would send, and read the URL and auth header that came out. Nothing is
//! sent.
//!
//! One provider per wire kind is covered — OpenAI (Responses), Anthropic
//! (Messages) and Gemini (Generate Content) — because the address is appended
//! in a provider's own way and the key is carried in three different places.

use llmshim::credentials::StoredCredentials;
use llmshim::provider::ProviderRequest;
use llmshim::router::Router;

struct Saved {
    secrets: Vec<(&'static str, &'static str)>,
    addresses: Vec<(&'static str, &'static str)>,
}

impl Saved {
    fn new(
        secrets: &[(&'static str, &'static str)],
        addresses: &[(&'static str, &'static str)],
    ) -> Self {
        Self {
            secrets: secrets.to_vec(),
            addresses: addresses.to_vec(),
        }
    }
}

impl StoredCredentials for Saved {
    fn secret_for(&self, provider: &str) -> Option<String> {
        self.secrets
            .iter()
            .find(|(name, _)| *name == provider)
            .map(|(_, secret)| (*secret).to_string())
    }

    fn base_url_for(&self, provider: &str) -> Option<String> {
        self.addresses
            .iter()
            .find(|(name, _)| *name == provider)
            .map(|(_, address)| (*address).to_string())
    }
}

fn no_environment(_name: &str) -> Option<String> {
    None
}

fn request(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}],
    })
}

fn header<'a>(request: &'a ProviderRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn prepared_by(router: &Router, name: &str, model: &str) -> ProviderRequest {
    let provider = router
        .get(name)
        .unwrap_or_else(|error| panic!("{name} should be registered: {error}"));
    provider
        .transform_request(model, &request(model))
        .unwrap_or_else(|error| panic!("{name} should prepare a request: {error}"))
}

/// A stored key with a stored address is sent to that address, with that key,
/// on every wire kind.
#[test]
fn a_stored_key_goes_to_its_stored_address() {
    let stored = Saved::new(
        &[
            ("openai", "stored-openai"),
            ("anthropic", "stored-anthropic"),
            ("gemini", "stored-gemini"),
        ],
        &[
            ("openai", "https://gateway.example/openai/v1"),
            ("anthropic", "https://gateway.example/anthropic/v1"),
            ("gemini", "https://gateway.example/gemini/v1beta"),
        ],
    );
    let router = Router::from_credentials_with_env(&stored, &no_environment);

    let openai = prepared_by(&router, "openai", "gpt-6-astra");
    assert_eq!(openai.url, "https://gateway.example/openai/v1/responses");
    assert_eq!(
        header(&openai, "Authorization"),
        Some("Bearer stored-openai")
    );

    let anthropic = prepared_by(&router, "anthropic", "claude-sonnet-5");
    assert_eq!(
        anthropic.url,
        "https://gateway.example/anthropic/v1/messages"
    );
    assert_eq!(header(&anthropic, "x-api-key"), Some("stored-anthropic"));

    let gemini = prepared_by(&router, "gemini", "gemini-3.8-flash");
    assert!(
        gemini
            .url
            .starts_with("https://gateway.example/gemini/v1beta/models/gemini-3.8-flash:"),
        "{}",
        gemini.url
    );
    assert!(gemini.url.contains("key=stored-gemini"), "{}", gemini.url);
}

/// An exported key is never sent to the stored address: it goes to the
/// provider's own address, and neither the stored key nor the stored address is
/// touched.
#[test]
fn an_exported_key_ignores_the_stored_address() {
    let stored = Saved::new(
        &[
            ("openai", "stored-openai"),
            ("anthropic", "stored-anthropic"),
            ("gemini", "stored-gemini"),
        ],
        &[
            ("openai", "https://gateway.example/openai/v1"),
            ("anthropic", "https://gateway.example/anthropic/v1"),
            ("gemini", "https://gateway.example/gemini/v1beta"),
        ],
    );
    let environment = |name: &str| match name {
        "OPENAI_API_KEY" => Some("exported-openai".to_string()),
        "ANTHROPIC_API_KEY" => Some("exported-anthropic".to_string()),
        "GEMINI_API_KEY" => Some("exported-gemini".to_string()),
        _ => None,
    };
    let router = Router::from_credentials_with_env(&stored, &environment);

    let openai = prepared_by(&router, "openai", "gpt-6-astra");
    assert_eq!(openai.url, "https://api.openai.com/v1/responses");
    assert_eq!(
        header(&openai, "Authorization"),
        Some("Bearer exported-openai")
    );

    let anthropic = prepared_by(&router, "anthropic", "claude-sonnet-5");
    assert_eq!(anthropic.url, "https://api.anthropic.com/v1/messages");
    assert_eq!(header(&anthropic, "x-api-key"), Some("exported-anthropic"));

    let gemini = prepared_by(&router, "gemini", "gemini-3.8-flash");
    assert!(
        gemini.url.starts_with(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:"
        ),
        "{}",
        gemini.url
    );
    assert!(gemini.url.contains("key=exported-gemini"), "{}", gemini.url);
    assert!(!gemini.url.contains("stored-gemini"), "{}", gemini.url);
}

/// A stored address that is not a plain http/https URL registers nothing: the
/// key was minted for a custom host, so the provider's own host must not be
/// used as a fallback either.
#[test]
fn an_invalid_stored_address_registers_nothing() {
    let stored = Saved::new(
        &[
            ("openai", "stored-openai"),
            ("anthropic", "stored-anthropic"),
            ("gemini", "stored-gemini"),
        ],
        &[
            ("openai", "https://user:pass@gateway.example/v1"),
            ("anthropic", "gateway.example/v1"),
            ("gemini", "ftp://gateway.example/v1beta"),
        ],
    );
    let router = Router::from_credentials_with_env(&stored, &no_environment);

    for provider in ["openai", "anthropic", "gemini"] {
        assert!(
            router.get(provider).is_err(),
            "{provider} must not be registered for an invalid stored address"
        );
    }
}

/// A stored key with no stored address behaves exactly as before: the
/// provider's own address.
#[test]
fn a_stored_key_with_no_address_uses_the_provider_address() {
    let stored = Saved::new(
        &[
            ("openai", "stored-openai"),
            ("anthropic", "stored-anthropic"),
            ("gemini", "stored-gemini"),
        ],
        &[],
    );
    let router = Router::from_credentials_with_env(&stored, &no_environment);

    let openai = prepared_by(&router, "openai", "gpt-6-astra");
    assert_eq!(openai.url, "https://api.openai.com/v1/responses");
    assert_eq!(
        header(&openai, "Authorization"),
        Some("Bearer stored-openai")
    );

    let anthropic = prepared_by(&router, "anthropic", "claude-sonnet-5");
    assert_eq!(anthropic.url, "https://api.anthropic.com/v1/messages");
    assert_eq!(header(&anthropic, "x-api-key"), Some("stored-anthropic"));

    let gemini = prepared_by(&router, "gemini", "gemini-3.8-flash");
    assert!(
        gemini
            .url
            .starts_with("https://generativelanguage.googleapis.com/v1beta/models/"),
        "{}",
        gemini.url
    );
    assert!(gemini.url.contains("key=stored-gemini"), "{}", gemini.url);
}

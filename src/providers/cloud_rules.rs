//! The rules the cloud transports (Bedrock, Vertex, Azure) apply to the values
//! that name where they connect, kept in one place so that the transports and the
//! `serve` configuration file cannot disagree (0020). A value this module
//! refuses would fail every request, so the file refuses it at `--check` instead.
//!
//! The module has no dependency on a transport feature: a `serve` build without
//! the transports still validates a file that names them (and then refuses to
//! start, naming the feature to rebuild with).

// Used by the transports when their features are on and by the `serve`
// configuration; a default build uses neither.
#![cfg_attr(
    not(any(
        feature = "serve",
        feature = "bedrock",
        feature = "vertex",
        feature = "azure"
    )),
    allow(dead_code)
)]

/// Why a Bedrock region cannot be used, if it cannot: lower-case letters, digits
/// and hyphens, and a partition the transport supports. The isolated partitions
/// (and the European Sovereign Cloud) are not supported.
pub(crate) fn bedrock_region(region: &str) -> Result<(), &'static str> {
    if region.is_empty()
        || !region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("Bedrock region must contain only lowercase letters, digits and hyphens");
    }
    for (prefix, name) in [
        ("us-iso-", "aws-iso"),
        ("us-isob-", "aws-iso-b"),
        ("eu-isoe-", "aws-iso-e"),
        ("us-isof-", "aws-iso-f"),
        ("eusc-", "aws-eusc"),
    ] {
        if region.starts_with(prefix) {
            return Err(name);
        }
    }
    Ok(())
}

/// A Vertex project or location: 1 to 128 lower-case letters, digits and hyphens.
pub(crate) fn vertex_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 128
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The root an Azure endpoint resolves to (`<origin>/openai/v1`), or `None` when
/// the endpoint is not an https resource origin or its `/openai/v1` base without
/// credentials, query or fragment.
pub(crate) fn azure_root(endpoint: &str) -> Option<String> {
    let url = reqwest::Url::parse(endpoint).ok()?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path().trim_end_matches('/'), "" | "/openai/v1")
    {
        return None;
    }
    Some(format!(
        "{}/openai/v1",
        url.as_str()
            .trim_end_matches('/')
            .trim_end_matches("/openai/v1")
    ))
}

/// An Azure deployment name and the catalog model it serves.
pub(crate) fn azure_deployment(deployment: &str, model: &str) -> bool {
    !deployment.is_empty()
        && deployment.len() <= 256
        && !model.trim().is_empty()
        && deployment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The most a service-account file may hold.
pub(crate) const SERVICE_ACCOUNT_MAX_BYTES: usize = 1024 * 1024;
/// The only token endpoint a service account may name.
pub(crate) const GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// Why a service-account file's content cannot be used, if it cannot: the type,
/// a client email, the Google token endpoint and a PKCS#8 RSA key. The content is
/// never echoed.
#[cfg(any(feature = "serve", feature = "vertex"))]
pub(crate) fn service_account_problem(bytes: &[u8]) -> Option<&'static str> {
    #[derive(serde::Deserialize)]
    struct Account {
        #[serde(rename = "type")]
        kind: String,
        client_email: String,
        private_key: String,
        token_uri: String,
    }
    use base64::Engine as _;
    if bytes.len() > SERVICE_ACCOUNT_MAX_BYTES {
        return Some("is larger than a service-account file can be");
    }
    let Ok(account) = serde_json::from_slice::<Account>(bytes) else {
        return Some("is not a service-account JSON file");
    };
    if account.kind != "service_account" {
        return Some("is not of type service_account");
    }
    if account.client_email.is_empty() {
        return Some("has no client_email");
    }
    if account.token_uri != GOOGLE_TOKEN_URI {
        return Some("names a token endpoint other than Google's");
    }
    let key = account
        .private_key
        .strip_prefix("-----BEGIN PRIVATE KEY-----")
        .and_then(|s| s.trim().strip_suffix("-----END PRIVATE KEY-----"))
        .and_then(|pem| {
            base64::engine::general_purpose::STANDARD
                .decode(pem.split_whitespace().collect::<String>())
                .ok()
        })
        .and_then(|der| ring::signature::RsaKeyPair::from_pkcs8(&der).ok());
    key.is_none().then_some("has no usable private key")
}

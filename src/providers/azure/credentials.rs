//! Explicit key or Entra source. No environment, default chain or metadata lookup exists.
use super::{AzureError, AzureErrorKind};
use crate::{
    error::Result,
    providers::token_cache::{BearerToken, CacheError, TokenCache},
};
use reqwest::header::HeaderValue;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Audience {
    CognitiveServices,
    Foundry,
}
impl Audience {
    pub fn scope(self) -> &'static str {
        match self {
            Self::CognitiveServices => "https://cognitiveservices.azure.com/.default",
            Self::Foundry => "https://ai.azure.com/.default",
        }
    }
}
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum CredentialError {
    InvalidBinding,
    Rejected,
    Unavailable,
    Timeout,
    InvalidToken,
}
impl CredentialError {
    pub(super) fn into_shim(self) -> crate::error::ShimError {
        let (status, code) = match self {
            Self::InvalidBinding => (401, "InvalidBinding"),
            Self::Rejected => (401, "TokenRejected"),
            Self::Unavailable => (503, "TokenUnavailable"),
            Self::Timeout => (503, "TokenTimeout"),
            Self::InvalidToken => (401, "InvalidToken"),
        };
        let mut error = AzureError::service(status, code);
        error.kind = AzureErrorKind::Credentials;
        error.into_shim(None)
    }
}
pub type TokenFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<AccessToken, CredentialError>> + Send + 'a>>;
/// Implementors resolve exactly the selected identity for the supplied explicit audience.
pub trait TokenSource: Send + Sync {
    fn token(&self, audience: Audience) -> TokenFuture<'_>;
}
#[derive(Clone)]
#[non_exhaustive]
pub struct AccessToken {
    token: BearerToken,
}
impl AccessToken {
    pub fn new(value: &str, lifetime: Duration) -> std::result::Result<Self, CredentialError> {
        Ok(Self {
            token: BearerToken::new(value, lifetime).ok_or(CredentialError::InvalidToken)?,
        })
    }
}
#[derive(Clone)]
enum Kind {
    Key(HeaderValue),
    Entra {
        audience: Audience,
        cache: TokenCache<CredentialError>,
    },
}
/// Construction performs no credential I/O. Keep the non-secret binding stable across replicas.
#[derive(Clone)]
#[non_exhaustive]
pub struct AzureIdentity {
    binding: String,
    kind: Kind,
}
impl AzureIdentity {
    pub fn api_key(binding: &str, key: &str) -> Result<Self> {
        Self::validate_binding(binding)?;
        let mut value =
            HeaderValue::from_str(key).map_err(|_| CredentialError::InvalidToken.into_shim())?;
        if key.is_empty() || key.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(CredentialError::InvalidToken.into_shim());
        }
        value.set_sensitive(true);
        Ok(Self {
            binding: binding.into(),
            kind: Kind::Key(value),
        })
    }
    pub fn entra(binding: &str, audience: Audience, source: Arc<dyn TokenSource>) -> Result<Self> {
        Self::validate_binding(binding)?;
        let cache = TokenCache::new(move || {
            let source = source.clone();
            Box::pin(async move { source.token(audience).await.map(|token| token.token) })
        });
        Ok(Self {
            binding: binding.into(),
            kind: Kind::Entra { audience, cache },
        })
    }
    fn validate_binding(binding: &str) -> Result<()> {
        if binding.trim().is_empty() {
            Err(CredentialError::InvalidBinding.into_shim())
        } else {
            Ok(())
        }
    }
    pub(super) fn binding(&self) -> String {
        match &self.kind {
            Kind::Key(_) => format!("key:{}", self.binding),
            Kind::Entra { audience, .. } => format!("{}:{}", audience.scope(), self.binding),
        }
    }
    pub(super) async fn authorize(&self, request: &mut reqwest::Request) -> Result<()> {
        request.headers_mut().remove("api-key");
        request.headers_mut().remove(reqwest::header::AUTHORIZATION);
        let (name, header) = match &self.kind {
            Kind::Key(key) => ("api-key", key.clone()),
            Kind::Entra { cache, .. } => (
                "authorization",
                cache.header().await.map_err(|error| {
                    match error {
                        CacheError::Source(error) => error,
                        CacheError::Timeout => CredentialError::Timeout,
                        CacheError::InvalidToken => CredentialError::InvalidToken,
                    }
                    .into_shim()
                })?,
            ),
        };
        request.headers_mut().insert(name, header);
        Ok(())
    }
    pub(super) async fn reject(&self, sent: &HeaderValue) -> bool {
        if let Kind::Entra { cache, .. } = &self.kind {
            cache.reject(sent).await;
            true
        } else {
            false
        }
    }
}

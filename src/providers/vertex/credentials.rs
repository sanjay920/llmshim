//! Closed identity selection and a clone-shared, single-flight refresh cache.
use super::{VertexError, VertexErrorKind};
use crate::error::Result;
use crate::providers::token_cache::{BearerToken, CacheError, TokenCache};
use reqwest::header::HeaderValue;
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration};

pub type TokenFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<AccessToken, CredentialError>> + Send + 'a>>;

/// Implementations resolve only the caller's selected identity. No ambient fallback is added.
pub trait TokenSource: Send + Sync {
    fn token(&self) -> TokenFuture<'_>;
}
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum CredentialError {
    InvalidBinding,
    FileUnavailable,
    InvalidServiceAccount,
    Rejected,
    Unavailable,
    Timeout,
    InvalidToken,
}
impl CredentialError {
    pub(super) fn into_shim(self) -> crate::error::ShimError {
        let (code, status) = match self {
            Self::InvalidBinding => ("invalid_binding", 401),
            Self::FileUnavailable => ("identity_file_unavailable", 401),
            Self::InvalidServiceAccount => ("invalid_service_account", 401),
            Self::Rejected => ("token_rejected", 401),
            Self::Unavailable => ("token_service_unavailable", 503),
            Self::Timeout => ("token_timeout", 503),
            Self::InvalidToken => ("invalid_token", 401),
        };
        let mut error = VertexError::service(status, code);
        error.kind = VertexErrorKind::Credentials;
        error.into_shim(None)
    }
}
/// An in-memory token with an absolute monotonic expiry; Debug intentionally is unavailable.
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
#[non_exhaustive]
pub struct GoogleIdentity {
    pub(super) project: String,
    pub(super) region: String,
    pub(super) binding: String,
    cache: TokenCache<CredentialError>,
}
impl GoogleIdentity {
    pub fn service_account(project: &str, region: &str, binding: &str, file: PathBuf) -> Self {
        Self::token_source(
            project,
            region,
            binding,
            Arc::new(super::ServiceAccount::new(file)),
        )
    }
    /// The binding is a stable, non-secret identity reference used to isolate replay.
    pub fn token_source(
        project: &str,
        region: &str,
        binding: &str,
        source: Arc<dyn TokenSource>,
    ) -> Self {
        Self {
            project: project.into(),
            region: region.into(),
            binding: binding.into(),
            cache: TokenCache::new(move || {
                let source = source.clone();
                Box::pin(async move { source.token().await.map(|token| token.token) })
            }),
        }
    }
    pub(super) async fn authorize(&self, request: &mut reqwest::Request) -> Result<()> {
        if self.binding.trim().is_empty() {
            return Err(CredentialError::InvalidBinding.into_shim());
        }
        let header = self.cache.header().await.map_err(|error| {
            match error {
                CacheError::Source(error) => error,
                CacheError::Timeout => CredentialError::Timeout,
                CacheError::InvalidToken => CredentialError::InvalidToken,
            }
            .into_shim()
        })?;
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, header);
        Ok(())
    }
    pub(super) async fn reject(&self, sent: &HeaderValue) {
        self.cache.reject(sent).await;
    }
}

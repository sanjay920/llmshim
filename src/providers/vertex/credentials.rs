//! Closed identity selection and a clone-shared, single-flight refresh cache.
use super::{VertexError, VertexErrorKind};
use crate::error::Result;
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
    header: HeaderValue,
    expires: tokio::time::Instant,
    refresh: tokio::time::Instant,
}
impl AccessToken {
    pub fn new(value: &str, lifetime: Duration) -> std::result::Result<Self, CredentialError> {
        if !value.is_ascii()
            || value.is_empty()
            || lifetime.is_zero()
            || value.bytes().any(|b| b.is_ascii_whitespace())
        {
            return Err(CredentialError::InvalidToken);
        }
        let mut header = HeaderValue::from_str(&format!("Bearer {value}"))
            .map_err(|_| CredentialError::InvalidToken)?;
        header.set_sensitive(true);
        let now = tokio::time::Instant::now();
        let expires = now
            .checked_add(lifetime)
            .ok_or(CredentialError::InvalidToken)?;
        let margin = (lifetime / 4).min(Duration::from_secs(30));
        Ok(Self {
            header,
            expires,
            refresh: expires - margin,
        })
    }
}
#[derive(Clone)]
#[non_exhaustive]
pub struct GoogleIdentity {
    pub(super) project: String,
    pub(super) region: String,
    pub(super) binding: String,
    source: Arc<dyn TokenSource>,
    cache: Arc<tokio::sync::RwLock<Option<AccessToken>>>,
    refresh: Arc<tokio::sync::Mutex<Option<tokio::time::Instant>>>,
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
            source,
            cache: Default::default(),
            refresh: Default::default(),
        }
    }
    pub(super) async fn authorize(&self, request: &mut reqwest::Request) -> Result<()> {
        if self.binding.trim().is_empty() {
            return Err(CredentialError::InvalidBinding.into_shim());
        }
        let cached = self.cache.read().await.clone();
        let token = if let Some(token) = cached.filter(Self::usable) {
            if tokio::time::Instant::now() >= token.refresh {
                if let Ok(mut refresh) = self.refresh.clone().try_lock_owned() {
                    let now = tokio::time::Instant::now();
                    if refresh.is_none_or(|retry_at| now >= retry_at) {
                        // Readers, including this caller, never await source I/O with a live token.
                        *refresh = now.checked_add(Duration::from_secs(1));
                        let identity = self.clone();
                        tokio::spawn(async move {
                            let _ = identity.load(&mut refresh).await;
                        });
                    }
                }
            }
            token
        } else {
            let mut refresh = self.refresh.lock().await;
            let cached = self.cache.read().await.clone();
            match cached.filter(Self::usable) {
                Some(token) => token,
                // Expiry or rejection bypasses the early-refresh cooldown.
                None => self
                    .load(&mut refresh)
                    .await
                    .map_err(CredentialError::into_shim)?,
            }
        };
        request
            .headers_mut()
            .insert(reqwest::header::AUTHORIZATION, token.header);
        Ok(())
    }

    fn usable(token: &AccessToken) -> bool {
        tokio::time::Instant::now()
            .checked_add(Duration::from_secs(1))
            .is_some_and(|send| token.expires > send)
    }

    async fn load(
        &self,
        retry_at: &mut Option<tokio::time::Instant>,
    ) -> std::result::Result<AccessToken, CredentialError> {
        let result = tokio::time::timeout(Duration::from_secs(30), self.source.token())
            .await
            .unwrap_or(Err(CredentialError::Timeout))
            .and_then(|token| {
                if Self::usable(&token) {
                    Ok(token)
                } else {
                    Err(CredentialError::InvalidToken)
                }
            });
        match result {
            Ok(token) => {
                *self.cache.write().await = Some(token.clone());
                *retry_at = None;
                Ok(token)
            }
            Err(error) => {
                // Keep any live token; throttle failed early refreshes without delaying readers.
                *retry_at = tokio::time::Instant::now().checked_add(Duration::from_secs(1));
                Err(error)
            }
        }
    }
    pub(super) async fn reject(&self, sent: &HeaderValue) {
        let mut cache = self.cache.write().await;
        // A delayed rejection of an old token must not evict a newer concurrent refresh.
        if cache.as_ref().is_some_and(|token| token.header == *sent) {
            *cache = None;
        }
    }
}

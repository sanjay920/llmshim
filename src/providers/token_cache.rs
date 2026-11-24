//! Shared bearer refresh: live readers never wait for source I/O or failed-refresh cooldowns.
use reqwest::header::HeaderValue;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, RwLock},
    time::Instant,
};

#[derive(Clone)]
pub(super) struct BearerToken {
    header: HeaderValue,
    expires: Instant,
    refresh: Instant,
}
impl BearerToken {
    pub(super) fn new(value: &str, lifetime: Duration) -> Option<Self> {
        if !value.is_ascii()
            || value.is_empty()
            || lifetime.is_zero()
            || value.bytes().any(|b| b.is_ascii_whitespace())
        {
            return None;
        }
        let mut header = HeaderValue::from_str(&format!("Bearer {value}")).ok()?;
        header.set_sensitive(true);
        let now = Instant::now();
        let expires = now.checked_add(lifetime)?;
        Some(Self {
            header,
            expires,
            refresh: expires - (lifetime / 4).min(Duration::from_secs(30)),
        })
    }
    fn usable(&self) -> bool {
        Instant::now()
            .checked_add(Duration::from_secs(1))
            .is_some_and(|send| self.expires > send)
    }
}
pub(super) enum CacheError<E> {
    Source(E),
    Timeout,
    InvalidToken,
}
type LoadFuture<E> = Pin<Box<dyn Future<Output = Result<BearerToken, E>> + Send>>;
struct Inner<E> {
    cached: RwLock<Option<BearerToken>>,
    refresh: Arc<Mutex<Option<Instant>>>,
    source: Box<dyn Fn() -> LoadFuture<E> + Send + Sync>,
}
#[derive(Clone)]
pub(super) struct TokenCache<E> {
    inner: Arc<Inner<E>>,
}
impl<E: Clone + Send + 'static> TokenCache<E> {
    pub(super) fn new(source: impl Fn() -> LoadFuture<E> + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(Inner {
                cached: Default::default(),
                refresh: Default::default(),
                source: Box::new(source),
            }),
        }
    }
    pub(super) async fn header(&self) -> Result<HeaderValue, CacheError<E>> {
        let cached = self.inner.cached.read().await;
        if let Some(token) = cached.as_ref().filter(|token| token.usable()) {
            let header = token.header.clone();
            if Instant::now() >= token.refresh {
                if let Ok(mut refresh) = self.inner.refresh.clone().try_lock_owned() {
                    let now = Instant::now();
                    if refresh.is_none_or(|retry_at| now >= retry_at) {
                        *refresh = now.checked_add(Duration::from_secs(1));
                        let cache = self.clone();
                        tokio::spawn(async move {
                            let _ = cache.load(&mut refresh).await;
                        });
                    }
                }
            }
            // Keep the read guard through the nonblocking refresh claim. A successful refresh
            // cannot publish between the freshness check and that claim, causing extra I/O.
            return Ok(header);
        }
        drop(cached);
        let mut refresh = self.inner.refresh.lock().await;
        let cached = self.inner.cached.read().await;
        if let Some(token) = cached.as_ref().filter(|token| token.usable()) {
            return Ok(token.header.clone());
        }
        drop(cached);
        // Rejection and expiry bypass early-refresh cooldowns.
        self.load(&mut refresh).await
    }
    async fn load(&self, retry_at: &mut Option<Instant>) -> Result<HeaderValue, CacheError<E>> {
        let result = tokio::time::timeout(Duration::from_secs(30), (self.inner.source)())
            .await
            .map_err(|_| CacheError::Timeout)
            .and_then(|token| token.map_err(CacheError::Source))
            .and_then(|token| {
                if token.usable() {
                    Ok(token)
                } else {
                    Err(CacheError::InvalidToken)
                }
            });
        match result {
            Ok(token) => {
                let header = token.header.clone();
                *self.inner.cached.write().await = Some(token);
                *retry_at = None;
                Ok(header)
            }
            Err(error) => {
                // Failed refresh never discards a live token or queues readers behind its delay.
                *retry_at = Instant::now().checked_add(Duration::from_secs(1));
                Err(error)
            }
        }
    }
    pub(super) async fn reject(&self, sent: &HeaderValue) {
        let mut cached = self.inner.cached.write().await;
        if cached.as_ref().is_some_and(|token| token.header == *sent) {
            *cached = None;
        }
    }
}

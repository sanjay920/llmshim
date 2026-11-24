//! Optional dispatch mechanics for transports that are not JSON over SSE.
use crate::error::{Result, ShimError};
use futures::Stream;
use std::{future::Future, pin::Pin, time::Duration};

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<bytes::Bytes>> + Send>>;
pub type EventStream = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

/// A resolved identity signs only after admission, without further credential traffic.
pub trait PreparedAuthorization: Send {
    fn sign(self: Box<Self>, request: &mut reqwest::Request) -> Result<()>;
}

pub type AuthorizationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Box<dyn PreparedAuthorization>>>> + Send + 'a>>;

/// Authorization runs on the built request on every HTTP attempt, under its deadline.
/// Existing providers retain JSON/SSE dispatch without implementing this trait.
pub trait RequestTransport: Send + Sync {
    /// Resolve before acquiring budget or rate capacity. The optional single-use signer
    /// runs after acquisition, so slow credential traffic cannot leave unsent liabilities.
    fn prepare_authorization<'a>(
        &'a self,
        request: &'a mut reqwest::Request,
    ) -> AuthorizationFuture<'a> {
        Box::pin(async move {
            self.authorize(request).await?;
            Ok(None)
        })
    }

    fn authorize<'a>(
        &'a self,
        request: &'a mut reqwest::Request,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

    /// Called when the dispatch deadline expires while resolving authorization.
    fn authorization_timeout(&self) -> ShimError {
        crate::error::provider_error(503, "authorization timeout")
    }

    /// Invalidate the identity actually sent when a typed upstream rejection permits refresh.
    /// Dispatch bounds this callback and retries at most once within its configured retry limit.
    fn authorization_rejected<'a>(
        &'a self,
        _status: u16,
        _body: &'a str,
        _sent: Option<&'a reqwest::header::HeaderValue>,
    ) -> Pin<Box<dyn Future<Output = Result<bool>> + Send + 'a>> {
        Box::pin(async { Ok(false) })
    }

    fn stream_normalizer(
        &self,
        target: crate::reasoning::ReplayTarget,
        limits: crate::streaming::StreamRetentionLimits,
    ) -> Result<crate::streaming::StreamNormalizer> {
        crate::streaming::StreamNormalizer::with_retention_limits(target, limits)
    }

    fn events(&self, model: &str, bytes: ByteStream) -> EventStream;

    fn events_with_response_id(
        &self,
        model: &str,
        _id: Option<&str>,
        bytes: ByteStream,
    ) -> EventStream {
        self.events(model, bytes)
    }

    fn error(
        &self,
        status: u16,
        headers: &reqwest::header::HeaderMap,
        body: String,
        retry_after: Option<Duration>,
    ) -> ShimError;

    /// A transport may expose the upstream correlation identifier on unary responses.
    fn response_id(&self, _headers: &reqwest::header::HeaderMap) -> Option<String> {
        None
    }

    /// Credential references bypass ambient proxies unless explicitly opted in.
    fn requires_scoped_client(&self) -> bool {
        true
    }
}

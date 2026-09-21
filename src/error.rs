#[cfg(feature = "proxy")]
mod normalize;
#[cfg(feature = "proxy")]
pub(crate) use normalize::{normalize_error, NormalizedError};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ShimError {
    #[error("unknown provider in model string: {0}")]
    UnknownProvider(String),

    #[error("missing model field in request")]
    MissingModel,

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// A non-success response from the provider. `retry_after` is the
    /// server's own `Retry-After` (delay-seconds or HTTP-date, parsed at
    /// receipt), so a caller with its own backoff can wait what was asked
    /// instead of guessing; `None` when the header was absent or unparseable.
    #[error("provider error ({status}): {body}")]
    ProviderError {
        status: u16,
        body: String,
        retry_after: Option<std::time::Duration>,
    },

    #[error("stream error: {0}")]
    Stream(String),

    #[error("all providers failed: {0:?}")]
    AllFailed(Vec<String>),
}

pub type Result<T> = std::result::Result<T, ShimError>;

#[cfg(feature = "proxy")]
mod normalize;
#[cfg(feature = "proxy")]
pub(crate) use normalize::{normalize_error, NormalizedError};

use thiserror::Error;

#[derive(Error)]
pub enum ShimError {
    #[error("unknown provider in model string: {0}")]
    UnknownProvider(String),

    #[error("missing model field in request")]
    MissingModel,

    #[error("HTTP error: {0}")]
    Http(reqwest::Error),

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

impl std::fmt::Debug for ShimError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownProvider(provider) => formatter
                .debug_tuple("UnknownProvider")
                .field(provider)
                .finish(),
            Self::MissingModel => formatter.write_str("MissingModel"),
            Self::Http(error) => formatter
                .debug_tuple("Http")
                .field(&error.to_string())
                .finish(),
            Self::Json(error) => formatter.debug_tuple("Json").field(error).finish(),
            Self::ProviderError {
                status,
                body,
                retry_after,
            } => formatter
                .debug_struct("ProviderError")
                .field("status", status)
                .field("body", body)
                .field("retry_after", retry_after)
                .finish(),
            Self::Stream(error) => formatter.debug_tuple("Stream").field(error).finish(),
            Self::AllFailed(errors) => formatter.debug_tuple("AllFailed").field(errors).finish(),
        }
    }
}

impl From<reqwest::Error> for ShimError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error.without_url())
    }
}

pub type Result<T> = std::result::Result<T, ShimError>;

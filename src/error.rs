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

    #[error("provider error ({status}): {body}")]
    ProviderError { status: u16, body: String },

    #[error("stream error: {0}")]
    Stream(String),

    #[error("all providers failed: {0:?}")]
    AllFailed(Vec<String>),
}

pub type Result<T> = std::result::Result<T, ShimError>;

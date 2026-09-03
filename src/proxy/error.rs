use super::types::{ErrorDetail, ErrorResponse};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::time::Duration;

/// Error type for proxy handlers. Wraps core [`ShimError`]s and adds the two
/// backpressure responses this layer introduces, both carrying a `Retry-After`.
pub enum ApiError {
    /// A translated core error.
    Shim(crate::error::ShimError),
    /// Proactive rate limit hit before dispatch → HTTP 429 + `Retry-After`.
    RateLimited(Duration),
    /// Instance concurrency queue timed out → HTTP 503 + `Retry-After`.
    Overloaded(Duration),
    /// Missing or invalid API key → HTTP 401. Constructed by the gateway.
    #[cfg_attr(not(feature = "gateway"), allow(dead_code))]
    Unauthorized,
}

impl From<crate::error::ShimError> for ApiError {
    fn from(err: crate::error::ShimError) -> Self {
        ApiError::Shim(err)
    }
}

/// `Retry-After` in whole seconds, rounded up so we never advise waiting less
/// than the true reset. Clamped to at least 1s when any wait is suggested.
fn retry_after_header(d: Duration) -> HeaderValue {
    let secs = d.as_secs() + u64::from(d.subsec_millis() > 0);
    HeaderValue::from(secs.max(1))
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let shim = match self {
            ApiError::RateLimited(retry_after) => {
                return error_with_retry_after(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limited",
                    "Upstream provider rate limit reached; retry after the suggested delay",
                    retry_after,
                );
            }
            ApiError::Overloaded(retry_after) => {
                return error_with_retry_after(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "overloaded",
                    "Proxy is at capacity; retry after the suggested delay",
                    retry_after,
                );
            }
            ApiError::Unauthorized => {
                let body = ErrorResponse {
                    error: ErrorDetail {
                        code: "unauthorized".to_string(),
                        message: "Missing or invalid API key".to_string(),
                    },
                };
                return (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
            }
            ApiError::Shim(e) => e,
        };

        let (status, code, message) = match &shim {
            crate::error::ShimError::MissingModel => (
                StatusCode::BAD_REQUEST,
                "missing_model",
                "Missing 'model' field in request".to_string(),
            ),
            crate::error::ShimError::UnknownProvider(p) => (
                StatusCode::BAD_REQUEST,
                "unknown_provider",
                format!("Unknown provider or model: {}", p),
            ),
            crate::error::ShimError::ProviderError { status, body } => {
                let http_status = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
                (http_status, "provider_error", body.clone())
            }
            crate::error::ShimError::Http(e) => (
                StatusCode::BAD_GATEWAY,
                "http_error",
                format!("HTTP error: {}", e),
            ),
            crate::error::ShimError::Json(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "json_error",
                format!("JSON error: {}", e),
            ),
            crate::error::ShimError::Stream(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "stream_error", e.clone())
            }
            crate::error::ShimError::AllFailed(errors) => (
                StatusCode::BAD_GATEWAY,
                "all_failed",
                format!("All providers failed: {:?}", errors),
            ),
        };

        let body = ErrorResponse {
            error: ErrorDetail {
                code: code.to_string(),
                message,
            },
        };

        (status, axum::Json(body)).into_response()
    }
}

fn error_with_retry_after(
    status: StatusCode,
    code: &str,
    message: &str,
    retry_after: Duration,
) -> Response {
    let body = ErrorResponse {
        error: ErrorDetail {
            code: code.to_string(),
            message: message.to_string(),
        },
    };
    let mut resp = (status, axum::Json(body)).into_response();
    resp.headers_mut()
        .insert(header::RETRY_AFTER, retry_after_header(retry_after));
    resp
}

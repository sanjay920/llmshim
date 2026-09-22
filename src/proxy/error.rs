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
    /// Whole proxy logical request lifetime expired → HTTP 504.
    RequestTimeout,
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
            ApiError::RequestTimeout => {
                let body = ErrorResponse {
                    error: ErrorDetail {
                        code: "request_timeout".to_string(),
                        message: "Request exceeded the configured logical lifetime".to_string(),
                    },
                };
                return (StatusCode::GATEWAY_TIMEOUT, axum::Json(body)).into_response();
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
            crate::error::ShimError::ProviderError { status, body, .. } => {
                let http_status = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
                let code = if *status == 400 {
                    "invalid_request"
                } else {
                    "provider_error"
                };
                (http_status, code, body.clone())
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
                format!(
                    "All providers failed: {}",
                    errors
                        .iter()
                        .map(|error| crate::error::normalize_error(error).message)
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            ),
        };

        let mut normalized = crate::error::normalize_error(&message);
        normalized.status = normalized.status.or(Some(status.as_u16()));
        let body = ErrorResponse {
            error: ErrorDetail {
                code: normalized.public_code(code).to_owned(),
                message: normalized.message.clone(),
            },
        };
        let mut response = (status, axum::Json(body)).into_response();
        // Native facades share this response in-process. Keep typed source
        // metadata separate from the public, human-readable message.
        response.extensions_mut().insert(normalized);
        response
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

/// Keep flat `type`/`message` for existing clients. The nested `error` deliberately
/// repeats the message alongside source metadata for lossless native rendering.
pub(crate) fn stream_error(message: &str) -> serde_json::Value {
    let error = crate::error::normalize_error(message);
    serde_json::json!({"type":"error","message":error.message,"error":error})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn fallback_error_summaries_unwrap_each_provider_message() {
        let source =
            serde_json::json!({"error":{"message":"Invalid key.","type":"authentication_error"}});
        let response = ApiError::from(crate::error::ShimError::AllFailed(vec![
            format!("provider error (401): {source}"),
            "stream error: Connection closed.".into(),
        ]))
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(response.into_body(), 10000)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "all_failed");
        assert_eq!(
            body["error"]["message"],
            "All providers failed: Invalid key.; Connection closed."
        );
    }
}

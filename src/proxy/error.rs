use super::types::{ErrorDetail, ErrorResponse};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Wrapper around ShimError for axum responses.
pub struct ApiError(pub crate::error::ShimError);

impl From<crate::error::ShimError> for ApiError {
    fn from(err: crate::error::ShimError) -> Self {
        ApiError(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self.0 {
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

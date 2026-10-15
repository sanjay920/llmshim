//! Status/code classification never interprets an upstream explanation.
use crate::error::ShimError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum VertexErrorKind {
    Credentials,
    AccessDenied,
    Validation,
    NotFound,
    Throttled,
    Timeout,
    Unavailable,
    Protocol,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct VertexError {
    pub kind: VertexErrorKind,
    pub status: u16,
    pub code: String,
}
impl VertexError {
    pub fn service(status: u16, code: impl Into<String>) -> Self {
        let code = code.into();
        let kind = match code.as_str() {
            "InvalidEvent" => VertexErrorKind::Protocol,
            "UNAUTHENTICATED" | "authentication_error" => VertexErrorKind::Credentials,
            "PERMISSION_DENIED" | "permission_error" => VertexErrorKind::AccessDenied,
            "INVALID_ARGUMENT" | "invalid_request_error" => VertexErrorKind::Validation,
            "NOT_FOUND" | "not_found_error" => VertexErrorKind::NotFound,
            "RESOURCE_EXHAUSTED" | "rate_limit_error" => VertexErrorKind::Throttled,
            "DEADLINE_EXCEEDED" | "timeout_error" => VertexErrorKind::Timeout,
            "UNAVAILABLE" | "INTERNAL" | "overloaded_error" | "api_error" => {
                VertexErrorKind::Unavailable
            }
            _ => match status {
                400 | 422 => VertexErrorKind::Validation,
                401 => VertexErrorKind::Credentials,
                403 => VertexErrorKind::AccessDenied,
                404 => VertexErrorKind::NotFound,
                408 | 504 => VertexErrorKind::Timeout,
                429 => VertexErrorKind::Throttled,
                500..=599 => VertexErrorKind::Unavailable,
                _ => VertexErrorKind::Protocol,
            },
        };
        Self { kind, status, code }
    }
    pub fn from_shim(error: &ShimError) -> Option<Self> {
        let ShimError::ProviderError { body, .. } = error else {
            return None;
        };
        serde_json::from_value(serde_json::from_str::<Value>(body).ok()?["vertex"].clone()).ok()
    }
    pub(super) fn into_shim(self, retry_after: Option<std::time::Duration>) -> ShimError {
        // Upstream messages can echo credentials or request data. Keep only typed diagnostics.
        let message = format!(
            "Vertex request failed ({}, status {})",
            self.code, self.status
        );
        ShimError::ProviderError {
            status: self.status,
            body: json!({"error":{"message":message,"code":self.code},"vertex":self}).to_string(),
            retry_after,
        }
    }
}
pub(super) fn native_error(native: &Value, status: Option<u16>) -> Option<VertexError> {
    let error = native.get("error").filter(|e| !e.is_null())?;
    let code = error["status"]
        .as_str()
        .or_else(|| error["type"].as_str())
        .unwrap_or("Unknown");
    let code = match code {
        "UNAUTHENTICATED"
        | "authentication_error"
        | "PERMISSION_DENIED"
        | "permission_error"
        | "INVALID_ARGUMENT"
        | "invalid_request_error"
        | "NOT_FOUND"
        | "not_found_error"
        | "RESOURCE_EXHAUSTED"
        | "rate_limit_error"
        | "DEADLINE_EXCEEDED"
        | "timeout_error"
        | "UNAVAILABLE"
        | "INTERNAL"
        | "overloaded_error"
        | "api_error" => code,
        _ => "Unknown",
    };
    let status = status
        .or_else(|| error["code"].as_u64().and_then(|v| u16::try_from(v).ok()))
        .filter(|s| (400..=599).contains(s))
        .unwrap_or(match code {
            "authentication_error" | "UNAUTHENTICATED" => 401,
            "permission_error" | "PERMISSION_DENIED" => 403,
            "not_found_error" | "NOT_FOUND" => 404,
            "rate_limit_error" | "RESOURCE_EXHAUSTED" => 429,
            "overloaded_error" | "UNAVAILABLE" => 503,
            "api_error" | "INTERNAL" => 500,
            "timeout_error" | "DEADLINE_EXCEEDED" => 504,
            _ => 400,
        });
    Some(VertexError::service(status, code))
}

/// ErrorInfo is machine-readable; IAM permission denials alone do not select a new token.
pub(super) fn token_rejected(status: u16, body: &str) -> bool {
    if status == 401 {
        return true;
    }
    if status != 403 {
        return false;
    }
    let native: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = &native["error"];
    error["status"] == "UNAUTHENTICATED"
        || error["type"] == "authentication_error"
        || error["details"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|detail| {
                detail["@type"] == "type.googleapis.com/google.rpc.ErrorInfo"
                    && detail["domain"] == "googleapis.com"
                    && matches!(
                        detail["reason"].as_str(),
                        Some(
                            "ACCESS_TOKEN_EXPIRED"
                                | "ACCESS_TOKEN_SCOPE_INSUFFICIENT"
                                | "ACCESS_TOKEN_TYPE_UNSUPPORTED"
                                | "ACCOUNT_STATE_INVALID"
                        )
                    )
            })
}

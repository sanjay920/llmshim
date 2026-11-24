//! Machine-readable classification; upstream explanations and details are never echoed.
use crate::error::ShimError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AzureErrorKind {
    Credentials,
    AccessDenied,
    Validation,
    NotFound,
    Throttled,
    Timeout,
    Unavailable,
    ContentFilter,
    Protocol,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AzureError {
    pub kind: AzureErrorKind,
    pub status: u16,
    pub code: String,
}
impl AzureError {
    pub fn service(status: u16, code: impl Into<String>) -> Self {
        let code = code.into();
        let kind = match code.as_str() {
            "content_filter" | "content_filter_error" | "ResponsibleAIPolicyViolation" => {
                AzureErrorKind::ContentFilter
            }
            "InvalidEvent" | "InvalidResponse" => AzureErrorKind::Protocol,
            "InvalidAuthenticationToken"
            | "ExpiredAuthenticationToken"
            | "Unauthorized"
            | "InvalidApiKey"
            | "TokenRejected" => AzureErrorKind::Credentials,
            "DeploymentNotFound" => AzureErrorKind::NotFound,
            _ => match status {
                400 | 422 => AzureErrorKind::Validation,
                401 => AzureErrorKind::Credentials,
                403 => AzureErrorKind::AccessDenied,
                404 => AzureErrorKind::NotFound,
                408 | 504 => AzureErrorKind::Timeout,
                429 => AzureErrorKind::Throttled,
                500..=599 => AzureErrorKind::Unavailable,
                _ => AzureErrorKind::Protocol,
            },
        };
        Self { kind, status, code }
    }
    pub fn from_shim(error: &ShimError) -> Option<Self> {
        let ShimError::ProviderError { body, .. } = error else {
            return None;
        };
        serde_json::from_value(serde_json::from_str::<Value>(body).ok()?["azure"].clone()).ok()
    }
    pub(super) fn into_shim(self, retry_after: Option<Duration>) -> ShimError {
        ShimError::ProviderError {
            status: self.status,
            body: json!({"error":{"message":format!("Azure request failed ({}, status {})",
                self.code,self.status),"code":self.code},"azure":self})
            .to_string(),
            retry_after,
        }
    }
}
fn code(error: &Value) -> &'static str {
    let inner = error["innererror"]["code"].as_str();
    if inner == Some("ResponsibleAIPolicyViolation") {
        return "ResponsibleAIPolicyViolation";
    }
    match error["code"].as_str().unwrap_or("") {
        "content_filter" => "content_filter",
        "content_filter_error" => "content_filter_error",
        "ResponsibleAIPolicyViolation" => "ResponsibleAIPolicyViolation",
        "DeploymentNotFound" => "DeploymentNotFound",
        "InvalidAuthenticationToken" => "InvalidAuthenticationToken",
        "ExpiredAuthenticationToken" => "ExpiredAuthenticationToken",
        "Unauthorized" => "Unauthorized",
        "InvalidApiKey" => "InvalidApiKey",
        "PermissionDenied" => "PermissionDenied",
        "Forbidden" => "Forbidden",
        "RateLimitReached" => "RateLimitReached",
        "TooManyRequests" => "TooManyRequests",
        "BadRequest" => "BadRequest",
        "OperationNotSupported" => "OperationNotSupported",
        "Timeout" => "Timeout",
        "ServiceUnavailable" => "ServiceUnavailable",
        _ => "Unknown",
    }
}
pub(super) fn native_error(native: &Value, status: Option<u16>) -> Option<AzureError> {
    let error = native.get("error").filter(|error| !error.is_null())?;
    let code = code(error);
    let status = status.unwrap_or(match code {
        "InvalidAuthenticationToken"
        | "ExpiredAuthenticationToken"
        | "Unauthorized"
        | "InvalidApiKey" => 401,
        "PermissionDenied" | "Forbidden" => 403,
        "DeploymentNotFound" => 404,
        "RateLimitReached" | "TooManyRequests" => 429,
        "Timeout" => 408,
        "ServiceUnavailable" => 503,
        _ => 400,
    });
    Some(AzureError::service(status, code))
}
pub(super) fn token_rejected(status: u16, body: &str) -> bool {
    if status == 401 {
        return true;
    }
    if status != 403 {
        return false;
    }
    let Ok(native) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    matches!(
        code(&native["error"]),
        "InvalidAuthenticationToken" | "ExpiredAuthenticationToken"
    )
}

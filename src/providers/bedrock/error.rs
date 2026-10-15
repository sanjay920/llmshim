//! Stable service classifications; messages never select behavior.
use crate::error::ShimError;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BedrockErrorKind {
    Credentials,
    AccessDenied,
    NotFound,
    Throttled,
    Validation,
    Timeout,
    Model,
    Unavailable,
    Protocol,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CredentialReason {
    ProfileNotFound,
    InvalidConfiguration,
    Timeout,
    ExpiredCredentials,
    ProviderFailed,
    Unhandled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{}", self.description())]
#[non_exhaustive]
pub struct BedrockError {
    pub kind: BedrockErrorKind,
    pub code: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_reason: Option<CredentialReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<serde_json::Value>,
}

impl BedrockError {
    fn description(&self) -> String {
        if let Some(reason) = self.credential_reason {
            let reason = match reason {
                CredentialReason::ProfileNotFound => "credentials could not be found",
                CredentialReason::InvalidConfiguration => "credential configuration is invalid",
                CredentialReason::Timeout => "credential loading timed out",
                CredentialReason::ExpiredCredentials => "returned credentials have expired",
                CredentialReason::ProviderFailed => "credentials provider failed",
                CredentialReason::Unhandled => "unexpected credential failure",
            };
            if let Some(binding) = &self.binding {
                return format!("Bedrock binding {binding}: {reason}");
            }
            if let Some(profile) = &self.profile {
                return format!("Bedrock profile {profile}: {reason}");
            }
        }
        format!(
            "Bedrock {:?} ({}, status {})",
            self.kind, self.code, self.status
        )
    }

    pub fn service(status: u16, code: &str) -> Self {
        let code = code
            .split(':')
            .next()
            .unwrap_or(code)
            .rsplit('#')
            .next()
            .unwrap_or(code);
        let kind = match code {
            "CredentialsError"
            | "ExpiredTokenException"
            | "UnrecognizedClientException"
            | "InvalidSignatureException"
            | "SignatureDoesNotMatch" => BedrockErrorKind::Credentials,
            "ProtocolError" => BedrockErrorKind::Protocol,
            "AccessDeniedException" => BedrockErrorKind::AccessDenied,
            "ResourceNotFoundException" => BedrockErrorKind::NotFound,
            "ThrottlingException" | "throttlingException" | "ModelNotReadyException" => {
                BedrockErrorKind::Throttled
            }
            "ValidationException" | "validationException" => BedrockErrorKind::Validation,
            "ModelTimeoutException" | "modelTimeoutException" => BedrockErrorKind::Timeout,
            "ModelErrorException"
            | "modelStreamErrorException"
            | "malformed_model_output"
            | "malformed_tool_use" => BedrockErrorKind::Model,
            "ServiceUnavailableException"
            | "serviceUnavailableException"
            | "InternalServerException"
            | "internalServerException" => BedrockErrorKind::Unavailable,
            _ => match status {
                401 | 403 => BedrockErrorKind::AccessDenied,
                404 => BedrockErrorKind::NotFound,
                429 => BedrockErrorKind::Throttled,
                400 => BedrockErrorKind::Validation,
                408 | 504 => BedrockErrorKind::Timeout,
                424 => BedrockErrorKind::Model,
                500..=503 => BedrockErrorKind::Unavailable,
                _ => BedrockErrorKind::Other,
            },
        };
        Self {
            kind,
            code: code.to_owned(),
            status,
            profile: None,
            credential_reason: None,
            binding: None,
            native: None,
        }
    }

    pub(super) fn credentials(profile: &str, reason: CredentialReason, status: u16) -> Self {
        let mut error = Self::service(status, "CredentialsError");
        error.profile = Some(profile.to_owned());
        error.credential_reason = Some(reason);
        error
    }

    pub(crate) fn protocol() -> Self {
        Self::service(502, "ProtocolError")
    }

    /// Recover the typed cloud outcome without branching on display text.
    pub fn from_shim(error: &ShimError) -> Option<Self> {
        let ShimError::ProviderError { body, .. } = error else {
            return None;
        };
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|value| serde_json::from_value(value["bedrock"].clone()).ok())
    }

    pub(crate) fn into_shim(self, retry_after: Option<Duration>) -> ShimError {
        let native = self.native.clone().unwrap_or(serde_json::Value::Null);
        self.with_native_error(native, retry_after)
    }

    fn is_signature_error(&self) -> bool {
        matches!(
            self.code.as_str(),
            "InvalidSignatureException" | "SignatureDoesNotMatch"
        )
    }
    pub(super) fn safe_native(&self, native: serde_json::Value) -> serde_json::Value {
        if self.is_signature_error() {
            // Canonical-request diagnostics may echo tokens anywhere in the structure.
            serde_json::json!({"message":"AWS request signature could not be verified"})
        } else {
            native
        }
    }

    pub(super) fn with_native_error(
        mut self,
        mut native: serde_json::Value,
        retry_after: Option<Duration>,
    ) -> ShimError {
        native = self.safe_native(native);
        if self.is_signature_error() {
            self.native = None;
        }
        let fallback = self.to_string();
        let message = native["message"].as_str().unwrap_or(&fallback);
        ShimError::ProviderError {
            status: self.status,
            body: serde_json::json!({
                "error":{"message":message,"type":self.kind,"code":self.code,"status":self.status},
                "bedrock":self,"native":native
            })
            .to_string(),
            retry_after,
        }
    }
}

//! One bounded interpretation of upstream errors, shared by all HTTP surfaces.
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct NormalizedError {
    pub message: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub code: Value,
    pub param: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

impl NormalizedError {
    pub fn public_code<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.code
            .as_str()
            .filter(|code| !code.is_empty())
            .or(self.kind.as_deref())
            .unwrap_or(fallback)
    }

    pub fn code_type(&self) -> Option<&'static str> {
        match self.code.as_str() {
            Some("invalid_api_key" | "unauthorized" | "authentication_error") => {
                Some("authentication_error")
            }
            Some("permission_denied" | "permission_error") => Some("permission_error"),
            Some("insufficient_quota" | "rate_limit_exceeded" | "rate_limit_error") => {
                Some("rate_limit_error")
            }
            Some("model_not_found" | "not_found_error") => Some("not_found_error"),
            _ => None,
        }
    }
}

/// Decode recognized error envelopes and remove known internal Display prefixes.
/// Ordinary text and malformed envelopes are retained; neither is an error here.
pub(crate) fn normalize_error(message: &str) -> NormalizedError {
    let mut error = NormalizedError {
        message: message.into(),
        kind: None,
        code: Value::Null,
        param: Value::Null,
        status: None,
    };
    for _ in 0..4 {
        let mut candidate = error.message.as_str();
        for _ in 0..4 {
            if let Some(rest) = candidate
                .strip_prefix("upstream error: ")
                .or_else(|| candidate.strip_prefix("stream error: "))
            {
                candidate = rest;
            } else if let Some((status, body)) = candidate
                .strip_prefix("provider error (")
                .and_then(|rest| rest.split_once("): "))
                .and_then(|(status, body)| {
                    status
                        .parse::<u16>()
                        .ok()
                        .filter(|s| (100..=599).contains(s))
                        .map(|status| (status, body))
                })
            {
                error.status = Some(status);
                candidate = body;
            } else {
                break;
            }
        }
        error.message = candidate.to_owned();
        let Ok(value) = serde_json::from_str::<Value>(&error.message) else {
            break;
        };
        let Some(inner) = value.get("error").and_then(Value::as_object) else {
            break;
        };
        let Some(message) = inner.get("message").and_then(Value::as_str) else {
            break;
        };
        error.message = message.into();
        error.kind = inner
            .get("type")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        error.code = inner.get("code").cloned().unwrap_or(Value::Null);
        error.param = inner.get("param").cloned().unwrap_or(Value::Null);
        if let Some(status) = inner
            .get("status")
            .and_then(Value::as_u64)
            .filter(|s| (100..=599).contains(s))
        {
            error.status = Some(status as u16);
        }
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shared_normalization_handles_plain_text_nested_envelopes_and_display_prefixes() {
        let text = "upstream error: provider error (401): Plain failure.";
        let error = normalize_error(text);
        assert_eq!(error.message, "Plain failure.");
        assert_eq!(error.status, Some(401));
        let inner = json!({"error":{"type":"authentication_error","code":"invalid_api_key","message":"Invalid key."}});
        let outer = json!({"error":{"message":inner.to_string()}});
        let error = normalize_error(&format!("stream error: {outer}"));
        assert_eq!(error.message, "Invalid key.");
        assert_eq!(error.kind.as_deref(), Some("authentication_error"));
        assert_eq!(error.public_code("provider_error"), "invalid_api_key");
        for text in [
            " ordinary text ",
            "{bad JSON",
            r#"{"error":{"message":null}}"#,
            r#"{"message":"not an envelope"}"#,
        ] {
            assert_eq!(normalize_error(text).message, text);
        }
    }

    #[test]
    fn nested_envelopes_have_a_fixed_unwrap_bound() {
        let mut message = "deep failure".to_owned();
        for _ in 0..5 {
            message = json!({"error":{"message":message}}).to_string();
        }
        let normalized = normalize_error(&message);
        assert_eq!(
            normalized.message,
            json!({"error":{"message":"deep failure"}}).to_string()
        );
    }
}

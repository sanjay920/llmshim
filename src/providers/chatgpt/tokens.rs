use super::auth::auth_error;
use crate::error::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(super) fn now() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn claims(token: &str) -> Option<Value> {
    // Claims are hints for expiry/account routing, not local proof of identity.
    // The upstream validates the token. Never print token contents on errors.
    let payload = token.split('.').nth(1)?.trim_end_matches('=');
    crate::json_bounds::parse_slice(
        &URL_SAFE_NO_PAD.decode(payload).ok()?,
        crate::json_bounds::Limits::OAUTH,
    )
    .ok()
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub(super) refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    pub account_id: Option<String>,
}

impl Tokens {
    pub(super) fn normalize(&mut self) {
        let access_claims = claims(&self.access_token);
        if self.expires_at.is_none() {
            self.expires_at = access_claims.as_ref().and_then(|c| c["exp"].as_u64());
        }
        if self.account_id.as_deref().is_none_or(str::is_empty) {
            let id_claims = self.id_token.as_deref().and_then(claims);
            self.account_id = id_claims.iter().chain(access_claims.iter()).find_map(|c| {
                c.get("https://api.openai.com/auth")?
                    .get("chatgpt_account_id")?
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            });
        }
    }

    pub(super) fn usable(&self) -> bool {
        !self.access_token.is_empty()
            && self
                .expires_at
                .is_some_and(|exp| exp > now().saturating_add(60))
    }

    pub(super) fn from_response(value: Value, previous: Option<&Tokens>) -> Result<Self> {
        let mut tokens: Self = serde_json::from_value(value.clone())
            .map_err(|_| auth_error(502, "invalid OAuth token response"))?;
        if tokens.access_token.is_empty() {
            return Err(auth_error(502, "OAuth token response has no access token"));
        }
        if let Some(old) = previous {
            if tokens.refresh_token.as_deref().is_none_or(str::is_empty) {
                tokens.refresh_token = old.refresh_token.clone();
            }
            if tokens.id_token.as_deref().is_none_or(str::is_empty) {
                tokens.id_token = old.id_token.clone();
            }
        }
        if tokens.expires_at.is_none() {
            tokens.expires_at = value["expires_in"]
                .as_u64()
                .map(|s| now().saturating_add(s));
        }
        tokens.normalize();
        if tokens.account_id.is_none() {
            tokens.account_id = previous.and_then(|old| old.account_id.clone());
        }
        if !tokens.usable() {
            return Err(auth_error(502, "OAuth response has no usable token expiry"));
        }
        Ok(tokens)
    }
}

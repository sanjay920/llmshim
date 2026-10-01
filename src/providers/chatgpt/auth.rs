//! Device-code OAuth for flat and Codex-format credential files.
//! Protocol reference: BerriAI/litellm fa09de9e, llms/chatgpt/authenticator.py.

use super::tokens::Tokens;
use crate::error::{Result, ShimError};
use reqwest::Client;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::{sleep, Instant};

const AUTH_BASE: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

pub(super) fn auth_error(status: u16, message: &str) -> ShimError {
    ShimError::ProviderError {
        status,
        body: format!("ChatGPT: {message}"),
        retry_after: None,
    }
}

/// Login status without disclosing tokens or contacting the network.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginStatus {
    SignedOut,
    Ready,
    NeedsRefresh,
    NeedsLogin,
}

/// A pending login. Display only `verification_url` and `user_code` to the user.
pub struct DeviceCode {
    pub verification_url: String,
    pub user_code: String,
    device_auth_id: String,
    interval: Duration,
    deadline: Instant,
}

/// OAuth credentials can use an independent cache or a shared Codex auth file.
/// Clones and separate processes coordinate refreshes through a cache file lock.
#[derive(Clone)]
pub struct ChatGptAuth {
    pub(super) path: PathBuf,
    pub(super) protected_default_root: Option<PathBuf>,
    pub(super) base_url: String,
    pub(super) http: Client,
}

impl Default for ChatGptAuth {
    fn default() -> Self {
        Self::from_env()
    }
}

impl ChatGptAuth {
    pub fn from_env() -> Self {
        let token_directory_override = std::env::var_os("CHATGPT_TOKEN_DIR");
        let auth_file_override = std::env::var_os("CHATGPT_AUTH_FILE");
        let use_default_path = token_directory_override.is_none() && auth_file_override.is_none();
        let protected_default_root = use_default_path.then(crate::config::config_dir);
        let default_token_directory = crate::config::config_dir().join("chatgpt");
        let token_directory = token_directory_override
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or(default_token_directory);
        let auth_file = PathBuf::from(auth_file_override.unwrap_or_else(|| "auth.json".into()));
        let auth_path = if auth_file.is_absolute() {
            auth_file
        } else {
            token_directory.join(auth_file)
        };
        let mut chatgpt_auth = Self::new(auth_path);
        chatgpt_auth.protected_default_root = protected_default_root;
        chatgpt_auth
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            protected_default_root: None,
            base_url: AUTH_BASE.into(),
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .expect("OAuth HTTP client"),
        }
    }

    /// Override the authorization server, primarily for local integration tests.
    pub fn with_auth_base(mut self, base_url: String) -> Self {
        self.base_url = base_url.trim_end_matches('/').into();
        self
    }

    pub fn auth_path(&self) -> &Path {
        &self.path
    }

    pub(super) fn replay_account(&self) -> Option<String> {
        self.read().ok().flatten().and_then(|t| t.account_id)
    }

    pub fn status(&self) -> Result<LoginStatus> {
        Ok(match self.read()? {
            None => LoginStatus::SignedOut,
            Some(t) if t.usable() => LoginStatus::Ready,
            Some(t) if t.refresh_token.as_deref().is_some_and(|s| !s.is_empty()) => {
                LoginStatus::NeedsRefresh
            }
            _ => LoginStatus::NeedsLogin,
        })
    }

    /// Read a valid cache or refresh it. Never initiates an interactive login.
    pub(super) async fn credentials(&self) -> Result<Tokens> {
        if let Some(tokens) = self.read()? {
            if tokens.usable() {
                return Ok(tokens);
            }
        }
        let _lock = self.lock().await?;
        let old = self
            .read()?
            .ok_or_else(|| auth_error(401, "run `llmshim login chatgpt` first"))?;
        if old.usable() {
            return Ok(old);
        }
        let refresh = old
            .refresh_token
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| auth_error(401, "session expired; run `llmshim login chatgpt`"))?;
        let response = self
            .http
            .post(format!("{}/oauth/token", self.base_url))
            .json(&json!({
                "client_id": CLIENT_ID, "grant_type": "refresh_token",
                "refresh_token": refresh, "scope": "openid profile email"
            }))
            .send()
            .await?;
        let value = oauth_json(
            response,
            "token refresh failed; run `llmshim login chatgpt` if the session was revoked",
        )
        .await?;
        let tokens = Tokens::from_response(value, Some(&old))?;
        self.save(&tokens, self.read_document()?)?;
        Ok(tokens)
    }

    pub(super) fn cached_credentials(&self) -> Result<Tokens> {
        self.read()?.filter(Tokens::usable).ok_or_else(|| auth_error(401,
            "a valid login is required; use `llmshim login chatgpt`, then the async completion/stream entry points"))
    }

    pub async fn start_login(&self) -> Result<DeviceCode> {
        let response = self
            .http
            .post(format!(
                "{}/api/accounts/deviceauth/usercode",
                self.base_url
            ))
            .json(&json!({"client_id": CLIENT_ID}))
            .send()
            .await?;
        let data = oauth_json(response, "device login could not start; check that device-code login is enabled in ChatGPT settings").await?;
        let field = |key: &str| {
            data[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let device_auth_id = field("device_auth_id")
            .ok_or_else(|| auth_error(502, "invalid device-code response"))?;
        let user_code = field("user_code")
            .or_else(|| field("usercode"))
            .ok_or_else(|| auth_error(502, "invalid device-code response"))?;
        let interval = data["interval"]
            .as_u64()
            .or_else(|| data["interval"].as_str()?.parse().ok())
            .unwrap_or(5)
            .clamp(5, 900);
        Ok(DeviceCode {
            verification_url: format!("{}/codex/device", self.base_url),
            user_code,
            device_auth_id,
            interval: Duration::from_secs(interval),
            deadline: Instant::now() + LOGIN_TIMEOUT,
        })
    }

    pub async fn finish_login(&self, code: DeviceCode) -> Result<()> {
        if Instant::now() >= code.deadline {
            return Err(auth_error(
                408,
                "device login timed out; run `llmshim login chatgpt` again",
            ));
        }
        tokio::time::timeout_at(code.deadline, self.poll_login(&code))
            .await
            .map_err(|_| {
                auth_error(
                    408,
                    "device login timed out; run `llmshim login chatgpt` again",
                )
            })?
    }

    async fn poll_login(&self, code: &DeviceCode) -> Result<()> {
        loop {
            let response = self
                .http
                .post(format!("{}/api/accounts/deviceauth/token", self.base_url))
                .json(&json!({"device_auth_id": code.device_auth_id, "user_code": code.user_code}))
                .send()
                .await?;
            if matches!(response.status().as_u16(), 403 | 404) {
                sleep(code.interval).await;
                continue;
            }
            let data = oauth_json(response, "device authorization failed").await?;
            let auth_code = data["authorization_code"]
                .as_str()
                .filter(|s| !s.is_empty());
            let verifier = data["code_verifier"].as_str().filter(|s| !s.is_empty());
            let (Some(auth_code), Some(verifier)) = (auth_code, verifier) else {
                return Err(auth_error(502, "invalid device authorization response"));
            };
            let redirect = format!("{}/deviceauth/callback", self.base_url);
            let response = self
                .http
                .post(format!("{}/oauth/token", self.base_url))
                .form(&[
                    ("grant_type", "authorization_code"),
                    ("code", auth_code),
                    ("redirect_uri", redirect.as_str()),
                    ("client_id", CLIENT_ID),
                    ("code_verifier", verifier),
                ])
                .send()
                .await?;
            let data = oauth_json(response, "device token exchange failed").await?;
            let tokens = Tokens::from_response(data, None)?;
            let _lock = self.lock().await?;
            let document = match self.read_document() {
                Ok(document) => document,
                Err(ShimError::ProviderError { status: 401, .. }) => None,
                Err(error) => return Err(error),
            };
            self.save(&tokens, document)?;
            return Ok(());
        }
    }
}

async fn oauth_json(response: reqwest::Response, message: &str) -> Result<Value> {
    if !response.status().is_success() {
        // OAuth bodies can echo credentials. Return only status + fixed context.
        return Err(auth_error(response.status().as_u16(), message));
    }
    use futures::StreamExt;
    let mut chunks = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| auth_error(502, "invalid OAuth JSON response"))?;
        if chunk.len() > (1024 * 1024_usize).saturating_sub(bytes.len()) {
            return Err(auth_error(502, "OAuth JSON response exceeds size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    match crate::json_bounds::parse_slice(&bytes, crate::json_bounds::Limits::OAUTH) {
        Ok(value) => Ok(value),
        Err(crate::json_bounds::ParseError::Malformed(_)) => {
            Err(auth_error(502, "invalid OAuth JSON response"))
        }
        Err(crate::json_bounds::ParseError::Complexity) => Err(auth_error(
            502,
            "OAuth JSON response exceeds complexity limit",
        )),
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;

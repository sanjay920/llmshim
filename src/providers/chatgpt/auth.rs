//! Device-code OAuth and an independent, LiteLLM-compatible token cache.
//! Protocol reference: BerriAI/litellm fa09de9e, llms/chatgpt/authenticator.py.

use crate::error::{Result, ShimError};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use fs2::FileExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::File,
    io::Write,
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

fn storage_error(_: impl std::fmt::Display) -> ShimError {
    auth_error(500, "cannot read or update the OAuth cache")
}

fn now() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn claims(token: &str) -> Option<Value> {
    // Claims are hints for expiry/account routing, not local proof of identity.
    // The upstream validates the token. Never print token contents on errors.
    let payload = token.split('.').nth(1)?.trim_end_matches('=');
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Tokens {
    pub access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    pub account_id: Option<String>,
}

impl Tokens {
    fn normalize(&mut self) {
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

    fn usable(&self) -> bool {
        !self.access_token.is_empty()
            && self
                .expires_at
                .is_some_and(|exp| exp > now().saturating_add(60))
    }

    fn from_response(value: Value, previous: Option<&Tokens>) -> Result<Self> {
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

/// OAuth credentials are stored separately from API keys and Codex credentials.
/// Clones and separate processes coordinate refreshes through a cache file lock.
#[derive(Clone)]
pub struct ChatGptAuth {
    path: PathBuf,
    base_url: String,
    http: Client,
}

impl Default for ChatGptAuth {
    fn default() -> Self {
        Self::from_env()
    }
}

impl ChatGptAuth {
    pub fn from_env() -> Self {
        let dir = std::env::var_os("CHATGPT_TOKEN_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::config::config_dir().join("chatgpt"));
        let file = std::env::var_os("CHATGPT_AUTH_FILE").unwrap_or_else(|| "auth.json".into());
        Self::new(dir.join(file))
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
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

    fn read(&self) -> Result<Option<Tokens>> {
        let data = match std::fs::read(&self.path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(storage_error(e)),
        };
        let mut tokens: Tokens = serde_json::from_slice(&data)
            .map_err(|_| auth_error(401, "invalid OAuth cache; run `llmshim login chatgpt`"))?;
        tokens.normalize();
        Ok(Some(tokens))
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

    fn parent(&self) -> &Path {
        self.path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
    }

    async fn lock(&self) -> Result<File> {
        let mut dir = std::fs::DirBuilder::new();
        dir.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            dir.mode(0o700);
        }
        dir.create(self.parent()).map_err(storage_error)?;
        let mut lock_name = self.path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(PathBuf::from(lock_name))
            .map_err(storage_error)?;
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(file),
                Err(e)
                    if e.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                        && Instant::now() < deadline =>
                {
                    sleep(Duration::from_millis(50)).await
                }
                Err(_) => return Err(auth_error(503, "OAuth cache is busy; retry shortly")),
            }
        }
    }

    fn save(&self, tokens: &Tokens) -> Result<()> {
        // NamedTempFile is owner-only on Unix. Atomic replacement means readers
        // see either complete generation, including rotated refresh tokens.
        let mut file = tempfile::NamedTempFile::new_in(self.parent()).map_err(storage_error)?;
        file.write_all(&serde_json::to_vec(tokens).map_err(storage_error)?)
            .map_err(storage_error)?;
        file.as_file().sync_all().map_err(storage_error)?;
        file.persist(&self.path).map_err(storage_error)?;
        Ok(())
    }

    pub async fn logout(&self) -> Result<()> {
        let _lock = self.lock().await?;
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(storage_error(e)),
        }
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
        self.save(&tokens)?;
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
            self.save(&tokens)?;
            return Ok(());
        }
    }
}

async fn oauth_json(response: reqwest::Response, message: &str) -> Result<Value> {
    if !response.status().is_success() {
        // OAuth bodies can echo credentials. Return only status + fixed context.
        return Err(auth_error(response.status().as_u16(), message));
    }
    response
        .json()
        .await
        .map_err(|_| auth_error(502, "invalid OAuth JSON response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_device_authorization_waits_and_times_out() {
        let mut server = mockito::Server::new_async().await;
        let pending = server
            .mock("POST", "/api/accounts/deviceauth/token")
            .with_status(403)
            .expect(1)
            .create_async()
            .await;
        let dir = tempfile::tempdir().unwrap();
        let auth = ChatGptAuth::new(dir.path().join("auth.json")).with_auth_base(server.url());
        let code = DeviceCode {
            verification_url: String::new(),
            user_code: "test".into(),
            device_auth_id: "device".into(),
            interval: Duration::from_secs(5),
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let err = auth.finish_login(code).await.unwrap_err();
        assert!(matches!(err, ShimError::ProviderError { status: 408, .. }));
        pending.assert_async().await;
        assert!(!auth.auth_path().exists());
    }

    #[tokio::test]
    async fn expired_device_code_does_not_start_polling() {
        let dir = tempfile::tempdir().unwrap();
        let auth = ChatGptAuth::new(dir.path().join("auth.json"));
        let code = DeviceCode {
            verification_url: String::new(),
            user_code: "test".into(),
            device_auth_id: "device".into(),
            interval: Duration::from_secs(5),
            deadline: Instant::now() - Duration::from_secs(1),
        };
        let err = auth.finish_login(code).await.unwrap_err();
        assert!(matches!(err, ShimError::ProviderError { status: 408, .. }));
    }

    #[test]
    fn unknown_or_expired_token_expiry_is_rejected() {
        for value in [
            json!({"access_token": "private"}),
            json!({"access_token": "private", "expires_at": 1}),
            json!({"access_token": "", "expires_in": 3600}),
            json!({"refresh_token": "private"}),
        ] {
            let err = Tokens::from_response(value, None).err().unwrap();
            assert!(!err.to_string().contains("private"));
        }
    }
}

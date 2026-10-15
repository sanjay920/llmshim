//! OAuth service-account JWT exchange; only an explicitly supplied file is read.
use super::{AccessToken, CredentialError, TokenFuture, TokenSource};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime},
};

pub type ExchangeFuture<'a> = Pin<
    Box<dyn Future<Output = std::result::Result<ExchangeResponse, CredentialError>> + Send + 'a>,
>;
/// A bounded token endpoint response. No Debug implementation exposes its body.
#[non_exhaustive]
pub struct ExchangeResponse {
    status: u16,
    body: Vec<u8>,
}
impl ExchangeResponse {
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self { status, body }
    }
    fn token(self) -> std::result::Result<AccessToken, CredentialError> {
        if !(200..300).contains(&self.status) {
            return Err(if self.status == 429 || self.status >= 500 {
                CredentialError::Unavailable
            } else {
                CredentialError::Rejected
            });
        }
        if self.body.len() > 64 * 1024 {
            return Err(CredentialError::InvalidToken);
        }
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            expires_in: u64,
            token_type: String,
        }
        let token: Token =
            serde_json::from_slice(&self.body).map_err(|_| CredentialError::InvalidToken)?;
        if !token.token_type.eq_ignore_ascii_case("Bearer") {
            return Err(CredentialError::InvalidToken);
        }
        AccessToken::new(&token.access_token, Duration::from_secs(token.expires_in))
    }
}
/// A caller may supply its own token networking policy or broker. The assertion is secret.
pub trait TokenExchange: Send + Sync {
    fn exchange(&self, request: reqwest::Request) -> ExchangeFuture<'_>;
}
#[derive(Clone)]
#[non_exhaustive]
pub struct ServiceAccount {
    file: PathBuf,
    exchange: Arc<dyn TokenExchange>,
}
impl ServiceAccount {
    pub fn new(file: PathBuf) -> Self {
        Self {
            file,
            exchange: Arc::new(GoogleExchange),
        }
    }
    /// Custom networking remains explicit and never consults application-default credentials.
    pub fn with_exchange(mut self, exchange: Arc<dyn TokenExchange>) -> Self {
        self.exchange = exchange;
        self
    }
}
#[derive(Deserialize)]
struct Account {
    #[serde(rename = "type")]
    kind: String,
    client_email: String,
    private_key: String,
    private_key_id: Option<String>,
    token_uri: String,
}
impl TokenSource for ServiceAccount {
    fn token(&self) -> TokenFuture<'_> {
        Box::pin(async {
            use tokio::io::AsyncReadExt;
            let file = tokio::fs::File::open(&self.file)
                .await
                .map_err(|_| CredentialError::FileUnavailable)?;
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| CredentialError::FileUnavailable)?;
            if bytes.len() > 1024 * 1024 {
                return Err(CredentialError::InvalidServiceAccount);
            }
            let account: Account = serde_json::from_slice(&bytes)
                .map_err(|_| CredentialError::InvalidServiceAccount)?;
            if account.kind != "service_account"
                || account.client_email.is_empty()
                || account.token_uri != "https://oauth2.googleapis.com/token"
            {
                return Err(CredentialError::InvalidServiceAccount);
            }
            let assertion = account.assertion()?;
            let http = scoped_client()?;
            let request = http
                .post("https://oauth2.googleapis.com/token")
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ("assertion", assertion.as_str()),
                ])
                .build()
                .map_err(|_| CredentialError::InvalidServiceAccount)?;
            self.exchange.exchange(request).await?.token()
        })
    }
}
impl Account {
    fn assertion(&self) -> std::result::Result<String, CredentialError> {
        let invalid = || CredentialError::InvalidServiceAccount;
        let pem = self
            .private_key
            .strip_prefix("-----BEGIN PRIVATE KEY-----")
            .and_then(|s| s.trim().strip_suffix("-----END PRIVATE KEY-----"))
            .ok_or_else(invalid)?;
        let der = STANDARD
            .decode(pem.split_whitespace().collect::<String>())
            .map_err(|_| invalid())?;
        let key = ring::signature::RsaKeyPair::from_pkcs8(&der).map_err(|_| invalid())?;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| invalid())?
            .as_secs();
        let mut header = json!({"alg":"RS256","typ":"JWT"});
        if let Some(kid) = &self.private_key_id {
            header["kid"] = json!(kid);
        }
        let claims = json!({"iss":self.client_email,"aud":self.token_uri,
            "scope":"https://www.googleapis.com/auth/cloud-platform","iat":now,"exp":now+3600});
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; key.public().modulus_len()];
        key.sign(
            &ring::signature::RSA_PKCS1_SHA256,
            &ring::rand::SystemRandom::new(),
            input.as_bytes(),
            &mut signature,
        )
        .map_err(|_| invalid())?;
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}
fn scoped_client() -> std::result::Result<reqwest::Client, CredentialError> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| CredentialError::Unavailable)
}
struct GoogleExchange;
impl TokenExchange for GoogleExchange {
    fn exchange(&self, request: reqwest::Request) -> ExchangeFuture<'_> {
        Box::pin(async move {
            let mut response = scoped_client()?.execute(request).await.map_err(|error| {
                if error.is_timeout() {
                    CredentialError::Timeout
                } else {
                    CredentialError::Unavailable
                }
            })?;
            let status = response.status().as_u16();
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| CredentialError::Unavailable)?
            {
                if chunk.len() > (64 * 1024usize).saturating_sub(body.len()) {
                    return Err(CredentialError::InvalidToken);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(ExchangeResponse::new(status, body))
        })
    }
}

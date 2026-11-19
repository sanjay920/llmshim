//! Google Cloud Storage over its JSON API (note 0050, slice 3b): four plain HTTP calls with a bearer
//! token, no SDK. The token comes from the GKE or GCE **metadata server** (Workload Identity: the pod's
//! Kubernetes service account is the Google identity), from a **service-account key file**, or, for an
//! emulator or a private endpoint that does no authentication, from nowhere.

use super::s3::{read_body, status_error};
use super::sigv4::encode;
use super::tokens::TokenHolder;
use super::{valid_name, BlobError, BlobStore, Listed};
use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const DEFAULT_API: &str = "https://storage.googleapis.com";
const DEFAULT_METADATA: &str = "http://metadata.google.internal";
const SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";
const ATTEMPTS: usize = 3;

/// Where the bearer token comes from.
pub enum GcsCredentials<'a> {
    /// The metadata server of GKE or GCE (Workload Identity).
    Metadata,
    /// A service-account key file (JSON), read here and at each token fetch.
    ServiceAccount(&'a Path),
    /// No authentication: an emulator, or a private endpoint (the endpoint must be set).
    None,
}

pub struct GcsSettings<'a> {
    pub bucket: &'a str,
    pub prefix: &'a str,
    /// The API origin, `https://host[:port]`; Google's own when `None`.
    pub endpoint: Option<&'a str>,
    /// The metadata server's origin; `http://metadata.google.internal` when `None`.
    pub metadata_endpoint: Option<&'a str>,
    pub credentials: GcsCredentials<'a>,
    pub ca_pem: Option<&'a [u8]>,
    pub max_object_bytes: usize,
}

enum Auth {
    None,
    Metadata {
        endpoint: String,
        holder: TokenHolder,
    },
    ServiceAccount {
        file: PathBuf,
        holder: TokenHolder,
    },
}

pub struct GcsStore {
    client: reqwest::Client,
    api: String,
    bucket: String,
    prefix: String,
    auth: Auth,
    max_object_bytes: usize,
}

/// The fields of a service-account key file that are used.
#[derive(Deserialize)]
struct Account {
    #[serde(rename = "type")]
    kind: String,
    client_email: String,
    private_key: String,
    private_key_id: Option<String>,
    token_uri: String,
}

/// Why `uri` cannot be a token endpoint, if it cannot: https, or http only to a loopback literal.
fn token_uri_problem(uri: &str) -> Option<&'static str> {
    let Ok(url) = reqwest::Url::parse(uri) else {
        return Some("is not a valid URL");
    };
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Some("must carry no credentials or fragment");
    }
    match url.scheme() {
        "https" => None,
        "http" => {
            let loopback = url
                .host_str()
                .unwrap_or("")
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
            (!loopback).then_some("must be https (http only to a loopback address)")
        }
        _ => Some("must be an https URL"),
    }
}

fn read_account(file: &Path) -> Result<Account, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .and_then(|f| f.take(1024 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("cannot read {}: {}", file.display(), e.kind()))?;
    if bytes.len() > 1024 * 1024 {
        return Err(format!("{} is larger than 1 MiB", file.display()));
    }
    let account: Account = serde_json::from_slice(&bytes)
        .map_err(|_| format!("{} is not a service-account key file", file.display()))?;
    if account.kind != "service_account" || account.client_email.is_empty() {
        return Err(format!(
            "{} is not a service-account key file",
            file.display()
        ));
    }
    if let Some(problem) = token_uri_problem(&account.token_uri) {
        return Err(format!("the key file's token_uri {problem}"));
    }
    Ok(account)
}

impl Account {
    /// The signed JWT that is traded at the token endpoint for an access token.
    fn assertion(&self) -> Result<String, BlobError> {
        let pem = self
            .private_key
            .strip_prefix("-----BEGIN PRIVATE KEY-----")
            .and_then(|s| s.trim().strip_suffix("-----END PRIVATE KEY-----"))
            .ok_or(BlobError::Denied)?;
        let der = STANDARD
            .decode(pem.split_whitespace().collect::<String>())
            .map_err(|_| BlobError::Denied)?;
        let key = ring::signature::RsaKeyPair::from_pkcs8(&der).map_err(|_| BlobError::Denied)?;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| BlobError::Unavailable)?
            .as_secs();
        let mut header = json!({"alg": "RS256", "typ": "JWT"});
        if let Some(kid) = &self.private_key_id {
            header["kid"] = json!(kid);
        }
        let claims = json!({"iss": self.client_email, "aud": self.token_uri, "scope": SCOPE,
            "iat": now, "exp": now + 3600});
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
        .map_err(|_| BlobError::Denied)?;
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

/// `{"access_token", "expires_in"}` of a token endpoint's or the metadata server's answer.
fn parse_token(body: &[u8]) -> Result<(String, Duration), BlobError> {
    #[derive(Deserialize)]
    struct Token {
        access_token: String,
        expires_in: u64,
    }
    let token: Token = serde_json::from_slice(body).map_err(|_| BlobError::Unavailable)?;
    // Sent in a header: visible ASCII, no space.
    let usable = !token.access_token.is_empty()
        && token.access_token.len() <= 8192
        && token
            .access_token
            .bytes()
            .all(|b| (b'!'..=b'~').contains(&b))
        && token.expires_in > 0;
    usable
        // Clamped: a lifetime of years is a misbehaving endpoint, and is never added to a clock.
        .then(|| {
            (
                token.access_token,
                Duration::from_secs(token.expires_in.min(24 * 3600)),
            )
        })
        .ok_or(BlobError::Unavailable)
}

impl GcsStore {
    pub fn new(settings: &GcsSettings<'_>) -> Result<Self, String> {
        let api = match settings.endpoint {
            Some(endpoint) => endpoint.trim_end_matches('/').to_owned(),
            None => DEFAULT_API.to_owned(),
        };
        if matches!(settings.credentials, GcsCredentials::None) && settings.endpoint.is_none() {
            return Err(
                "credentials = \"none\" needs an endpoint: it is for an emulator or a private \
                endpoint, never for Google's own"
                    .into(),
            );
        }
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120));
        if let Some(pem) = settings.ca_pem {
            let certificates = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|_| "ca_file holds no readable certificate".to_string())?;
            if certificates.is_empty() {
                return Err("ca_file holds no certificate".into());
            }
            builder = builder.tls_built_in_root_certs(false);
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let auth = match &settings.credentials {
            GcsCredentials::None => Auth::None,
            GcsCredentials::Metadata => Auth::Metadata {
                endpoint: settings
                    .metadata_endpoint
                    .unwrap_or(DEFAULT_METADATA)
                    .trim_end_matches('/')
                    .to_owned(),
                holder: TokenHolder::default(),
            },
            GcsCredentials::ServiceAccount(file) => {
                // Read and checked now, so a wrong path or a key that is not one fails at start.
                let account = read_account(file)?;
                account.assertion().map_err(|_| {
                    "the key file's private_key is not a usable RSA key (it must be PKCS8, \
                     `BEGIN PRIVATE KEY`: convert a PKCS1 `BEGIN RSA PRIVATE KEY` with \
                     `openssl pkcs8 -topk8 -nocrypt`)"
                        .to_string()
                })?;
                Auth::ServiceAccount {
                    file: file.to_path_buf(),
                    holder: TokenHolder::default(),
                }
            }
        };
        Ok(Self {
            client: builder
                .build()
                .map_err(|_| "cannot build the HTTP client".to_string())?,
            api,
            bucket: settings.bucket.to_owned(),
            prefix: settings.prefix.to_owned(),
            auth,
            max_object_bytes: settings.max_object_bytes,
        })
    }

    async fn fetch_token(&self) -> Result<Option<String>, BlobError> {
        match &self.auth {
            Auth::None => Ok(None),
            Auth::Metadata { endpoint, holder } => holder
                .get(|| async {
                    let response = self
                        .client
                        .get(format!(
                            "{endpoint}/computeMetadata/v1/instance/service-accounts/default/token"
                        ))
                        .header("Metadata-Flavor", "Google")
                        .timeout(Duration::from_secs(10))
                        .send()
                        .await
                        .map_err(|_| BlobError::Unavailable)?;
                    if !response.status().is_success() {
                        return Err(BlobError::Unavailable);
                    }
                    // Only the metadata server sets this: another service answering on that address
                    // is not one (as Google's own clients check).
                    if response
                        .headers()
                        .get("metadata-flavor")
                        .is_none_or(|v| v != "Google")
                    {
                        return Err(BlobError::Unavailable);
                    }
                    parse_token(&read_body(response, 64 * 1024).await?)
                })
                .await
                .map(Some),
            Auth::ServiceAccount { file, holder } => holder
                .get(|| async {
                    // Read at each fetch: a rotated key file is picked up without a restart.
                    let account = read_account(file).map_err(|_| BlobError::Unavailable)?;
                    let assertion = account.assertion()?;
                    let response = self
                        .client
                        .post(&account.token_uri)
                        .form(&[
                            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                            ("assertion", assertion.as_str()),
                        ])
                        .timeout(Duration::from_secs(30))
                        .send()
                        .await
                        .map_err(|_| BlobError::Unavailable)?;
                    let status = response.status().as_u16();
                    if status == 429 || status >= 500 {
                        return Err(BlobError::Unavailable);
                    }
                    if !(200..300).contains(&status) {
                        // The provider rejected this identity: not something to retry.
                        return Err(BlobError::Denied);
                    }
                    parse_token(&read_body(response, 64 * 1024).await?)
                })
                .await
                .map(Some),
        }
    }

    async fn invalidate(&self, refused: &str) {
        match &self.auth {
            Auth::None => {}
            Auth::Metadata { holder, .. } | Auth::ServiceAccount { holder, .. } => {
                holder.invalidate(refused).await;
            }
        }
    }

    fn object_url(&self, name: &str) -> String {
        format!(
            "{}/storage/v1/b/{}/o/{}",
            self.api,
            encode(&self.bucket, false),
            encode(&format!("{}{}", self.prefix, name), false)
        )
    }

    /// One call with the token, retried on a transport failure or a server error, and once more with
    /// a fresh token if the first was refused (401).
    async fn call(
        &self,
        build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
        limit: usize,
    ) -> Result<(u16, Vec<u8>), BlobError> {
        let mut last = BlobError::Unavailable;
        let mut refreshed = false;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
            }
            let token = self.fetch_token().await?;
            let mut request = build(&self.client);
            if let Some(token) = &token {
                let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| BlobError::Denied)?;
                value.set_sensitive(true);
                request = request.header(reqwest::header::AUTHORIZATION, value);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(_) => {
                    last = BlobError::Unavailable;
                    continue;
                }
            };
            let status = response.status().as_u16();
            if status >= 500 || status == 429 {
                last = BlobError::Unavailable;
                continue;
            }
            if status == 401 && !refreshed {
                refreshed = true;
                if let Some(token) = &token {
                    self.invalidate(token).await;
                }
                continue;
            }
            if response.content_length().is_some_and(|n| n > limit as u64) {
                return Err(BlobError::TooLarge);
            }
            match read_body(response, limit).await {
                Ok(bytes) => return Ok((status, bytes)),
                Err(BlobError::TooLarge) => return Err(BlobError::TooLarge),
                Err(error) => last = error,
            }
        }
        Err(last)
    }
}

#[async_trait]
impl BlobStore for GcsStore {
    async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<(), BlobError> {
        if !valid_name(name) {
            return Err(BlobError::Invalid);
        }
        let url = format!(
            "{}/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            self.api,
            encode(&self.bucket, false),
            encode(&format!("{}{}", self.prefix, name), false)
        );
        let (status, _) = self
            .call(
                |client| {
                    client
                        .post(&url)
                        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                        .body(bytes.clone())
                },
                64 * 1024,
            )
            .await?;
        match status {
            200 | 201 => Ok(()),
            status => Err(status_error(status)),
        }
    }

    async fn get(&self, name: &str, max: usize) -> Result<Vec<u8>, BlobError> {
        if !valid_name(name) {
            return Err(BlobError::Invalid);
        }
        let url = format!("{}?alt=media", self.object_url(name));
        match self.call(|client| client.get(&url), max).await? {
            (200, bytes) => Ok(bytes),
            (status, _) => Err(status_error(status)),
        }
    }

    async fn delete(&self, name: &str) -> Result<(), BlobError> {
        if !valid_name(name) {
            return Err(BlobError::Invalid);
        }
        let url = self.object_url(name);
        match self.call(|client| client.delete(&url), 64 * 1024).await? {
            (200 | 204 | 404, _) => Ok(()),
            (status, _) => Err(status_error(status)),
        }
    }

    async fn list(&self) -> Result<Vec<Listed>, BlobError> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..10_000 {
            let mut url = format!(
                "{}/storage/v1/b/{}/o?prefix={}&maxResults=1000&fields={}",
                self.api,
                encode(&self.bucket, false),
                encode(&self.prefix, false),
                encode("items(name,updated),nextPageToken", false)
            );
            if let Some(token) = &token {
                url.push_str(&format!("&pageToken={}", encode(token, false)));
            }
            let (status, body) = self.call(|client| client.get(&url), 8 << 20).await?;
            if status != 200 {
                return Err(status_error(status));
            }
            let page: Value = serde_json::from_slice(&body).map_err(|_| BlobError::Unavailable)?;
            for item in page["items"].as_array().into_iter().flatten() {
                let (Some(key), Some(updated)) = (item["name"].as_str(), item["updated"].as_str())
                else {
                    continue;
                };
                let Ok(modified) = chrono::DateTime::parse_from_rfc3339(updated) else {
                    continue;
                };
                if let Some(name) = key.strip_prefix(&self.prefix) {
                    out.push(Listed {
                        key: name.to_owned(),
                        modified: SystemTime::from(modified.with_timezone(&chrono::Utc)),
                    });
                }
            }
            match page["nextPageToken"].as_str() {
                Some(next) => token = Some(next.to_owned()),
                None => return Ok(out),
            }
        }
        Err(BlobError::Unavailable)
    }

    fn max_object_bytes(&self) -> usize {
        self.max_object_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Matcher;

    const RSA_DER_B64: &str = include_str!("../jwt_test_rsa_2048.b64");

    fn store(api: &str, prefix: &str) -> GcsStore {
        GcsStore::new(&GcsSettings {
            bucket: "media",
            prefix,
            endpoint: Some(api),
            metadata_endpoint: None,
            credentials: GcsCredentials::None,
            ca_pem: None,
            max_object_bytes: 1 << 20,
        })
        .unwrap()
    }

    fn pem() -> String {
        let der = RSA_DER_B64.split_whitespace().collect::<String>();
        format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            der
        )
    }

    fn key_file(token_uri: &str, key: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("llmshim-gcs-sa-{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            json!({"type": "service_account", "client_email": "media@proj.iam.example",
                "private_key": key, "private_key_id": "kid-1", "token_uri": token_uri})
            .to_string(),
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn the_four_calls_use_the_json_api_with_the_name_encoded_as_one_segment() {
        let mut server = mockito::Server::new_async().await;
        let put = server
            .mock("POST", "/upload/storage/v1/b/media/o")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("uploadType".into(), "media".into()),
                Matcher::UrlEncoded("name".into(), "llmshim/20260101/ab".into()),
            ]))
            .match_header("content-type", "application/octet-stream")
            .match_body("ciphertext")
            .with_status(200)
            .with_body("{}")
            .expect(1)
            .create_async()
            .await;
        // The name is one path segment: its slashes are %2F.
        let get = server
            .mock("GET", "/storage/v1/b/media/o/llmshim%2F20260101%2Fab")
            .match_query(Matcher::UrlEncoded("alt".into(), "media".into()))
            .with_body("ciphertext")
            .expect(1)
            .create_async()
            .await;
        let delete = server
            .mock("DELETE", "/storage/v1/b/media/o/llmshim%2F20260101%2Fab")
            .with_status(204)
            .expect(1)
            .create_async()
            .await;
        let store = store(&server.url(), "llmshim/");
        store
            .put("20260101/ab", b"ciphertext".to_vec())
            .await
            .unwrap();
        assert_eq!(store.get("20260101/ab", 100).await.unwrap(), b"ciphertext");
        store.delete("20260101/ab").await.unwrap();
        put.assert_async().await;
        get.assert_async().await;
        delete.assert_async().await;
    }

    #[tokio::test]
    async fn each_failure_class_is_named_a_server_error_is_retried_and_a_401_is_tried_once_more() {
        let mut server = mockito::Server::new_async().await;
        let store = store(&server.url(), "");
        for (status, expected, hits) in [
            (404, BlobError::NotFound, 1),
            (403, BlobError::Denied, 1),
            (400, BlobError::Invalid, 1),
            // A 401 is tried once more (the token may have expired), then is a refusal.
            (401, BlobError::Denied, 2),
        ] {
            let mock = server
                .mock("GET", "/storage/v1/b/media/o/a%2Fb")
                .match_query(Matcher::Any)
                .with_status(status)
                .with_body(r#"{"error": {"message": "secret details"}}"#)
                .expect(hits)
                .create_async()
                .await;
            assert_eq!(store.get("a/b", 100).await, Err(expected), "{status}");
            mock.assert_async().await;
            mock.remove_async().await;
        }
        let down = server
            .mock("GET", "/storage/v1/b/media/o/a%2Fb")
            .match_query(Matcher::Any)
            .with_status(503)
            .expect(3)
            .create_async()
            .await;
        assert_eq!(store.get("a/b", 100).await, Err(BlobError::Unavailable));
        down.assert_async().await;
        down.remove_async().await;
        let _large = server
            .mock("GET", "/storage/v1/b/media/o/a%2Fc")
            .match_query(Matcher::Any)
            .with_body("x".repeat(500))
            .create_async()
            .await;
        assert_eq!(store.get("a/c", 100).await, Err(BlobError::TooLarge));
    }

    #[tokio::test]
    async fn a_listing_is_paged_the_prefix_removed_and_the_page_token_encoded() {
        let mut server = mockito::Server::new_async().await;
        let first = server
            .mock("GET", "/storage/v1/b/media/o")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("prefix".into(), "llmshim/".into()),
                Matcher::UrlEncoded("maxResults".into(), "1000".into()),
                Matcher::UrlEncoded("fields".into(), "items(name,updated),nextPageToken".into()),
            ]))
            .with_body(
                r#"{"items": [{"name": "llmshim/20260101/a", "updated": "2026-01-01T00:00:00.000Z"},
                    {"name": "llmshim/20260101/nodate"}],
                    "nextPageToken": "tok/+="}"#,
            )
            .expect(1)
            .create_async()
            .await;
        let second = server
            .mock("GET", "/storage/v1/b/media/o")
            .match_query(Matcher::UrlEncoded("pageToken".into(), "tok/+=".into()))
            .with_body(
                r#"{"items": [{"name": "llmshim/20260102/b", "updated": "2026-01-02T03:04:05Z"},
                    {"name": "other/x", "updated": "2026-01-02T03:04:05Z"}]}"#,
            )
            .expect(1)
            .create_async()
            .await;
        let listed = store(&server.url(), "llmshim/").list().await.unwrap();
        let keys: Vec<&str> = listed.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["20260101/a", "20260102/b"]);
        first.assert_async().await;
        second.assert_async().await;
    }

    #[tokio::test]
    async fn the_metadata_server_gives_a_token_that_is_kept_and_a_failure_is_remembered() {
        let mut server = mockito::Server::new_async().await;
        let token = server
            .mock(
                "GET",
                "/computeMetadata/v1/instance/service-accounts/default/token",
            )
            .match_header("metadata-flavor", "Google")
            .with_header("content-type", "application/json")
            .with_header("metadata-flavor", "Google")
            .with_body(
                r#"{"access_token": "ya29.wi-token", "expires_in": 3599, "token_type": "Bearer"}"#,
            )
            .expect(1)
            .create_async()
            .await;
        let api = mockito::Server::new_async().await;
        let mut api = api;
        let object = api
            .mock("GET", "/storage/v1/b/media/o/a%2Fb")
            .match_query(Matcher::Any)
            .match_header("authorization", "Bearer ya29.wi-token")
            .with_body("data")
            .expect(3)
            .create_async()
            .await;
        let store = GcsStore::new(&GcsSettings {
            bucket: "media",
            prefix: "",
            endpoint: Some(&api.url()),
            metadata_endpoint: Some(&server.url()),
            credentials: GcsCredentials::Metadata,
            ca_pem: None,
            max_object_bytes: 1024,
        })
        .unwrap();
        for _ in 0..3 {
            assert_eq!(store.get("a/b", 100).await.unwrap(), b"data");
        }
        token.assert_async().await;
        object.assert_async().await;
        // A metadata server that is down is unavailable, and remembered for a few seconds.
        let mut down = mockito::Server::new_async().await;
        let hits = down
            .mock("GET", Matcher::Any)
            .with_status(500)
            .expect(1)
            .create_async()
            .await;
        let store = GcsStore::new(&GcsSettings {
            bucket: "media",
            prefix: "",
            endpoint: Some(&api.url()),
            metadata_endpoint: Some(&down.url()),
            credentials: GcsCredentials::Metadata,
            ca_pem: None,
            max_object_bytes: 1024,
        })
        .unwrap();
        assert_eq!(store.get("a/b", 100).await, Err(BlobError::Unavailable));
        assert_eq!(store.get("a/b", 100).await, Err(BlobError::Unavailable));
        hits.assert_async().await;
    }

    #[tokio::test]
    async fn an_answer_without_the_metadata_flavor_header_is_not_the_metadata_server() {
        let mut impostor = mockito::Server::new_async().await;
        let _answer = impostor
            .mock("GET", Matcher::Any)
            .with_header("content-type", "application/json")
            .with_body(r#"{"access_token": "stolen", "expires_in": 3600}"#)
            .create_async()
            .await;
        // The storage API would accept the impostor's token: it must never be sent.
        let mut api = mockito::Server::new_async().await;
        let object = api
            .mock("GET", Matcher::Any)
            .match_header("authorization", "Bearer stolen")
            .with_body("data")
            .expect(0)
            .create_async()
            .await;
        let store = GcsStore::new(&GcsSettings {
            bucket: "media",
            prefix: "",
            endpoint: Some(&api.url()),
            metadata_endpoint: Some(&impostor.url()),
            credentials: GcsCredentials::Metadata,
            ca_pem: None,
            max_object_bytes: 1024,
        })
        .unwrap();
        assert_eq!(store.get("a/b", 100).await, Err(BlobError::Unavailable));
        object.assert_async().await;
    }

    #[tokio::test]
    async fn a_service_account_signs_a_jwt_the_token_endpoint_trades_for_an_access_token() {
        let mut token_server = mockito::Server::new_async().await;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let capture = seen.clone();
        let exchange = token_server
            .mock("POST", "/token")
            .match_header("content-type", "application/x-www-form-urlencoded")
            .with_header("content-type", "application/json")
            .with_body_from_request(move |request| {
                *capture.lock().unwrap() =
                    String::from_utf8_lossy(request.body().unwrap()).into_owned();
                br#"{"access_token": "ya29.sa-token", "expires_in": 3600, "token_type": "Bearer"}"#
                    .to_vec()
            })
            .expect(1)
            .create_async()
            .await;
        let mut api = mockito::Server::new_async().await;
        let object = api
            .mock("GET", "/storage/v1/b/media/o/a%2Fb")
            .match_query(Matcher::Any)
            .match_header("authorization", "Bearer ya29.sa-token")
            .with_body("data")
            .expect(1)
            .create_async()
            .await;
        let file = key_file(&format!("{}/token", token_server.url()), &pem());
        let store = GcsStore::new(&GcsSettings {
            bucket: "media",
            prefix: "",
            endpoint: Some(&api.url()),
            metadata_endpoint: None,
            credentials: GcsCredentials::ServiceAccount(&file),
            ca_pem: None,
            max_object_bytes: 1024,
        })
        .unwrap();
        assert_eq!(store.get("a/b", 100).await.unwrap(), b"data");
        exchange.assert_async().await;
        object.assert_async().await;
        // The form is a JWT-bearer grant whose assertion is signed by the key and says who, for whom,
        // and what for, with a one-hour life.
        let form = seen.lock().unwrap().clone();
        let field = |name: &str| {
            form.split('&')
                .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
                .map(|v| v.replace("%3A", ":"))
                .unwrap()
        };
        assert_eq!(
            field("grant_type"),
            "urn:ietf:params:oauth:grant-type:jwt-bearer"
        );
        let assertion = field("assertion");
        let parts: Vec<&str> = assertion.split('.').collect();
        let decode = |part: &str| -> Value {
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
        };
        let (header, claims) = (decode(parts[0]), decode(parts[1]));
        assert_eq!(
            (header["alg"].as_str(), header["kid"].as_str()),
            (Some("RS256"), Some("kid-1"))
        );
        assert_eq!(claims["iss"], "media@proj.iam.example");
        assert_eq!(claims["aud"], format!("{}/token", token_server.url()));
        assert_eq!(claims["scope"], SCOPE);
        assert_eq!(
            claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
            3600
        );
        let der = STANDARD
            .decode(RSA_DER_B64.split_whitespace().collect::<String>())
            .unwrap();
        let pair = ring::signature::RsaKeyPair::from_pkcs8(&der).unwrap();
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            pair.public().as_ref(),
        )
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
        )
        .expect("the assertion verifies against the key's public half");
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn a_bad_key_file_or_an_unsafe_setting_fails_at_start_without_echoing_the_key() {
        let settings = |credentials, endpoint| GcsSettings {
            bucket: "media",
            prefix: "",
            endpoint,
            metadata_endpoint: None,
            credentials,
            ca_pem: None,
            max_object_bytes: 1024,
        };
        // Not a key file, a wrong type, a token endpoint that is not https, an RSA key that is not one.
        let junk = std::env::temp_dir().join(format!("llmshim-gcs-junk-{}", uuid::Uuid::new_v4()));
        std::fs::write(&junk, "hunter2-not-json").unwrap();
        let wrong_type = key_file("https://oauth2.example/token", &pem());
        std::fs::write(
            &wrong_type,
            json!({"type": "authorized_user", "client_email": "x", "private_key": pem(),
                "token_uri": "https://oauth2.example/token"})
            .to_string(),
        )
        .unwrap();
        let plain_http = key_file("http://oauth2.example/token", &pem());
        let bad_key = key_file(
            "https://oauth2.example/token",
            "-----BEGIN PRIVATE KEY-----\nhunter2AAAA\n-----END PRIVATE KEY-----\n",
        );
        for file in [&junk, &wrong_type, &plain_http, &bad_key] {
            let error = GcsStore::new(&settings(GcsCredentials::ServiceAccount(file), None))
                .err()
                .expect("refused");
            assert!(!error.contains("hunter2"), "{error}");
        }
        assert!(GcsStore::new(&settings(
            GcsCredentials::ServiceAccount(std::path::Path::new("/nonexistent/key.json")),
            None
        ))
        .is_err());
        // No authentication is only for an endpoint that is not Google's.
        assert!(GcsStore::new(&settings(GcsCredentials::None, None)).is_err());
        assert!(GcsStore::new(&settings(
            GcsCredentials::None,
            Some("http://127.0.0.1:4443")
        ))
        .is_ok());
        for file in [junk, wrong_type, plain_http, bad_key] {
            let _ = std::fs::remove_file(file);
        }
    }

    #[test]
    fn a_token_answer_is_checked_before_it_is_used_in_a_header() {
        assert!(parse_token(br#"{"access_token": "ya29.ok", "expires_in": 60}"#).is_ok());
        for bad in [
            &br#"{"access_token": "has space", "expires_in": 60}"#[..],
            br#"{"access_token": "line\nbreak", "expires_in": 60}"#,
            br#"{"access_token": "", "expires_in": 60}"#,
            br#"{"access_token": "ok", "expires_in": 0}"#,
            br#"{"expires_in": 60}"#,
            b"not json",
        ] {
            assert_eq!(parse_token(bad), Err(BlobError::Unavailable), "{bad:?}");
        }
    }
}

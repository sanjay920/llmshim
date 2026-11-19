//! Azure Blob Storage over its REST API (note 0050, slice 3c): four plain HTTP calls, no SDK. The
//! credential is a **SAS token**, the **account key** (Shared Key signing, HMAC-SHA256, with `ring`), or
//! a Microsoft Entra **bearer token**: Azure AD Workload Identity (the AKS pod's federated service
//! account token traded at the Entra token endpoint) or a managed identity (the instance metadata
//! service). Objects are sealed as for every backend, so the container holds ciphertext.

use super::s3::{between, read_body, status_error, unescape, Source};
use super::sigv4::encode;
use super::tokens::TokenHolder;
use super::{valid_name, BlobError, BlobStore, Listed};
use crate::serve::config::SecretRef;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ring::hmac;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const API_VERSION: &str = "2021-12-02";
const STORAGE_SCOPE: &str = "https://storage.azure.com/.default";
const STORAGE_RESOURCE: &str = "https://storage.azure.com/";
const DEFAULT_AUTHORITY: &str = "https://login.microsoftonline.com";
const DEFAULT_IMDS: &str = "http://169.254.169.254";
const ATTEMPTS: usize = 3;

/// How requests are authorized.
pub enum AzureAuth<'a> {
    /// A SAS token for the container (read, write, delete and list), by secret reference.
    Sas(&'a SecretRef),
    /// The storage account's key (base64), by secret reference.
    AccountKey(&'a SecretRef),
    /// Azure AD Workload Identity: the federated token file the kubelet projects, traded for a token.
    WorkloadIdentity {
        tenant_id: &'a str,
        client_id: &'a str,
        token_file: &'a Path,
        authority: Option<&'a str>,
    },
    /// A managed identity, through the instance metadata service.
    ManagedIdentity {
        /// For a user-assigned identity; omitted for the system-assigned one.
        client_id: Option<&'a str>,
        endpoint: Option<&'a str>,
    },
}

pub struct AzureSettings<'a> {
    pub account: &'a str,
    pub container: &'a str,
    pub prefix: &'a str,
    /// The service address, `https://<account>.blob.core.windows.net` by default; an emulator's is
    /// `http://127.0.0.1:10000/devstoreaccount1` (the account in the path).
    pub endpoint: Option<&'a str>,
    pub auth: AzureAuth<'a>,
    pub ca_pem: Option<&'a [u8]>,
    pub max_object_bytes: usize,
}

enum Auth {
    Sas(Source),
    Key(Source),
    Workload {
        tenant_id: String,
        client_id: String,
        token_file: PathBuf,
        authority: String,
        holder: TokenHolder,
    },
    Managed {
        client_id: Option<String>,
        endpoint: String,
        holder: TokenHolder,
    },
}

pub struct AzureStore {
    client: reqwest::Client,
    account: String,
    container: String,
    prefix: String,
    base: String,
    auth: Auth,
    max_object_bytes: usize,
}

/// What authorizes one request.
enum Credential {
    Sas(String),
    Key(Vec<u8>),
    Bearer(String),
}

fn https_or_loopback(uri: &str) -> Option<&'static str> {
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

/// `{"access_token", "expires_in"}`: Entra returns the lifetime as a number and the metadata service as
/// a string.
fn parse_token(body: &[u8]) -> Result<(String, Duration), BlobError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| BlobError::Unavailable)?;
    let token = value["access_token"].as_str().unwrap_or_default();
    let lifetime = match &value["expires_in"] {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
    .unwrap_or(0);
    let usable = !token.is_empty()
        && token.len() <= 8192
        && token.bytes().all(|b| (b'!'..=b'~').contains(&b))
        && lifetime > 0;
    usable
        .then(|| {
            (
                token.to_owned(),
                Duration::from_secs(lifetime.min(24 * 3600)),
            )
        })
        .ok_or(BlobError::Unavailable)
}

fn sas_problem(sas: &str) -> Option<&'static str> {
    (sas.is_empty()
        || sas.len() > 4096
        || !sas.bytes().all(|b| (b'!'..=b'~').contains(&b))
        || sas.contains('#')
        || sas.starts_with('?'))
    .then_some("is not a SAS token (the query string, without the leading `?`)")
}

impl AzureStore {
    pub fn new(
        settings: &AzureSettings<'_>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let base = match settings.endpoint {
            Some(endpoint) => endpoint.trim_end_matches('/').to_owned(),
            None => format!("https://{}.blob.core.windows.net", settings.account),
        };
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
        let auth = match &settings.auth {
            AzureAuth::Sas(reference) => {
                let source = Source::new(reference, env).map_err(|m| format!("sas_token: {m}"))?;
                let value = source
                    .read()
                    .map_err(|_| "sas_token cannot be read".to_string())?;
                if let Some(problem) = sas_problem(&value) {
                    return Err(format!("sas_token {problem}"));
                }
                Auth::Sas(source)
            }
            AzureAuth::AccountKey(reference) => {
                let source =
                    Source::new(reference, env).map_err(|m| format!("account_key: {m}"))?;
                let value = source
                    .read()
                    .map_err(|_| "account_key cannot be read".to_string())?;
                if STANDARD.decode(value.trim()).is_err() {
                    return Err("account_key is not base64".into());
                }
                Auth::Key(source)
            }
            AzureAuth::WorkloadIdentity {
                tenant_id,
                client_id,
                token_file,
                authority,
            } => {
                let authority = authority.unwrap_or(DEFAULT_AUTHORITY).trim_end_matches('/');
                if let Some(problem) = https_or_loopback(authority) {
                    return Err(format!("authority_host {problem}"));
                }
                // The projected token must be there at start; it is read again at each fetch.
                read_token_file(token_file).map_err(|_| {
                    format!(
                        "cannot read the federated token file {}",
                        token_file.display()
                    )
                })?;
                Auth::Workload {
                    tenant_id: (*tenant_id).to_owned(),
                    client_id: (*client_id).to_owned(),
                    token_file: token_file.to_path_buf(),
                    authority: authority.to_owned(),
                    holder: TokenHolder::default(),
                }
            }
            AzureAuth::ManagedIdentity {
                client_id,
                endpoint,
            } => Auth::Managed {
                client_id: client_id.map(str::to_owned),
                endpoint: endpoint
                    .unwrap_or(DEFAULT_IMDS)
                    .trim_end_matches('/')
                    .to_owned(),
                holder: TokenHolder::default(),
            },
        };
        Ok(Self {
            client: builder
                .build()
                .map_err(|_| "cannot build the HTTP client".to_string())?,
            account: settings.account.to_owned(),
            container: settings.container.to_owned(),
            prefix: settings.prefix.to_owned(),
            base,
            auth,
            max_object_bytes: settings.max_object_bytes,
        })
    }

    async fn credential(&self) -> Result<Credential, BlobError> {
        match &self.auth {
            Auth::Sas(source) => Ok(Credential::Sas(source.read()?)),
            Auth::Key(source) => STANDARD
                .decode(source.read()?.trim())
                .map(Credential::Key)
                .map_err(|_| BlobError::Denied),
            Auth::Workload {
                tenant_id,
                client_id,
                token_file,
                authority,
                holder,
            } => holder
                .get(|| async {
                    let assertion =
                        read_token_file(token_file).map_err(|_| BlobError::Unavailable)?;
                    let response = self
                        .client
                        .post(format!("{authority}/{tenant_id}/oauth2/v2.0/token"))
                        .form(&[
                            ("client_id", client_id.as_str()),
                            ("scope", STORAGE_SCOPE),
                            (
                                "client_assertion_type",
                                "urn:ietf:params:oauth:client-assertion-type:jwt-bearer",
                            ),
                            ("client_assertion", assertion.as_str()),
                            ("grant_type", "client_credentials"),
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
                        // Entra rejected this federated identity: not something to retry.
                        return Err(BlobError::Denied);
                    }
                    parse_token(&read_body(response, 64 * 1024).await?)
                })
                .await
                .map(Credential::Bearer),
            Auth::Managed {
                client_id,
                endpoint,
                holder,
            } => holder
                .get(|| async {
                    let mut url = format!(
                        "{endpoint}/metadata/identity/oauth2/token?api-version=2018-02-01&resource={}",
                        encode(STORAGE_RESOURCE, false)
                    );
                    if let Some(client_id) = client_id {
                        url.push_str(&format!("&client_id={}", encode(client_id, false)));
                    }
                    let response = self
                        .client
                        .get(url)
                        .header("Metadata", "true")
                        .timeout(Duration::from_secs(10))
                        .send()
                        .await
                        .map_err(|_| BlobError::Unavailable)?;
                    if !response.status().is_success() {
                        return Err(BlobError::Unavailable);
                    }
                    parse_token(&read_body(response, 64 * 1024).await?)
                })
                .await
                .map(Credential::Bearer),
        }
    }

    async fn invalidate(&self, refused: &str) {
        match &self.auth {
            Auth::Workload { holder, .. } | Auth::Managed { holder, .. } => {
                holder.invalidate(refused).await;
            }
            // A SAS or a key is read afresh for every request.
            Auth::Sas(_) | Auth::Key(_) => {}
        }
    }

    /// The path of the container, or of a blob in it (slashes kept: they are the blob's virtual
    /// directories).
    fn path(&self, name: Option<&str>) -> String {
        match name {
            Some(name) => format!(
                "/{}/{}",
                encode(&self.container, false),
                encode(&format!("{}{}", self.prefix, name), true)
            ),
            None => format!("/{}", encode(&self.container, false)),
        }
    }

    /// One call, authorized, retried on a transport failure or a server error, and once more if the
    /// credential was refused (a rotated SAS, key or token).
    async fn call(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Vec<u8>,
        extra: &[(&str, &str)],
        limit: usize,
    ) -> Result<(u16, Vec<u8>), BlobError> {
        let mut last = BlobError::Unavailable;
        let mut refused_once = false;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
            }
            let credential = self.credential().await?;
            let mut query = query.to_vec();
            if let Credential::Sas(sas) = &credential {
                for pair in sas.split('&') {
                    if let Some((k, v)) = pair.split_once('=') {
                        query.push((decode(k), decode(v)));
                    }
                }
            }
            let mut sorted: Vec<(String, String)> = query
                .iter()
                .map(|(k, v)| (encode(k, false), encode(v, false)))
                .collect();
            sorted.sort();
            let mut url = format!("{}{path}", self.base);
            if !sorted.is_empty() {
                let pairs: Vec<String> = sorted.iter().map(|(k, v)| format!("{k}={v}")).collect();
                url.push('?');
                url.push_str(&pairs.join("&"));
            }
            let date = httpdate(SystemTime::now());
            let mut headers: Vec<(String, String)> = vec![
                ("x-ms-date".into(), date),
                ("x-ms-version".into(), API_VERSION.into()),
            ];
            headers.extend(
                extra
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
            );
            let content_type = (method == "PUT").then_some("application/octet-stream");
            let mut request = self
                .client
                .request(method.parse().map_err(|_| BlobError::Invalid)?, &url);
            let mut bearer = None;
            match &credential {
                Credential::Key(key) => {
                    let full_path = reqwest::Url::parse(&url)
                        .map_err(|_| BlobError::Invalid)?
                        .path()
                        .to_owned();
                    let signature = shared_key(
                        &self.account,
                        key,
                        method,
                        body.len(),
                        content_type.unwrap_or(""),
                        &headers,
                        &full_path,
                        &query,
                    );
                    request = request.header(
                        reqwest::header::AUTHORIZATION,
                        format!("SharedKey {}:{signature}", self.account),
                    );
                }
                Credential::Bearer(token) => {
                    let mut value =
                        reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                            .map_err(|_| BlobError::Denied)?;
                    value.set_sensitive(true);
                    request = request.header(reqwest::header::AUTHORIZATION, value);
                    bearer = Some(token.clone());
                }
                Credential::Sas(_) => {}
            }
            for (name, value) in &headers {
                request = request.header(name.as_str(), value.as_str());
            }
            if let Some(content_type) = content_type {
                request = request.header(reqwest::header::CONTENT_TYPE, content_type);
            }
            if !body.is_empty() {
                request = request.body(body.clone());
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
            if matches!(status, 401 | 403) && !refused_once {
                refused_once = true;
                if let Some(token) = &bearer {
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

fn read_token_file(path: &Path) -> Result<String, ()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(64 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|_| ())?;
    let text = String::from_utf8(bytes).map_err(|_| ())?;
    let text = text.trim();
    (!text.is_empty()
        && text.len() <= 64 * 1024
        && text.bytes().all(|b| (b'!'..=b'~').contains(&b)))
    .then(|| text.to_owned())
    .ok_or(())
}

/// `%XX` decoding of one query component.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An HTTP date (RFC 1123, GMT), as `x-ms-date` wants.
fn httpdate(at: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(at)
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

/// The Shared Key signature of a request: HMAC-SHA256, with the account key, of the canonical string
/// (the verb, the standard headers that matter here, the `x-ms-*` headers sorted, and the resource).
#[allow(clippy::too_many_arguments)]
fn shared_key(
    account: &str,
    key: &[u8],
    method: &str,
    content_length: usize,
    content_type: &str,
    headers: &[(String, String)],
    url_path: &str,
    query: &[(String, String)],
) -> String {
    let mut ms: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| k.starts_with("x-ms-"))
        .map(|(k, v)| {
            (
                k.to_ascii_lowercase(),
                v.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect();
    ms.sort();
    let canonical_headers: String = ms.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let mut params: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (k, v) in query {
        params
            .entry(k.to_ascii_lowercase())
            .or_default()
            .push(v.clone());
    }
    let mut resource = format!("/{account}{url_path}");
    for (name, mut values) in params {
        values.sort();
        resource.push_str(&format!("\n{name}:{}", values.join(",")));
    }
    let length = if content_length == 0 {
        String::new()
    } else {
        content_length.to_string()
    };
    // verb, encoding, language, length, md5, type, date, if-modified, if-match, if-none-match,
    // if-unmodified, range: only the verb, length and type are ever present.
    let string_to_sign = format!(
        "{method}\n\n\n{length}\n\n{content_type}\n\n\n\n\n\n\n{canonical_headers}{resource}"
    );
    STANDARD.encode(hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, key),
        string_to_sign.as_bytes(),
    ))
}

#[async_trait]
impl BlobStore for AzureStore {
    async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<(), BlobError> {
        if !valid_name(name) {
            return Err(BlobError::Invalid);
        }
        let path = self.path(Some(name));
        match self
            .call(
                "PUT",
                &path,
                &[],
                bytes,
                &[("x-ms-blob-type", "BlockBlob")],
                64 * 1024,
            )
            .await?
        {
            (200 | 201, _) => Ok(()),
            (status, _) => Err(status_error(status)),
        }
    }

    async fn get(&self, name: &str, max: usize) -> Result<Vec<u8>, BlobError> {
        if !valid_name(name) {
            return Err(BlobError::Invalid);
        }
        let path = self.path(Some(name));
        match self.call("GET", &path, &[], Vec::new(), &[], max).await? {
            (200, bytes) => Ok(bytes),
            (status, _) => Err(status_error(status)),
        }
    }

    async fn delete(&self, name: &str) -> Result<(), BlobError> {
        if !valid_name(name) {
            return Err(BlobError::Invalid);
        }
        let path = self.path(Some(name));
        match self
            .call(
                "DELETE",
                &path,
                &[],
                Vec::new(),
                &[("x-ms-delete-snapshots", "include")],
                64 * 1024,
            )
            .await?
        {
            (200 | 202 | 204 | 404, _) => Ok(()),
            (status, _) => Err(status_error(status)),
        }
    }

    async fn list(&self) -> Result<Vec<Listed>, BlobError> {
        let path = self.path(None);
        let mut out = Vec::new();
        let mut marker: Option<String> = None;
        for _ in 0..10_000 {
            let mut query = vec![
                ("restype".to_owned(), "container".to_owned()),
                ("comp".to_owned(), "list".to_owned()),
                ("prefix".to_owned(), self.prefix.clone()),
                ("maxresults".to_owned(), "1000".to_owned()),
            ];
            if let Some(marker) = &marker {
                query.push(("marker".to_owned(), marker.clone()));
            }
            let (status, body) = self
                .call("GET", &path, &query, Vec::new(), &[], 8 << 20)
                .await?;
            if status != 200 {
                return Err(status_error(status));
            }
            let xml = String::from_utf8_lossy(&body);
            let mut at = 0;
            while let Some((block, end)) = between(&xml, "Blob", at) {
                at = end;
                let name = between(block, "Name", 0).map(|(n, _)| unescape(n));
                let modified = between(block, "Last-Modified", 0)
                    .and_then(|(t, _)| chrono::DateTime::parse_from_rfc2822(t).ok())
                    .map(|t| SystemTime::from(t.with_timezone(&chrono::Utc)));
                if let (Some(name), Some(modified)) = (name, modified) {
                    if let Some(key) = name.strip_prefix(&self.prefix) {
                        out.push(Listed {
                            key: key.to_owned(),
                            modified,
                        });
                    }
                }
            }
            match between(&xml, "NextMarker", 0).map(|(m, _)| unescape(m)) {
                Some(next) if !next.is_empty() => marker = Some(next),
                _ => return Ok(out),
            }
        }
        Err(BlobError::Unavailable)
    }

    fn max_object_bytes(&self) -> usize {
        self.max_object_bytes
    }
}

impl AzureStore {
    /// Create the container (an emulator's starts empty). Not part of serving: a gateway never creates
    /// the storage it uses, and a container that exists answers 409, which is fine here.
    #[doc(hidden)]
    pub async fn create_container_for_tests(&self) -> Result<(), BlobError> {
        let path = self.path(None);
        let query = [("restype".to_owned(), "container".to_owned())];
        match self
            .call("PUT", &path, &query, Vec::new(), &[], 64 * 1024)
            .await?
        {
            (200 | 201 | 409, _) => Ok(()),
            (status, _) => Err(status_error(status)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Matcher;

    const KEY_B64: &str =
        "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

    fn secret(env: &str) -> SecretRef {
        SecretRef {
            env: Some(env.to_owned()),
            file: None,
        }
    }

    fn env(name: &str) -> Option<String> {
        match name {
            "KEY" => Some(KEY_B64.to_owned()),
            "SAS" => Some("sv=2021-12-02&sp=rwdl&sig=abc%2Bdef%3D".to_owned()),
            _ => None,
        }
    }

    fn store(base: &str, auth: AzureAuth<'_>, prefix: &str) -> AzureStore {
        AzureStore::new(
            &AzureSettings {
                account: "acct",
                container: "media",
                prefix,
                endpoint: Some(base),
                auth,
                ca_pem: None,
                max_object_bytes: 1 << 20,
            },
            &env,
        )
        .unwrap()
    }

    #[test]
    fn the_shared_key_string_to_sign_has_the_documented_layout() {
        // Signatures computed independently (HMAC-SHA256 over the string spelled out by hand) for the
        // layout in Microsoft's Shared Key documentation: the verb, twelve standard-header lines of
        // which only length and type are set, the sorted x-ms headers, then the resource and the
        // sorted lower-cased query parameters. (The emulator test proves a server accepts it.)
        let key = STANDARD.decode(KEY_B64).unwrap();
        let headers = vec![
            (
                "x-ms-date".to_owned(),
                "Thu, 19 Nov 2026 20:00:00 GMT".to_owned(),
            ),
            ("x-ms-version".to_owned(), "2021-12-02".to_owned()),
            ("x-ms-blob-type".to_owned(), "BlockBlob".to_owned()),
        ];
        assert_eq!(
            shared_key(
                "devstoreaccount1",
                &key,
                "PUT",
                10,
                "application/octet-stream",
                &headers,
                "/devstoreaccount1/media/p/a",
                &[],
            ),
            "Y3Rz3TWCeJ3uc6OOAyaRgScWJ9r2usjCdoYOcAyK1tE="
        );
        let headers = &headers[..2];
        let query = [
            ("restype".to_owned(), "container".to_owned()),
            ("comp".to_owned(), "list".to_owned()),
            ("prefix".to_owned(), "llmshim/".to_owned()),
            ("maxresults".to_owned(), "1000".to_owned()),
        ];
        assert_eq!(
            shared_key("acct", &key, "GET", 0, "", headers, "/media", &query),
            "QjOslNoeOCga90T2SWQgQrRnOawiuzj3furGUOA5F84="
        );
    }

    #[tokio::test]
    async fn a_sas_token_rides_every_request_and_the_blob_calls_use_the_container_path() {
        let mut server = mockito::Server::new_async().await;
        let sas = Matcher::AllOf(vec![
            Matcher::UrlEncoded("sv".into(), "2021-12-02".into()),
            Matcher::UrlEncoded("sp".into(), "rwdl".into()),
            // The token's own escapes survive: decoded once, encoded once.
            Matcher::UrlEncoded("sig".into(), "abc+def=".into()),
        ]);
        let put = server
            .mock("PUT", "/media/llmshim/20260101/ab")
            .match_query(sas.clone())
            .match_header("x-ms-blob-type", "BlockBlob")
            .match_header("x-ms-version", API_VERSION)
            .match_header("authorization", Matcher::Missing)
            .match_body("ciphertext")
            .with_status(201)
            .expect(1)
            .create_async()
            .await;
        let get = server
            .mock("GET", "/media/llmshim/20260101/ab")
            .match_query(sas.clone())
            .with_body("ciphertext")
            .expect(1)
            .create_async()
            .await;
        let delete = server
            .mock("DELETE", "/media/llmshim/20260101/ab")
            .match_query(sas)
            .match_header("x-ms-delete-snapshots", "include")
            .with_status(202)
            .expect(1)
            .create_async()
            .await;
        let sas_ref = secret("SAS");
        let store = store(&server.url(), AzureAuth::Sas(&sas_ref), "llmshim/");
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
    async fn each_failure_class_is_named_a_server_error_is_retried_and_a_refusal_tried_once_more() {
        let mut server = mockito::Server::new_async().await;
        let key = secret("KEY");
        let store = store(&server.url(), AzureAuth::AccountKey(&key), "");
        for (status, expected, hits) in [
            (404, BlobError::NotFound, 1),
            (409, BlobError::Invalid, 1),
            (403, BlobError::Denied, 2),
            (401, BlobError::Denied, 2),
        ] {
            let mock = server
                .mock("GET", "/media/a/b")
                .with_status(status)
                .with_body("<Error><Code>AuthenticationFailed</Code><Message>secret details</Message></Error>")
                .expect(hits)
                .create_async()
                .await;
            assert_eq!(store.get("a/b", 100).await, Err(expected), "{status}");
            mock.assert_async().await;
            mock.remove_async().await;
        }
        let down = server
            .mock("GET", "/media/a/b")
            .with_status(503)
            .expect(3)
            .create_async()
            .await;
        assert_eq!(store.get("a/b", 100).await, Err(BlobError::Unavailable));
        down.assert_async().await;
        down.remove_async().await;
        let _large = server
            .mock("GET", "/media/a/c")
            .with_body("x".repeat(500))
            .create_async()
            .await;
        assert_eq!(store.get("a/c", 100).await, Err(BlobError::TooLarge));
    }

    #[tokio::test]
    async fn a_listing_is_paged_by_marker_and_names_and_dates_are_read() {
        let mut server = mockito::Server::new_async().await;
        let key = secret("KEY");
        let first = server
            .mock("GET", "/media")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("restype".into(), "container".into()),
                Matcher::UrlEncoded("comp".into(), "list".into()),
                Matcher::UrlEncoded("prefix".into(), "llmshim/".into()),
            ]))
            .match_header("authorization", Matcher::Regex("^SharedKey acct:".into()))
            .with_body(
                "<EnumerationResults><Blobs>\
                 <Blob><Name>llmshim/20260101/a</Name><Properties><Last-Modified>Thu, 01 Jan 2026 00:00:00 GMT</Last-Modified></Properties></Blob>\
                 <Blob><Name>llmshim/20260101/nodate</Name><Properties></Properties></Blob>\
                 </Blobs><NextMarker>2!tok/+=</NextMarker></EnumerationResults>",
            )
            .expect(1)
            .create_async()
            .await;
        let second = server
            .mock("GET", "/media")
            .match_query(Matcher::UrlEncoded("marker".into(), "2!tok/+=".into()))
            .with_body(
                "<EnumerationResults><Blobs>\
                 <Blob><Name>llmshim/20260102/b</Name><Properties><Last-Modified>Fri, 02 Jan 2026 03:04:05 GMT</Last-Modified></Properties></Blob>\
                 <Blob><Name>other/x</Name><Properties><Last-Modified>Fri, 02 Jan 2026 03:04:05 GMT</Last-Modified></Properties></Blob>\
                 </Blobs><NextMarker /></EnumerationResults>",
            )
            .expect(1)
            .create_async()
            .await;
        let store = store(&server.url(), AzureAuth::AccountKey(&key), "llmshim/");
        let listed = store.list().await.unwrap();
        let keys: Vec<&str> = listed.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["20260101/a", "20260102/b"]);
        first.assert_async().await;
        second.assert_async().await;
    }

    #[tokio::test]
    async fn workload_identity_trades_the_projected_token_for_a_bearer_that_is_kept() {
        let dir =
            std::env::temp_dir().join(format!("llmshim-azwi-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let token_file = dir.join("token");
        std::fs::write(&token_file, "projected-federated-token\n").unwrap();
        let mut entra = mockito::Server::new_async().await;
        let exchange = entra
            .mock("POST", "/tenant-1/oauth2/v2.0/token")
            .match_body(Matcher::AllOf(vec![
                Matcher::Regex("client_id=app-1".into()),
                Matcher::Regex("scope=https%3A%2F%2Fstorage.azure.com%2F.default".into()),
                Matcher::Regex("client_assertion=projected-federated-token".into()),
                Matcher::Regex("grant_type=client_credentials".into()),
                Matcher::Regex("client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer".into()),
            ]))
            .with_header("content-type", "application/json")
            .with_body(r#"{"access_token": "eyJ.entra.token", "expires_in": 3599, "token_type": "Bearer"}"#)
            .expect(1)
            .create_async()
            .await;
        let mut blob = mockito::Server::new_async().await;
        let get = blob
            .mock("GET", "/media/a/b")
            .match_header("authorization", "Bearer eyJ.entra.token")
            .match_header("x-ms-version", API_VERSION)
            .with_body("data")
            .expect(3)
            .create_async()
            .await;
        let store = store(
            &blob.url(),
            AzureAuth::WorkloadIdentity {
                tenant_id: "tenant-1",
                client_id: "app-1",
                token_file: &token_file,
                authority: Some(&entra.url()),
            },
            "",
        );
        for _ in 0..3 {
            assert_eq!(store.get("a/b", 100).await.unwrap(), b"data");
        }
        exchange.assert_async().await;
        get.assert_async().await;
        // Entra rejecting the identity is a refusal, not an outage.
        let mut reject = mockito::Server::new_async().await;
        let _no = reject
            .mock("POST", Matcher::Any)
            .with_status(400)
            .with_body(r#"{"error": "invalid_client", "error_description": "secret detail"}"#)
            .create_async()
            .await;
        let denied = AzureStore::new(
            &AzureSettings {
                account: "acct",
                container: "media",
                prefix: "",
                endpoint: Some(&blob.url()),
                auth: AzureAuth::WorkloadIdentity {
                    tenant_id: "tenant-1",
                    client_id: "app-1",
                    token_file: &token_file,
                    authority: Some(&reject.url()),
                },
                ca_pem: None,
                max_object_bytes: 1024,
            },
            &env,
        )
        .unwrap();
        assert_eq!(denied.get("a/b", 100).await, Err(BlobError::Denied));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_managed_identity_asks_the_metadata_service_and_reads_a_string_lifetime() {
        let mut imds = mockito::Server::new_async().await;
        let token = imds
            .mock("GET", "/metadata/identity/oauth2/token")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("api-version".into(), "2018-02-01".into()),
                Matcher::UrlEncoded("resource".into(), "https://storage.azure.com/".into()),
                Matcher::UrlEncoded("client_id".into(), "user-assigned-1".into()),
            ]))
            .match_header("metadata", "true")
            .with_header("content-type", "application/json")
            // The metadata service returns the lifetime as a string.
            .with_body(r#"{"access_token": "imds.token", "expires_in": "86399", "resource": "https://storage.azure.com/"}"#)
            .expect(1)
            .create_async()
            .await;
        let mut blob = mockito::Server::new_async().await;
        let get = blob
            .mock("GET", "/media/a/b")
            .match_header("authorization", "Bearer imds.token")
            .with_body("data")
            .expect(2)
            .create_async()
            .await;
        let store = store(
            &blob.url(),
            AzureAuth::ManagedIdentity {
                client_id: Some("user-assigned-1"),
                endpoint: Some(&imds.url()),
            },
            "",
        );
        assert_eq!(store.get("a/b", 100).await.unwrap(), b"data");
        assert_eq!(store.get("a/b", 100).await.unwrap(), b"data");
        token.assert_async().await;
        get.assert_async().await;
    }

    #[test]
    fn bad_secrets_and_settings_fail_at_start_without_echoing_a_value() {
        let make = |auth: AzureAuth<'_>, env: &dyn Fn(&str) -> Option<String>| {
            AzureStore::new(
                &AzureSettings {
                    account: "acct",
                    container: "media",
                    prefix: "",
                    endpoint: None,
                    auth,
                    ca_pem: None,
                    max_object_bytes: 1024,
                },
                env,
            )
            .err()
        };
        let (key, sas) = (secret("K"), secret("S"));
        let error = make(AzureAuth::AccountKey(&key), &|_| {
            Some("not base64 hunter2!".into())
        })
        .unwrap();
        assert!(
            error.contains("account_key") && !error.contains("hunter2"),
            "{error}"
        );
        let error = make(AzureAuth::Sas(&sas), &|_| Some("?sv=hunter2".into())).unwrap();
        assert!(
            error.contains("sas_token") && !error.contains("hunter2"),
            "{error}"
        );
        assert!(make(AzureAuth::Sas(&sas), &|_| None).is_some());
        let missing = std::path::Path::new("/nonexistent/token");
        let error = make(
            AzureAuth::WorkloadIdentity {
                tenant_id: "t",
                client_id: "c",
                token_file: missing,
                authority: None,
            },
            &|_| None,
        )
        .unwrap();
        assert!(error.contains("federated token file"), "{error}");
        let error = make(
            AzureAuth::WorkloadIdentity {
                tenant_id: "t",
                client_id: "c",
                token_file: missing,
                authority: Some("http://login.example"),
            },
            &|_| None,
        )
        .unwrap();
        assert!(error.contains("authority_host"), "{error}");
        // A managed identity needs nothing at start (nothing is contacted).
        assert!(make(
            AzureAuth::ManagedIdentity {
                client_id: None,
                endpoint: None
            },
            &|_| None
        )
        .is_none());
    }

    #[test]
    fn a_token_answer_is_read_with_either_lifetime_form_and_checked() {
        assert!(parse_token(br#"{"access_token": "a.b", "expires_in": 60}"#).is_ok());
        assert_eq!(
            parse_token(br#"{"access_token": "a.b", "expires_in": "90"}"#)
                .unwrap()
                .1,
            Duration::from_secs(90)
        );
        // A lifetime of years is clamped to a day.
        assert_eq!(
            parse_token(br#"{"access_token": "a.b", "expires_in": 99999999999}"#)
                .unwrap()
                .1,
            Duration::from_secs(24 * 3600)
        );
        for bad in [
            &br#"{"access_token": "has space", "expires_in": 60}"#[..],
            br#"{"access_token": "", "expires_in": 60}"#,
            br#"{"access_token": "ok", "expires_in": 0}"#,
            br#"{"access_token": "ok", "expires_in": "soon"}"#,
            br#"{"expires_in": 60}"#,
            b"not json",
        ] {
            assert_eq!(parse_token(bad), Err(BlobError::Unavailable), "{bad:?}");
        }
    }

    #[test]
    fn percent_decoding_handles_escapes_plus_and_a_truncated_escape() {
        assert_eq!(decode("abc%2Bdef%3D"), "abc+def=");
        assert_eq!(decode("a+b"), "a b");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%4"), "%4");
        assert_eq!(decode("%zz"), "%zz");
    }
}

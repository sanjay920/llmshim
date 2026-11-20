//! Any S3-compatible object store (note 0050): AWS S3, MinIO, Ceph RGW, Cloudflare R2, DigitalOcean
//! Spaces, Backblaze B2, Cloud Storage's S3 interoperability API. Four plain HTTP operations signed
//! with SigV4 (`sigv4.rs`); no SDK. Credentials are secret references re-read at most once a
//! minute, so a rotated file or a projected secret takes effect without a restart.

use super::sigv4::{self, Credentials, Request};
use super::{valid_name, BlobError, BlobStore, Listed};
use crate::serve::config::{read_secret, SecretRef};
use async_trait::async_trait;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

const REREAD: Duration = Duration::from_secs(60);
const ATTEMPTS: usize = 3;

/// Where one credential comes from. An environment variable cannot change in a running process, so
/// it is read once; a file is re-read.
pub(crate) enum Source {
    Fixed(String),
    File(SecretRef),
}

impl Source {
    pub(crate) fn new(
        reference: &SecretRef,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        if reference.env.is_some() {
            read_secret(reference, env).map(Self::Fixed)
        } else {
            // Checked now, so a wrong path fails at start and not at the first request.
            read_secret(reference, &|_| None)?;
            Ok(Self::File(reference.clone()))
        }
    }

    pub(crate) fn read(&self) -> Result<String, BlobError> {
        match self {
            Self::Fixed(value) => Ok(value.clone()),
            Self::File(reference) => {
                read_secret(reference, &|_| None).map_err(|_| BlobError::Unavailable)
            }
        }
    }
}

/// Where the signing credentials come from.
enum Auth {
    Keys(Box<KeyAuth>),
    #[cfg(feature = "bedrock")]
    Chain(Box<super::aws_chain::AwsChain>),
}

/// Static keys by reference, re-read once a minute.
struct KeyAuth {
    key_id: Source,
    secret: Source,
    token: Option<Source>,
    loaded: Mutex<Option<Loaded>>,
}

struct Loaded {
    id: String,
    secret: String,
    token: Option<String>,
    at: Instant,
}

/// What `[media_store]` of kind `s3` says, with the secrets still references.
pub struct S3Settings<'a> {
    pub bucket: &'a str,
    pub region: &'a str,
    pub endpoint: Option<&'a str>,
    pub path_style: Option<bool>,
    pub prefix: &'a str,
    /// Static keys, by reference. `None` for all three when `aws_chain` is set.
    pub access_key_id: Option<&'a SecretRef>,
    pub secret_access_key: Option<&'a SecretRef>,
    pub session_token: Option<&'a SecretRef>,
    /// Use the AWS credential chain (IRSA, Pod Identity, ECS/EC2 roles, ...): `serve-cloud` only.
    pub aws_chain: bool,
    pub ca_pem: Option<&'a [u8]>,
    pub max_object_bytes: usize,
}

pub struct S3Store {
    client: reqwest::Client,
    bucket: String,
    region: String,
    /// `https://host[:port]` of the endpoint, or of AWS for the region.
    scheme: String,
    host: String,
    path_style: bool,
    prefix: String,
    auth: Auth,
    max_object_bytes: usize,
}

impl S3Store {
    pub fn new(
        settings: &S3Settings<'_>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        Self::from_parts(settings, Self::auth(settings, env)?)
    }

    fn from_parts(settings: &S3Settings<'_>, auth: Auth) -> Result<Self, String> {
        let (scheme, host, has_endpoint) = match settings.endpoint {
            Some(endpoint) => {
                let url = reqwest::Url::parse(endpoint)
                    .map_err(|_| "endpoint is not a valid URL".to_string())?;
                let host = url
                    .host_str()
                    .ok_or_else(|| "endpoint names no host".to_string())?;
                let authority = match url.port() {
                    Some(port) => format!("{host}:{port}"),
                    None => host.to_owned(),
                };
                (url.scheme().to_owned(), authority, true)
            }
            None => (
                "https".to_owned(),
                format!("s3.{}.amazonaws.com", settings.region),
                false,
            ),
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
        let store = Self {
            client: builder
                .build()
                .map_err(|_| "cannot build the HTTP client".to_string())?,
            bucket: settings.bucket.to_owned(),
            region: settings.region.to_owned(),
            scheme,
            host,
            path_style: settings.path_style.unwrap_or(has_endpoint),
            prefix: settings.prefix.to_owned(),
            auth,
            max_object_bytes: settings.max_object_bytes,
        };
        Ok(store)
    }

    fn auth(
        settings: &S3Settings<'_>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Auth, String> {
        if settings.aws_chain {
            if settings.access_key_id.is_some()
                || settings.secret_access_key.is_some()
                || settings.session_token.is_some()
            {
                return Err("keys and the AWS credential chain are exclusive".into());
            }
            #[cfg(feature = "bedrock")]
            return Ok(Auth::Chain(Box::new(super::aws_chain::AwsChain::new(
                settings.region,
            ))));
            #[cfg(not(feature = "bedrock"))]
            return Err(
                "the AWS credential chain needs a binary built with --features serve-cloud".into(),
            );
        }
        let (Some(id), Some(secret)) = (settings.access_key_id, settings.secret_access_key) else {
            return Err("access_key_id and secret_access_key are required".into());
        };
        Ok(Auth::Keys(Box::new(KeyAuth {
            key_id: Source::new(id, env).map_err(|m| format!("access_key_id: {m}"))?,
            secret: Source::new(secret, env).map_err(|m| format!("secret_access_key: {m}"))?,
            token: settings
                .session_token
                .map(|reference| {
                    Source::new(reference, env).map_err(|m| format!("session_token: {m}"))
                })
                .transpose()?,
            loaded: Mutex::new(None),
        })))
    }

    async fn credentials(&self) -> Result<(String, String, Option<String>), BlobError> {
        match &self.auth {
            Auth::Keys(keys) => {
                let mut loaded = keys.loaded.lock().unwrap_or_else(|e| e.into_inner());
                if loaded.as_ref().is_none_or(|l| l.at.elapsed() > REREAD) {
                    *loaded = Some(Loaded {
                        id: keys.key_id.read()?,
                        secret: keys.secret.read()?,
                        token: keys.token.as_ref().map(Source::read).transpose()?,
                        at: Instant::now(),
                    });
                }
                let l = loaded.as_ref().expect("just set");
                Ok((l.id.clone(), l.secret.clone(), l.token.clone()))
            }
            #[cfg(feature = "bedrock")]
            Auth::Chain(chain) => chain.get().await,
        }
    }

    /// Forget the credentials a refused request was signed with (key id `refused`): a refusal may mean
    /// they rotated.
    async fn invalidate(&self, refused: &str) {
        match &self.auth {
            Auth::Keys(keys) => {
                let mut loaded = keys.loaded.lock().unwrap_or_else(|e| e.into_inner());
                if loaded.as_ref().is_some_and(|l| l.id == refused) {
                    *loaded = None;
                }
            }
            #[cfg(feature = "bedrock")]
            Auth::Chain(chain) => chain.invalidate(refused).await,
        }
    }

    /// `(url, host header, canonical path)` of an object, or of the bucket when `name` is empty.
    fn target(&self, name: &str) -> (String, String, String) {
        let key = if name.is_empty() {
            String::new()
        } else {
            format!("{}{}", self.prefix, name)
        };
        let (host, path) = if self.path_style {
            (
                self.host.clone(),
                if key.is_empty() {
                    format!("/{}", self.bucket)
                } else {
                    format!("/{}/{}", self.bucket, key)
                },
            )
        } else {
            (format!("{}.{}", self.bucket, self.host), format!("/{key}"))
        };
        let path = sigv4::encode(&path, true);
        (format!("{}://{}{}", self.scheme, host, path), host, path)
    }

    async fn call_once(
        &self,
        method: &str,
        name: &str,
        query: &[(String, String)],
        body: Vec<u8>,
        limit: usize,
    ) -> Result<(u16, Vec<u8>), BlobError> {
        let (id, secret, token) = self.credentials().await?;
        let mut last = BlobError::Unavailable;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100 << attempt)).await;
            }
            let (url, host, path) = self.target(name);
            let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let payload = sigv4::sha256_hex(&body);
            let mut headers = vec![
                ("host".to_owned(), host),
                ("x-amz-content-sha256".to_owned(), payload.clone()),
                ("x-amz-date".to_owned(), amz_date.clone()),
            ];
            if let Some(token) = &token {
                headers.push(("x-amz-security-token".to_owned(), token.clone()));
            }
            let credentials = Credentials {
                access_key_id: &id,
                secret_access_key: &secret,
            };
            let (signed, signature) = sigv4::sign(
                &credentials,
                &self.region,
                &amz_date,
                &Request {
                    method,
                    path: &path,
                    query,
                    headers: &headers,
                    payload_sha256: &payload,
                },
            );
            // The query is built here, with the signer's own encoding: the client's would spell a
            // space or a slash differently and the signature would not match.
            let mut sorted: Vec<(String, String)> = query
                .iter()
                .map(|(k, v)| (sigv4::encode(k, false), sigv4::encode(v, false)))
                .collect();
            sorted.sort();
            let url = if sorted.is_empty() {
                url
            } else {
                let pairs: Vec<String> = sorted.iter().map(|(k, v)| format!("{k}={v}")).collect();
                format!("{url}?{}", pairs.join("&"))
            };
            let mut request = self
                .client
                .request(method.parse().map_err(|_| BlobError::Invalid)?, &url)
                .header("x-amz-date", &amz_date)
                .header("x-amz-content-sha256", &payload)
                .header(
                    reqwest::header::AUTHORIZATION,
                    sigv4::authorization(
                        &credentials,
                        &self.region,
                        &amz_date,
                        &signed,
                        &signature,
                    ),
                );
            if let Some(token) = &token {
                let mut value =
                    reqwest::header::HeaderValue::from_str(token).map_err(|_| BlobError::Denied)?;
                value.set_sensitive(true);
                request = request.header("x-amz-security-token", value);
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
            if response.content_length().is_some_and(|n| n > limit as u64) {
                return Err(BlobError::TooLarge);
            }
            match read_body(response, limit).await {
                Ok(bytes) => return Ok((status, bytes)),
                Err(BlobError::TooLarge) => return Err(BlobError::TooLarge),
                // The connection broke mid-body: the operations are idempotent, so try again.
                Err(error) => last = error,
            }
        }
        Err(last)
    }
}

/// The body, or [`BlobError::TooLarge`] past `limit`, or `Unavailable` if the connection broke.
pub(super) async fn read_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, BlobError> {
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len() + chunk.len() > limit {
                    return Err(BlobError::TooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(bytes),
            Err(_) => return Err(BlobError::Unavailable),
        }
    }
}

impl S3Store {
    /// One operation. A refusal (401 or 403) may be a credential that rotated between two files or
    /// since the cache was filled, so the cache is dropped and the operation tried once more.
    async fn call(
        &self,
        method: &str,
        name: &str,
        query: &[(String, String)],
        body: Vec<u8>,
        limit: usize,
    ) -> Result<(u16, Vec<u8>), BlobError> {
        // The key id this attempt is signed with (cached, so no second fetch).
        let used = self.credentials().await?.0;
        let first = self
            .call_once(method, name, query, body.clone(), limit)
            .await?;
        if !matches!(first.0, 401 | 403) {
            return Ok(first);
        }
        self.invalidate(&used).await;
        self.call_once(method, name, query, body, limit).await
    }
}

pub(super) fn status_error(status: u16) -> BlobError {
    match status {
        401 | 403 => BlobError::Denied,
        404 => BlobError::NotFound,
        400 | 405 | 409 | 411 | 412 | 413 | 414 | 422 => BlobError::Invalid,
        _ => BlobError::Unavailable,
    }
}

#[async_trait]
impl BlobStore for S3Store {
    async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<(), BlobError> {
        if !valid_name(name) {
            return Err(BlobError::Invalid);
        }
        match self.call("PUT", name, &[], bytes, 64 * 1024).await? {
            (200 | 201 | 204, _) => Ok(()),
            (status, _) => Err(status_error(status)),
        }
    }

    async fn get(&self, name: &str, max: usize) -> Result<Vec<u8>, BlobError> {
        if !valid_name(name) {
            return Err(BlobError::Invalid);
        }
        match self.call("GET", name, &[], Vec::new(), max).await? {
            (200, bytes) => Ok(bytes),
            (status, _) => Err(status_error(status)),
        }
    }

    async fn delete(&self, name: &str) -> Result<(), BlobError> {
        if !valid_name(name) {
            return Err(BlobError::Invalid);
        }
        match self
            .call("DELETE", name, &[], Vec::new(), 64 * 1024)
            .await?
        {
            (200 | 202 | 204 | 404, _) => Ok(()),
            (status, _) => Err(status_error(status)),
        }
    }

    async fn list(&self) -> Result<Vec<Listed>, BlobError> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        // A hard bound on pages (ten million objects): a bucket that never stops listing is not swept
        // forever.
        for _ in 0..10_000 {
            let mut query = vec![
                ("list-type".to_owned(), "2".to_owned()),
                ("prefix".to_owned(), self.prefix.clone()),
            ];
            if let Some(token) = &token {
                query.push(("continuation-token".to_owned(), token.clone()));
            }
            let (status, body) = self.call("GET", "", &query, Vec::new(), 8 << 20).await?;
            if status != 200 {
                return Err(status_error(status));
            }
            let page = parse_listing(&String::from_utf8_lossy(&body), &self.prefix);
            out.extend(page.objects);
            match (page.truncated, page.next) {
                (_, Some(next)) => token = Some(next),
                (false, None) => return Ok(out),
                // Cut, with no way to continue: a partial list is not passed off as the whole.
                (true, None) => return Err(BlobError::Unavailable),
            }
        }
        Err(BlobError::Unavailable)
    }

    fn max_object_bytes(&self) -> usize {
        self.max_object_bytes
    }
}

struct Page {
    objects: Vec<Listed>,
    truncated: bool,
    next: Option<String>,
}

/// The text between `<tag>` and `</tag>` at or after `from`, and where the closing tag ends.
pub(super) fn between<'a>(xml: &'a str, tag: &str, from: usize) -> Option<(&'a str, usize)> {
    let open = format!("<{tag}>");
    let start = xml[from..].find(&open)? + from + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some((&xml[start..end], end + tag.len() + 3))
}

pub(super) fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// A ListObjectsV2 answer: each `<Contents>`'s key (with the store's prefix removed, so it is the name
/// the other operations take) and `LastModified`, and the continuation token if the page was cut.
fn parse_listing(xml: &str, prefix: &str) -> Page {
    let mut objects = Vec::new();
    let mut at = 0;
    while let Some((block, end)) = between(xml, "Contents", at) {
        at = end;
        let key = between(block, "Key", 0).map(|(k, _)| unescape(k));
        let modified = between(block, "LastModified", 0)
            .and_then(|(t, _)| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| SystemTime::from(t.with_timezone(&chrono::Utc)));
        if let (Some(key), Some(modified)) = (key, modified) {
            if let Some(name) = key.strip_prefix(prefix) {
                objects.push(Listed {
                    key: name.to_owned(),
                    modified,
                });
            }
        }
    }
    let truncated = between(xml, "IsTruncated", 0).is_some_and(|(v, _)| v == "true");
    let next = truncated
        .then(|| between(xml, "NextContinuationToken", 0).map(|(t, _)| unescape(t)))
        .flatten();
    Page {
        objects,
        truncated,
        next,
    }
}

#[cfg(test)]
pub(super) fn listing_for_tests(xml: &str, prefix: &str) -> (Vec<Listed>, Option<String>) {
    let page = parse_listing(xml, prefix);
    (page.objects, page.next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Matcher;

    fn secret(env: &str) -> SecretRef {
        SecretRef {
            env: Some(env.to_owned()),
            file: None,
        }
    }

    fn store(endpoint: &str, prefix: &str) -> S3Store {
        let (id, key) = (secret("ID"), secret("KEY"));
        let token = secret("TOKEN");
        S3Store::new(
            &S3Settings {
                bucket: "media",
                region: "us-east-1",
                endpoint: Some(endpoint),
                path_style: None,
                prefix,
                access_key_id: Some(&id),
                secret_access_key: Some(&key),
                session_token: Some(&token),
                aws_chain: false,
                ca_pem: None,
                max_object_bytes: 1 << 20,
            },
            &|name| match name {
                "ID" => Some("AKIDEXAMPLE".to_owned()),
                "KEY" => Some("secret-key-0123".to_owned()),
                "TOKEN" => Some("session-token-abc".to_owned()),
                _ => None,
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn put_get_and_delete_are_signed_and_use_the_path_style_address() {
        let mut server = mockito::Server::new_async().await;
        let signed = Matcher::Regex(
            r"^AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/\d{8}/us-east-1/s3/aws4_request,SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token,Signature=[0-9a-f]{64}$".into(),
        );
        let put = server
            .mock("PUT", "/media/llmshim/20260101/ab")
            .match_header("authorization", signed.clone())
            .match_header("x-amz-security-token", "session-token-abc")
            .match_header(
                "x-amz-content-sha256",
                Matcher::Regex("^[0-9a-f]{64}$".into()),
            )
            .match_body("ciphertext")
            .with_status(200)
            .create_async()
            .await;
        let get = server
            .mock("GET", "/media/llmshim/20260101/ab")
            .match_header("authorization", signed.clone())
            .with_status(200)
            .with_body("ciphertext")
            .create_async()
            .await;
        let delete = server
            .mock("DELETE", "/media/llmshim/20260101/ab")
            .match_header("authorization", signed)
            .with_status(204)
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
    async fn each_failure_class_is_named_and_a_server_error_is_retried() {
        let mut server = mockito::Server::new_async().await;
        let store = store(&server.url(), "");
        for (status, expected) in [
            (404, BlobError::NotFound),
            (403, BlobError::Denied),
            (401, BlobError::Denied),
            (400, BlobError::Invalid),
        ] {
            let mock = server
                .mock("GET", "/media/a/b")
                .with_status(status)
                .with_body("<Error><Message>secret details</Message></Error>")
                // A refusal is tried once more with freshly read credentials.
                .expect(if matches!(status, 401 | 403) { 2 } else { 1 })
                .create_async()
                .await;
            assert_eq!(store.get("a/b", 100).await, Err(expected), "{status}");
            mock.assert_async().await;
            mock.remove_async().await;
        }
        // A 503 is retried (three attempts), then reported as unavailable.
        let down = server
            .mock("GET", "/media/a/b")
            .with_status(503)
            .expect(3)
            .create_async()
            .await;
        assert_eq!(store.get("a/b", 100).await, Err(BlobError::Unavailable));
        down.assert_async().await;
        down.remove_async().await;
        // A 503 then a 200 is a success.
        let flaky = server
            .mock("GET", "/media/a/b")
            .with_status(503)
            .expect(1)
            .create_async()
            .await;
        let fine = server
            .mock("GET", "/media/a/b")
            .with_status(200)
            .with_body("ok")
            .create_async()
            .await;
        assert_eq!(store.get("a/b", 100).await.unwrap(), b"ok");
        flaky.assert_async().await;
        fine.assert_async().await;
        // A larger body than the cap, by header or by stream, is refused.
        fine.remove_async().await;
        let _large = server
            .mock("GET", "/media/a/c")
            .with_status(200)
            .with_body("x".repeat(500))
            .create_async()
            .await;
        assert_eq!(store.get("a/c", 100).await, Err(BlobError::TooLarge));
        // Nothing listens: unavailable, after retries.
        let dead = S3Store::new(
            &S3Settings {
                bucket: "media",
                region: "us-east-1",
                endpoint: Some("http://127.0.0.1:9"),
                path_style: None,
                prefix: "",
                access_key_id: Some(&secret("ID")),
                secret_access_key: Some(&secret("KEY")),
                session_token: None,
                aws_chain: false,
                ca_pem: None,
                max_object_bytes: 1024,
            },
            &|_| Some("v".to_owned()),
        )
        .unwrap();
        assert_eq!(dead.get("a/b", 10).await, Err(BlobError::Unavailable));
    }

    #[tokio::test]
    async fn a_listing_is_paged_and_the_prefix_is_removed() {
        let mut server = mockito::Server::new_async().await;
        let first = server
            .mock("GET", "/media")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("list-type".into(), "2".into()),
                Matcher::UrlEncoded("prefix".into(), "llmshim/".into()),
            ]))
            .with_body(
                r#"<ListBucketResult><IsTruncated>true</IsTruncated>
                <Contents><Key>llmshim/20260101/a</Key><LastModified>2026-01-01T00:00:00.000Z</LastModified></Contents>
                <Contents><Key>other/x</Key><LastModified>2026-01-01T00:00:00.000Z</LastModified></Contents>
                <NextContinuationToken>tok/+=</NextContinuationToken></ListBucketResult>"#,
            )
            .expect(1)
            .create_async()
            .await;
        let second = server
            .mock("GET", "/media")
            .match_query(Matcher::UrlEncoded("continuation-token".into(), "tok/+=".into()))
            .with_body(
                r#"<ListBucketResult><IsTruncated>false</IsTruncated>
                <Contents><Key>llmshim/20260102/b</Key><LastModified>2026-01-02T03:04:05Z</LastModified></Contents>
                </ListBucketResult>"#,
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
    async fn a_refusal_drops_the_cached_credentials_and_tries_once_more() {
        // A key file that rotated after the cache was filled: the first signature is refused, the
        // second, from the re-read file, is accepted.
        let path = std::env::temp_dir().join(format!("llmshim-s3-rot-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, "old-secret\n").unwrap();
        let mut server = mockito::Server::new_async().await;
        let (id, key) = (
            secret("ID"),
            SecretRef {
                env: None,
                file: Some(path.clone()),
            },
        );
        let store = S3Store::new(
            &S3Settings {
                bucket: "media",
                region: "us-east-1",
                endpoint: Some(&server.url()),
                path_style: None,
                prefix: "",
                access_key_id: Some(&id),
                secret_access_key: Some(&key),
                session_token: None,
                aws_chain: false,
                ca_pem: None,
                max_object_bytes: 1024,
            },
            &|_| Some("AKID".to_owned()),
        )
        .unwrap();
        // Fill the cache with the old secret, then rotate the file.
        store.credentials().await.unwrap();
        std::fs::write(&path, "new-secret\n").unwrap();
        let refused = server
            .mock("GET", "/media/a/b")
            .with_status(403)
            .expect(1)
            .create_async()
            .await;
        let accepted = server
            .mock("GET", "/media/a/b")
            .with_status(200)
            .with_body("ok")
            .create_async()
            .await;
        assert_eq!(store.get("a/b", 100).await.unwrap(), b"ok");
        refused.assert_async().await;
        accepted.assert_async().await;
        assert_eq!(store.credentials().await.unwrap().1, "new-secret");
        // A refusal that persists is reported once, after one retry, as a class.
        let mut second = mockito::Server::new_async().await;
        let always = second
            .mock("GET", "/media/a/b")
            .with_status(403)
            .expect(2)
            .create_async()
            .await;
        let denied = S3Store::new(
            &S3Settings {
                bucket: "media",
                region: "us-east-1",
                endpoint: Some(&second.url()),
                path_style: None,
                prefix: "",
                access_key_id: Some(&id),
                secret_access_key: Some(&secret("KEY")),
                session_token: None,
                aws_chain: false,
                ca_pem: None,
                max_object_bytes: 1024,
            },
            &|_| Some("v".to_owned()),
        )
        .unwrap();
        assert_eq!(denied.get("a/b", 10).await, Err(BlobError::Denied));
        always.assert_async().await;
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn a_listing_that_is_cut_with_no_way_to_continue_is_an_error_not_a_short_list() {
        let mut server = mockito::Server::new_async().await;
        let _cut = server
            .mock("GET", "/media")
            .match_query(Matcher::Any)
            .with_body(
                "<ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>20260101/a</Key>\
                 <LastModified>2026-01-01T00:00:00Z</LastModified></Contents></ListBucketResult>",
            )
            .create_async()
            .await;
        assert_eq!(
            store(&server.url(), "").list().await,
            Err(BlobError::Unavailable)
        );
    }

    #[test]
    fn the_listing_parser_reads_keys_dates_and_unescapes() {
        let xml = "<R><Contents><Key>a&amp;b/c</Key><LastModified>2013-05-24T00:00:00.000Z</LastModified></Contents>\
                   <Contents><Key>no-date</Key></Contents></R>";
        let (objects, next) = listing_for_tests(xml, "");
        assert_eq!(
            objects.len(),
            1,
            "an entry without a date is not an object to age"
        );
        assert_eq!(objects[0].key, "a&b/c");
        assert_eq!(next, None);
    }

    #[test]
    fn the_address_follows_the_style_and_the_endpoint() {
        let path = store("https://minio.example:9000", "p/");
        assert_eq!(
            path.target("d/k"),
            (
                "https://minio.example:9000/media/p/d/k".to_owned(),
                "minio.example:9000".to_owned(),
                "/media/p/d/k".to_owned()
            )
        );
        let (id, key) = (secret("ID"), secret("KEY"));
        let aws = S3Store::new(
            &S3Settings {
                bucket: "media",
                region: "eu-west-1",
                endpoint: None,
                path_style: None,
                prefix: "",
                access_key_id: Some(&id),
                secret_access_key: Some(&key),
                session_token: None,
                aws_chain: false,
                ca_pem: None,
                max_object_bytes: 1024,
            },
            &|_| Some("v".to_owned()),
        )
        .unwrap();
        // AWS with no endpoint: the bucket is in the host.
        assert_eq!(
            aws.target("d/k").0,
            "https://media.s3.eu-west-1.amazonaws.com/d/k"
        );
        assert_eq!(aws.target("d/k").1, "media.s3.eu-west-1.amazonaws.com");
    }

    #[tokio::test]
    async fn credentials_that_cannot_be_read_fail_at_start_and_a_file_is_reread() {
        let missing = SecretRef {
            env: None,
            file: Some("/nonexistent/secret".into()),
        };
        let err = S3Store::new(
            &S3Settings {
                bucket: "media",
                region: "us-east-1",
                endpoint: Some("http://127.0.0.1:9"),
                path_style: None,
                prefix: "",
                access_key_id: Some(&missing),
                secret_access_key: Some(&secret("KEY")),
                session_token: None,
                aws_chain: false,
                ca_pem: None,
                max_object_bytes: 1024,
            },
            &|_| Some("v".to_owned()),
        )
        .err()
        .unwrap();
        assert!(
            err.starts_with("access_key_id:") && !err.contains("v"),
            "{err}"
        );
        // A file is read each time the cache expires: changing it changes the next signature.
        let path = std::env::temp_dir().join(format!("llmshim-s3-cred-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, "first-secret\n").unwrap();
        let source = Source::new(
            &SecretRef {
                env: None,
                file: Some(path.clone()),
            },
            &|_| None,
        )
        .unwrap();
        assert_eq!(source.read().unwrap(), "first-secret");
        std::fs::write(&path, "second-secret\n").unwrap();
        assert_eq!(source.read().unwrap(), "second-secret");
        let _ = std::fs::remove_file(path);
    }
}

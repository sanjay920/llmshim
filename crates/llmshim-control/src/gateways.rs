//! The gateways the control plane shows (design note 0051, slice 4): a file of their admin listeners,
//! read at start, and one answer that asks each for its status.
//!
//! The control plane only **reads**: `GET /v1/gateway/stats`, `GET /v1/serve/config` and `GET /ready` on each
//! admin listener, with the listener's own bearer token. The addresses are the operator's file, never a
//! caller's input; redirects are not followed, ambient proxies are not used, a body is capped and every
//! failure is a short code, never the token or the gateway's own error body. A **successful** answer is the
//! gateway's own status JSON, forwarded as it is (it is built from names, numbers and fixed text, never a
//! secret value: see `/v1/serve/config`), and must be a JSON object. One answer is shared for a few seconds,
//! so any number of viewers is one request per gateway per interval.

use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The most gateways one file may list.
pub const MAX_GATEWAYS: usize = 64;
/// How long one answer is reused.
const CACHE_FOR: Duration = Duration::from_secs(5);
/// Each request to a gateway.
const TIMEOUT: Duration = Duration::from_secs(4);
/// The largest status body read: stats and config are small; anything more is not what was asked for.
const MAX_BODY: usize = 1 << 20;

/// The file given to `--gateways-file`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewaysFile {
    pub gateways: Vec<GatewayEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayEntry {
    /// Shown in the UI: 1 to 64 of letters, digits, `.`, `_` and `-`.
    pub name: String,
    /// The admin listener's origin: `https://host[:port]`, or `http://` with `allow_insecure_http`.
    pub admin_url: String,
    /// The listener's bearer token (`[admin] token`), read at each fetch so a rotation needs no restart.
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// PEM certificates to trust for this listener instead of the web PKI roots.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    /// Plain http to a listener on a private network.
    #[serde(default)]
    pub allow_insecure_http: bool,
}

struct Target {
    name: String,
    origin: String,
    token_file: Option<PathBuf>,
    client: reqwest::Client,
}

pub struct Gateways {
    targets: Vec<Target>,
    cache_for: Duration,
    cache: tokio::sync::Mutex<Option<(Instant, Value)>>,
}

/// An owner-only token file's content (checked at each read), or why it cannot be used.
pub fn read_token_file(path: &Path) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("cannot read {}: {}", path.display(), e.kind()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "{} is readable by group or others; make it owner-only (chmod 600)",
                path.display()
            ));
        }
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e.kind()))?;
    let token = text.trim().to_string();
    match crate::api::token_problem(&token) {
        None => Ok(token),
        Some(problem) => Err(format!("the token in {} {problem}", path.display())),
    }
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The origin of an admin URL, or why it is not one. Never echoes the value.
fn origin(entry: &GatewayEntry) -> Result<String, String> {
    let url =
        reqwest::Url::parse(&entry.admin_url).map_err(|_| "admin_url: not a URL".to_string())?;
    let secure = url.scheme() == "https";
    if !secure && !(url.scheme() == "http" && entry.allow_insecure_http) {
        return Err("admin_url: must be https (http needs allow_insecure_http)".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("admin_url: must not carry credentials".into());
    }
    if url.query().is_some() || url.fragment().is_some() || !matches!(url.path(), "" | "/") {
        return Err("admin_url: must be an origin only (no path, query or fragment)".into());
    }
    let host = url.host_str().ok_or("admin_url: needs a host")?;
    Ok(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

impl Gateways {
    /// From the file's bytes. Every problem names the entry's position and the field, never a value.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        Self::with_cache(bytes, CACHE_FOR)
    }

    /// As [`Gateways::from_bytes`] with an answer reused for `cache_for` (a test uses none).
    pub fn with_cache(bytes: &[u8], cache_for: Duration) -> Result<Self, String> {
        if bytes.len() > 256 * 1024 {
            return Err("the gateways file is larger than 256 KiB".into());
        }
        let file: GatewaysFile = serde_json::from_slice(bytes).map_err(|_| {
            "not a gateways file: a field is missing, unknown or of the wrong type".to_string()
        })?;
        if file.gateways.len() > MAX_GATEWAYS {
            return Err(format!("at most {MAX_GATEWAYS} gateways"));
        }
        let mut names = std::collections::BTreeSet::new();
        let mut targets = Vec::new();
        for (index, entry) in file.gateways.iter().enumerate() {
            let at = |message: String| format!("gateways[{index}].{message}");
            if !valid_name(&entry.name) {
                return Err(at(
                    "name: 1 to 64 of letters, digits, '.', '_' and '-'".into()
                ));
            }
            if !names.insert(entry.name.clone()) {
                return Err(at("name: used twice".into()));
            }
            let origin = origin(entry).map_err(&at)?;
            for (field, path) in [
                ("token_file", &entry.token_file),
                ("ca_file", &entry.ca_file),
            ] {
                if path.as_ref().is_some_and(|p| !p.has_root()) {
                    return Err(at(format!("{field}: must be an absolute path")));
                }
            }
            if let Some(path) = &entry.token_file {
                read_token_file(path).map_err(|m| at(format!("token_file: {m}")))?;
            }
            let mut builder = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(Duration::from_secs(3))
                .timeout(TIMEOUT);
            if let Some(path) = &entry.ca_file {
                let pem = std::fs::read(path).map_err(|e| {
                    at(format!(
                        "ca_file: cannot read {}: {}",
                        path.display(),
                        e.kind()
                    ))
                })?;
                let certificates = reqwest::Certificate::from_pem_bundle(&pem)
                    .map_err(|_| at("ca_file: no certificate could be read".into()))?;
                if certificates.is_empty() {
                    return Err(at("ca_file: no certificate could be read".into()));
                }
                builder = builder.tls_built_in_root_certs(false);
                for certificate in certificates {
                    builder = builder.add_root_certificate(certificate);
                }
            }
            targets.push(Target {
                name: entry.name.clone(),
                origin,
                token_file: entry.token_file.clone(),
                client: builder
                    .build()
                    .map_err(|_| at("could not build a client".into()))?,
            });
        }
        Ok(Self {
            targets,
            cache_for,
            cache: tokio::sync::Mutex::new(None),
        })
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Each gateway's status. One fetch at a time, reused for [`CACHE_FOR`].
    pub async fn status(&self) -> Value {
        let mut cache = self.cache.lock().await;
        if let Some((at, value)) = cache.as_ref() {
            if at.elapsed() < self.cache_for {
                return value.clone();
            }
        }
        let answers = futures_join(self.targets.iter().map(fetch)).await;
        let value = json!({
            "configured": true,
            "fetched_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "gateways": answers,
        });
        *cache = Some((Instant::now(), value.clone()));
        value
    }
}

/// Why a payload could not be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// No gateways are listed, so nothing can open a payload.
    NotConfigured,
    /// No gateway has the object (it was deleted, or was never written).
    NotFound,
    /// A gateway refused the reference: another deployment's, or altered.
    BadReference,
    /// No gateway could be asked or answered usefully.
    Unavailable,
}

/// The largest document a gateway may answer an open with: two parts of at most 1 MiB each, escaped.
const MAX_OPEN_BODY: usize = 16 << 20;

impl Gateways {
    /// Ask every gateway at once to open a payload reference; the first that does wins. The answer is the gateway's:
    /// `{"payload": {…}, "segments": […] or null}`. In a fleet any replica can
    /// (the store and the key are shared); with a local store only the one that wrote it can, and the others say
    /// not found. The reference is only handed on, never read, and nothing a gateway says is repeated but the
    /// document.
    pub async fn open_payload(&self, reference: &str) -> Result<Value, OpenError> {
        if self.targets.is_empty() {
            return Err(OpenError::NotConfigured);
        }
        let tasks: Vec<_> = self
            .targets
            .iter()
            .map(|target| {
                let (client, origin, token_file) = (
                    target.client.clone(),
                    target.origin.clone(),
                    target.token_file.clone(),
                );
                let reference = reference.to_owned();
                tokio::spawn(async move {
                    let token = match &token_file {
                        Some(path) => {
                            Some(read_token_file(path).map_err(|_| OpenError::Unavailable)?)
                        }
                        None => None,
                    };
                    let mut request = client
                        .post(format!("{origin}/v1/serve/payload/open"))
                        .json(&json!({"reference": reference}));
                    if let Some(token) = &token {
                        request = request.bearer_auth(token);
                    }
                    let mut response = request.send().await.map_err(|_| OpenError::Unavailable)?;
                    let status = response.status().as_u16();
                    let mut body = Vec::new();
                    while let Some(chunk) =
                        response.chunk().await.map_err(|_| OpenError::Unavailable)?
                    {
                        body.extend_from_slice(&chunk);
                        if body.len() > MAX_OPEN_BODY {
                            return Err(OpenError::Unavailable);
                        }
                    }
                    match status {
                        200 => serde_json::from_slice::<Value>(&body)
                            .ok()
                            .filter(|v| v.get("payload").is_some_and(Value::is_object))
                            .ok_or(OpenError::Unavailable),
                        404 => Err(OpenError::NotFound),
                        422 => Err(OpenError::BadReference),
                        _ => Err(OpenError::Unavailable),
                    }
                })
            })
            .collect();
        let mut worst = OpenError::NotFound;
        for task in tasks {
            match task.await.unwrap_or(Err(OpenError::Unavailable)) {
                Ok(document) => return Ok(document),
                Err(OpenError::Unavailable) => worst = OpenError::Unavailable,
                Err(OpenError::BadReference) if worst == OpenError::NotFound => {
                    worst = OpenError::BadReference
                }
                Err(_) => {}
            }
        }
        Err(worst)
    }
}

/// Run the futures together and collect their outputs in order (no extra dependency for a join).
async fn futures_join<F: std::future::Future<Output = Value> + Send + 'static>(
    futures: impl Iterator<Item = F>,
) -> Vec<Value> {
    let handles: Vec<_> = futures.map(tokio::spawn).collect();
    let mut out = Vec::new();
    for handle in handles {
        out.push(
            handle
                .await
                .unwrap_or_else(|_| json!({"error": "internal"})),
        );
    }
    out
}

enum Got {
    Body(u16, Vec<u8>),
    TooLarge,
    Failed,
}

async fn get(client: &reqwest::Client, url: &str, token: Option<&str>) -> Got {
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let Ok(mut response) = request.send().await else {
        return Got::Failed;
    };
    let status = response.status().as_u16();
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() > MAX_BODY {
                    return Got::TooLarge;
                }
            }
            Ok(None) => return Got::Body(status, body),
            Err(_) => return Got::Failed,
        }
    }
}

fn fetch(target: &Target) -> impl std::future::Future<Output = Value> + Send + 'static {
    let name = target.name.clone();
    let origin = target.origin.clone();
    let client = target.client.clone();
    let token_file = target.token_file.clone();
    async move {
        // Read at each fetch: a rotated token is picked up, and a bad file is a code, not a crash.
        let token = match &token_file {
            Some(path) => match read_token_file(path) {
                Ok(token) => Some(token),
                Err(_) => {
                    return json!({"name": name, "url": origin, "reachable": false, "error": "token_unreadable"});
                }
            },
            None => None,
        };
        let token = token.as_deref();
        let (stats_url, config_url, ready_url) = (
            format!("{origin}/v1/gateway/stats"),
            format!("{origin}/v1/serve/config"),
            format!("{origin}/ready"),
        );
        let (stats, config, ready) = tokio::join!(
            get(&client, &stats_url, token),
            get(&client, &config_url, token),
            get(&client, &ready_url, None),
        );
        let mut answer = json!({"name": name, "url": origin});
        let parse = |got: &Got| -> Result<Option<Value>, &'static str> {
            match got {
                Got::Failed => Err("unreachable"),
                Got::TooLarge => Err("bad_response"),
                Got::Body(401 | 403, _) => Err("unauthorized"),
                Got::Body(200, body) => match serde_json::from_slice::<Value>(body) {
                    Ok(value) if value.is_object() => Ok(Some(value)),
                    _ => Err("bad_response"),
                },
                Got::Body(404, _) => Ok(None),
                Got::Body(_, _) => Err("bad_response"),
            }
        };
        let reached = !matches!(stats, Got::Failed) || !matches!(ready, Got::Failed);
        answer["reachable"] = json!(reached);
        answer["ready"] = match &ready {
            Got::Body(200, _) => json!(true),
            Got::Body(503, _) => json!(false),
            _ => Value::Null,
        };
        match parse(&stats) {
            Ok(value) => answer["stats"] = value.unwrap_or(Value::Null),
            Err(code) => answer["error"] = json!(code),
        }
        // The reload status is secondary: a gateway without it still shows its stats.
        answer["config"] = parse(&config).ok().flatten().unwrap_or(Value::Null);
        answer
    }
}

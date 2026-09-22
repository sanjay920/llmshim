use crate::{Catalog, CatalogError, CATALOG_URL};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;

fn transport_error() -> CatalogError {
    // reqwest includes the request URL in its Display implementation. Catalog
    // endpoints may use query credentials, so keep transport diagnostics URL-free.
    CatalogError::Invalid("catalog transport failed")
}

async fn bounded_response_body(
    mut response: reqwest::Response,
    size_error: &'static str,
) -> Result<Vec<u8>, CatalogError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| transport_error())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_CATALOG_BYTES {
            return Err(CatalogError::Invalid(size_error));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Debug, Clone)]
pub struct CatalogOptions {
    pub cache_file: Option<PathBuf>,
    pub global_overrides: Option<PathBuf>,
    pub project_overrides: Option<PathBuf>,
    pub offline: bool,
    pub ttl: Duration,
    /// Change explicitly for a mirror or a local test server.
    pub url: String,
}

impl Default for CatalogOptions {
    fn default() -> Self {
        let home = dirs::home_dir();
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|p| p.join(".config")));
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|p| p.join(".cache")));
        Self {
            cache_file: cache.map(|p| p.join("llmshim/models.dev.json")),
            global_overrides: config.map(|p| p.join("llmshim/models.toml")),
            project_overrides: std::env::var_os("LLMSHIM_CATALOG_PROJECT")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|p| p.join(".llmshim/models.toml"))
                }),
            offline: std::env::var("LLMSHIM_CATALOG_OFFLINE").is_ok_and(|v| v == "1"),
            ttl: Duration::from_secs(24 * 60 * 60),
            url: CATALOG_URL.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    Offline,
    Fresh,
    Updated {
        models: usize,
    },
    NotModified,
    /// Last good snapshot remains available; the reason is diagnostic only.
    Stale {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheMetadata {
    etag: Option<String>,
    fetched_at: DateTime<Utc>,
    sha256: String,
}

fn metadata_path(path: &Path) -> PathBuf {
    path.with_extension("metadata.json")
}
fn hash(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn read_cache(options: &CatalogOptions) -> Option<(String, Option<CacheMetadata>)> {
    if options.offline {
        return None;
    }
    let path = options.cache_file.as_ref()?;
    if std::fs::metadata(path).ok()?.len() > MAX_CATALOG_BYTES as u64 {
        return None;
    }
    let data = std::fs::read_to_string(path).ok()?;
    let metadata: Option<CacheMetadata> = std::fs::read(metadata_path(path))
        .ok()
        .and_then(|b| serde_json::from_slice::<CacheMetadata>(&b).ok())
        .filter(|m| m.sha256 == hash(data.as_bytes()));
    Some((data, metadata))
}

fn apply_overrides(catalog: &mut Catalog, options: &CatalogOptions) -> Result<(), CatalogError> {
    for path in [&options.global_overrides, &options.project_overrides]
        .into_iter()
        .flatten()
    {
        match std::fs::read_to_string(path) {
            Ok(data) => catalog.merge_local_toml(&data)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

impl Catalog {
    /// Disk-only startup: cache corruption falls back to the vendored floor.
    /// Invalid local policy is an explicit error, never silently ignored.
    pub fn load(options: &CatalogOptions) -> Result<Self, CatalogError> {
        let mut catalog = Self::vendored();
        if let Some((data, meta)) = read_cache(options) {
            // Validate before merging; a failed parse cannot partially modify it.
            let _ = catalog.merge_models_dev(&data, meta.map(|m| m.fetched_at));
        }
        apply_overrides(&mut catalog, options)?;
        Ok(catalog)
    }
}

struct Inner {
    options: CatalogOptions,
    snapshot: RwLock<Arc<Catalog>>,
    refresh_lock: tokio::sync::Mutex<()>,
    provider_layers: RwLock<BTreeMap<String, (Value, DateTime<Utc>)>>,
    last_refresh: RwLock<Option<DateTime<Utc>>>,
}

/// A resident catalog. Requests borrow a snapshot immediately while detached
/// refreshes build a replacement; a network failure never removes the old view.
#[derive(Clone)]
pub struct CatalogHandle(Arc<Inner>);

impl CatalogHandle {
    pub fn load(options: CatalogOptions) -> Result<Self, CatalogError> {
        let catalog = Catalog::load(&options)?;
        Ok(Self(Arc::new(Inner {
            options,
            snapshot: RwLock::new(Arc::new(catalog)),
            refresh_lock: tokio::sync::Mutex::new(()),
            provider_layers: RwLock::new(BTreeMap::new()),
            last_refresh: RwLock::new(None),
        })))
    }

    pub fn snapshot(&self) -> Arc<Catalog> {
        self.0
            .snapshot
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Starts background refresh only when a Tokio runtime is already present.
    /// Does not create a runtime or make synchronous network requests.
    pub fn refresh_in_background(&self) -> Option<tokio::task::JoinHandle<RefreshOutcome>> {
        if self.0.options.offline {
            return None;
        }
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let handle = self.clone();
        Some(runtime.spawn(async move { handle.refresh(false).await }))
    }

    pub async fn refresh(&self, force: bool) -> RefreshOutcome {
        if self.0.options.offline {
            return RefreshOutcome::Offline;
        }
        let _guard = self.0.refresh_lock.lock().await;
        match self.try_refresh(force).await {
            Ok(outcome) => outcome,
            Err(e) => RefreshOutcome::Stale {
                reason: e.to_string(),
            },
        }
    }

    async fn try_refresh(&self, force: bool) -> Result<RefreshOutcome, CatalogError> {
        let options = &self.0.options;
        // Disk work is independent of request snapshots and never takes their lock.
        let opts = options.clone();
        let cache = tokio::task::spawn_blocking(move || read_cache(&opts))
            .await
            .map_err(|_| CatalogError::Invalid("catalog cache reader stopped"))?;
        // A valid ETag is tied to the exact validated body, not just a sidecar.
        let cache = cache.filter(|(data, _)| crate::parse::models_dev(data, None).is_ok());
        let meta = cache.as_ref().and_then(|(_, m)| m.as_ref());
        let last_refresh = *self
            .0
            .last_refresh
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let at = last_refresh.or_else(|| meta.map(|m| m.fetched_at));
        if !force
            && at.is_some_and(|at| {
                Utc::now()
                    .signed_duration_since(at)
                    .to_std()
                    .is_ok_and(|age| age < options.ttl)
            })
        {
            return Ok(RefreshOutcome::Fresh);
        }
        let client = http_client()?;
        let mut request = client.get(&options.url);
        if let Some(etag) = meta.and_then(|m| m.etag.as_deref()) {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await.map_err(|_| transport_error())?;
        let not_modified = response.status() == reqwest::StatusCode::NOT_MODIFIED;
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned)
            .or_else(|| {
                if not_modified {
                    meta.and_then(|m| m.etag.clone())
                } else {
                    None
                }
            });
        let data = if not_modified {
            if meta.and_then(|m| m.etag.as_ref()).is_none() {
                return Err(CatalogError::Invalid("304 without a validated ETag"));
            }
            cache
                .map(|(data, _)| data)
                .ok_or(CatalogError::Invalid("304 without a validated cache"))?
        } else {
            if !response.status().is_success() {
                return Err(CatalogError::Invalid("catalog server returned an error"));
            }
            let bytes = bounded_response_body(response, "catalog exceeds size limit").await?;
            String::from_utf8(bytes).map_err(|_| CatalogError::Invalid("catalog is not UTF-8"))?
        };
        let fetched_at = Utc::now();
        let mut catalog = Catalog::vendored();
        let models = catalog.merge_models_dev(&data, Some(fetched_at))?;
        for (provider, (value, at)) in self
            .0
            .provider_layers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
        {
            catalog.merge_provider_models(provider, value, *at)?;
        }
        apply_overrides(&mut catalog, options)?;
        if let Some(path) = options.cache_file.clone() {
            let metadata = CacheMetadata {
                etag,
                fetched_at,
                sha256: hash(data.as_bytes()),
            };
            tokio::task::spawn_blocking(move || save_cache(&path, &data, &metadata))
                .await
                .map_err(|_| CatalogError::Invalid("catalog cache writer stopped"))??;
        }
        *self.0.snapshot.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(catalog);
        *self
            .0
            .last_refresh
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Some(fetched_at);
        Ok(if not_modified {
            RefreshOutcome::NotModified
        } else {
            RefreshOutcome::Updated { models }
        })
    }

    /// On-demand entitlement/capability discovery for a configured provider.
    /// Never called automatically. No pricing is accepted from this endpoint.
    pub async fn discover_provider(
        &self,
        provider: &str,
        models_url: &str,
        bearer: Option<&str>,
    ) -> Result<RefreshOutcome, CatalogError> {
        if self.0.options.offline {
            return Ok(RefreshOutcome::Offline);
        }
        let _guard = self.0.refresh_lock.lock().await;
        let mut request = http_client()?.get(models_url);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|_| transport_error())?;
        if !response.status().is_success() {
            return Err(CatalogError::Invalid(
                "provider catalog server returned an error",
            ));
        }
        let bytes = bounded_response_body(response, "provider catalog exceeds size limit").await?;
        let value: Value = serde_json::from_slice(&bytes)?;
        let at = Utc::now();
        let mut catalog = (*self.snapshot()).clone();
        let models = catalog.merge_provider_models(provider, &value, at)?;
        self.0
            .provider_layers
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(provider.into(), (value, at));
        *self.0.snapshot.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(catalog);
        Ok(RefreshOutcome::Updated { models })
    }
}

fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("llmshim-catalog/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<(), CatalogError> {
    let parent = path
        .parent()
        .ok_or(CatalogError::Invalid("cache path has no parent"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(data)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn save_cache(path: &Path, data: &str, metadata: &CacheMetadata) -> Result<(), CatalogError> {
    std::fs::create_dir_all(
        path.parent()
            .ok_or(CatalogError::Invalid("cache path has no parent"))?,
    )?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    lock.lock_exclusive()?;
    atomic_write(path, data.as_bytes())?;
    atomic_write(&metadata_path(path), &serde_json::to_vec(metadata)?)?;
    FileExt::unlock(&lock)?;
    Ok(())
}

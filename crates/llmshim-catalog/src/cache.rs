//! Where catalog bytes live between processes: the on-disk snapshot, the local
//! policy layered over it, and the options that name both. Nothing here touches
//! the network and nothing here holds a snapshot.
use crate::{Catalog, CatalogError, CATALOG_URL};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CacheMetadata {
    pub(crate) etag: Option<String>,
    pub(crate) fetched_at: DateTime<Utc>,
    pub(crate) sha256: String,
}

fn metadata_path(path: &Path) -> PathBuf {
    path.with_extension("metadata.json")
}

pub(crate) fn hash(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

pub(crate) fn read_cache(options: &CatalogOptions) -> Option<(String, Option<CacheMetadata>)> {
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

pub(crate) fn apply_overrides(
    catalog: &mut Catalog,
    options: &CatalogOptions,
) -> Result<(), CatalogError> {
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
        let mut local = Catalog::empty();
        apply_overrides(&mut local, options)?;
        let mut catalog = Self::vendored_with_cache(options);
        catalog.merge_layer(&local);
        Ok(catalog)
    }

    /// The vendored floor plus whatever a usable cache adds. The merge parses
    /// into a vector first, so a failed parse cannot partially modify it.
    pub(crate) fn vendored_with_cache(options: &CatalogOptions) -> Self {
        let mut catalog = Self::vendored();
        if let Some((data, meta)) = read_cache(options) {
            let _ = catalog.merge_models_dev(&data, meta.map(|m| m.fetched_at));
        }
        catalog
    }
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

pub(crate) fn save_cache(
    path: &Path,
    data: &str,
    metadata: &CacheMetadata,
) -> Result<(), CatalogError> {
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

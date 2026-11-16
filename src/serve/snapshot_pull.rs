//! Pulling a snapshot from a control plane (0024, 0026): `GET {url}/v1/orgs/{org}/snapshot`
//! with the bearer token and `If-None-Match`, judged exactly as a load would judge it, and
//! kept in `state.policy_dir`, which the replica then reads like any other snapshot file.
//!
//! The control plane is never in the request path: a poll that fails (network, 5xx, a wrong
//! token, a snapshot that does not verify or that this configuration would refuse) changes
//! nothing, the last good snapshot keeps serving, and the reason is a fixed line in the status.
//! Nothing the control plane sends is trusted until it passes the same checks a load applies;
//! nothing here ever includes the token or the body in a message.

use super::config::{read_secret, ServeConfig};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Pulled {
    /// The control plane has nothing newer.
    NotModified,
    /// A newer snapshot passed every check and is now the cache.
    Updated(u64),
}

pub struct Puller {
    client: reqwest::Client,
    directory: PathBuf,
    cache: PathBuf,
    generation: Option<u64>,
}

impl Puller {
    pub fn new(config: &ServeConfig) -> Result<Self, String> {
        let control_plane = config
            .control_plane
            .as_ref()
            .filter(|cp| cp.source.is_some())
            .ok_or_else(|| "no control_plane.source".to_string())?;
        let directory = config
            .state
            .policy_dir
            .clone()
            .ok_or_else(|| "no state.policy_dir".to_string())?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            // A redirect would carry the token to wherever it points.
            .redirect(reqwest::redirect::Policy::none())
            // The body is read with a byte limit as it arrives; a compressed one would be
            // inflated past it before the limit could apply.
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .build()
            .map_err(|_| "the HTTP client could not be built".to_string())?;
        let cache = directory.join("snapshot.json");
        let generation = std::fs::read(&cache).ok().and_then(|bytes| {
            super::snapshot::verify_for_cache(
                &bytes,
                &control_plane.verify_keys,
                &control_plane.org,
            )
            .ok()
        });
        Ok(Self {
            client,
            directory,
            cache,
            generation,
        })
    }

    pub fn poll_interval(config: &ServeConfig) -> Duration {
        Duration::from_secs(
            config
                .control_plane
                .as_ref()
                .and_then(|cp| cp.source.as_ref())
                .and_then(|source| source.poll_interval_secs)
                .unwrap_or(15),
        )
    }

    /// Whether a verified snapshot is already cached.
    pub fn has_cache(&self) -> bool {
        self.generation.is_some()
    }

    /// Whether a cache file exists at all (it may not verify any longer).
    pub fn cache_file_exists(&self) -> bool {
        self.cache.exists()
    }

    /// The token, read from its reference now: at each poll, so a rotated file takes effect.
    pub fn token(
        config: &ServeConfig,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<String, String> {
        let source = config
            .control_plane
            .as_ref()
            .and_then(|cp| cp.source.as_ref())
            .ok_or_else(|| "no control_plane.source".to_string())?;
        read_secret(&source.token, env)
            .map_err(|_| "the control plane token cannot be read".to_string())
    }

    /// One poll against the **current** configuration (its `verify_keys` may have changed since
    /// the start). An `Err` is a fixed reason, safe to log and to show in the status.
    pub async fn pull(&mut self, config: &ServeConfig, token: String) -> Result<Pulled, String> {
        let control_plane = config
            .control_plane
            .as_ref()
            .ok_or_else(|| "no control_plane".to_string())?;
        let source = control_plane
            .source
            .as_ref()
            .ok_or_else(|| "no control_plane.source".to_string())?;
        let url = format!(
            "{}/v1/orgs/{}/snapshot",
            source.url.trim_end_matches('/'),
            control_plane.org
        );
        let mut request = self.client.get(&url).bearer_auth(token);
        if let Some(generation) = self.generation {
            request = request.header("if-none-match", format!("\"{generation}\""));
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| "the control plane is unreachable".to_string())?;
        match response.status().as_u16() {
            304 => return Ok(Pulled::NotModified),
            200 => {}
            401 => return Err("the control plane refused the token (401)".into()),
            404 => {
                return Err("the control plane has no snapshot for this organization (404)".into())
            }
            status => return Err(format!("the control plane answered {status}")),
        }
        // Read as it arrives, with a limit: Content-Length may be absent or a lie.
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > MAX_BODY {
                        return Err("the snapshot is larger than 8 MiB".into());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return Err("the snapshot could not be read in full".into()),
            }
        }
        let generation = super::snapshot::verify_for_cache(
            &bytes,
            &control_plane.verify_keys,
            &control_plane.org,
        )
        .map_err(|reason| format!("the pulled snapshot was refused: {reason}"))?;
        if self.generation.is_some_and(|known| generation <= known) {
            // Verified but not newer: the cache keeps what it has.
            return Ok(Pulled::NotModified);
        }
        // Beside the cache, never following a link, synced; then judged exactly as a load would
        // judge it (the floor, `min_generation`, the key rules) BEFORE it can replace the cache:
        // a snapshot that is signed but unusable must not survive a restart.
        let temp = self
            .write_temp(&bytes)
            .map_err(|_| "the pulled snapshot could not be stored in policy_dir".to_string())?;
        if let Err(issues) = config.check_snapshot_candidate(&temp) {
            let _ = std::fs::remove_file(&temp);
            let reason = issues
                .first()
                .map(|issue| issue.message.clone())
                .unwrap_or_default();
            return Err(format!("the pulled snapshot was refused: {reason}"));
        }
        if std::fs::rename(&temp, &self.cache).is_err() {
            let _ = std::fs::remove_file(&temp);
            return Err("the pulled snapshot could not be stored in policy_dir".into());
        }
        if let Ok(handle) = std::fs::File::open(&self.directory) {
            let _ = handle.sync_all();
        }
        self.generation = Some(generation);
        Ok(Pulled::Updated(generation))
    }

    fn write_temp(&self, bytes: &[u8]) -> std::io::Result<PathBuf> {
        use std::io::Write;
        std::fs::create_dir_all(&self.directory)?;
        let name = format!(
            ".snapshot.{}.{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = self.directory.join(name);
        // create_new: an existing file or a link of that name is an error, never followed.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(path)
    }
}

/// The directory a pulled snapshot is kept in, for messages.
pub fn cache_path(directory: &Path) -> PathBuf {
    directory.join("snapshot.json")
}

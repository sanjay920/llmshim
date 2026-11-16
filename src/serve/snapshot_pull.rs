//! Pulling a snapshot from a control plane (0024, 0026): `GET {url}/v1/orgs/{org}/snapshot`
//! with the bearer token and `If-None-Match`, verified against `verify_keys` and kept in
//! `state.policy_dir`, which the replica then reads like any other snapshot file.
//!
//! The control plane is never in the request path: a poll that fails (network, 5xx, a wrong
//! token, a snapshot that does not verify) changes nothing, the last good snapshot keeps
//! serving, and the reason is a fixed line in the status. Nothing the control plane sends is
//! trusted until it verifies; nothing here ever includes the token or the body in a message.

use super::config::{read_secret, ControlPlane, SnapshotSource};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Pulled {
    /// The control plane has nothing newer.
    NotModified,
    /// A newer snapshot was verified and is now the cache.
    Updated(u64),
}

pub struct Puller {
    client: reqwest::Client,
    url: String,
    org: String,
    verify_keys: Vec<String>,
    source: SnapshotSource,
    cache: PathBuf,
    directory: PathBuf,
    generation: Option<u64>,
}

impl Puller {
    pub fn new(control_plane: &ControlPlane, policy_dir: &Path) -> Result<Self, String> {
        let source = control_plane
            .source
            .clone()
            .ok_or_else(|| "no control_plane.source".to_string())?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            // A redirect would carry the token to wherever it points.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "the HTTP client could not be built".to_string())?;
        let cache = policy_dir.join("snapshot.json");
        let mut puller = Self {
            client,
            url: format!(
                "{}/v1/orgs/{}/snapshot",
                source.url.trim_end_matches('/'),
                control_plane.org
            ),
            org: control_plane.org.clone(),
            verify_keys: control_plane.verify_keys.clone(),
            source,
            cache,
            directory: policy_dir.to_path_buf(),
            generation: None,
        };
        // What the cache already holds (a restart): the generation to ask "newer than" about.
        puller.generation = std::fs::read(&puller.cache).ok().and_then(|bytes| {
            super::snapshot::verify_for_cache(&bytes, &puller.verify_keys, &puller.org).ok()
        });
        Ok(puller)
    }

    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.source.poll_interval_secs.unwrap_or(15))
    }

    /// Whether a verified snapshot is already cached.
    pub fn has_cache(&self) -> bool {
        self.generation.is_some()
    }

    /// The token, read from its reference now: at each poll, so a rotated file takes effect.
    pub fn token(&self, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
        read_secret(&self.source.token, env)
            .map_err(|_| "the control plane token cannot be read".to_string())
    }

    /// One poll with the token from [`Self::token`]. An `Err` is a fixed reason, safe to log
    /// and to show in the status.
    pub async fn pull(&mut self, token: String) -> Result<Pulled, String> {
        let mut request = self.client.get(&self.url).bearer_auth(token);
        if let Some(generation) = self.generation {
            request = request.header("if-none-match", format!("\"{generation}\""));
        }
        let response = request
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
        if response
            .content_length()
            .is_some_and(|n| n as usize > MAX_BODY)
        {
            return Err("the snapshot is larger than 8 MiB".into());
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| "the snapshot could not be read in full".to_string())?;
        if bytes.len() > MAX_BODY {
            return Err("the snapshot is larger than 8 MiB".into());
        }
        let generation = super::snapshot::verify_for_cache(&bytes, &self.verify_keys, &self.org)
            .map_err(|reason| format!("the pulled snapshot was refused: {reason}"))?;
        if self.generation.is_some_and(|known| generation <= known) {
            // Verified but not newer: the cache keeps what it has.
            return Ok(Pulled::NotModified);
        }
        // Written beside the cache and renamed: a reader sees the old file or the new, never half.
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&self.directory)?;
            let temp = self
                .directory
                .join(format!(".snapshot.{}.tmp", std::process::id()));
            std::fs::write(&temp, &bytes)?;
            std::fs::rename(&temp, &self.cache)
        };
        write().map_err(|_| "the pulled snapshot could not be stored in policy_dir".to_string())?;
        self.generation = Some(generation);
        Ok(Pulled::Updated(generation))
    }
}

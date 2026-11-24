//! Reading the upstream catalog and provider listings, and the handle methods
//! that do it. Every call here is explicit: a refresh is something a caller
//! asks for, it stages what it fetched, and it builds no snapshot.
use crate::{
    cache::{hash, read_cache, save_cache, CacheMetadata, MAX_CATALOG_BYTES},
    handle::CatalogHandle,
    Catalog, CatalogError,
};
use chrono::Utc;
use serde_json::Value;
use std::time::Duration;

pub(crate) fn transport_error(error: reqwest::Error) -> CatalogError {
    // reqwest includes the request URL in its Display implementation. Catalog
    // endpoints may use query credentials, so keep transport diagnostics URL-free.
    CatalogError::Http(error.without_url())
}

pub(crate) async fn bounded_response_body(
    mut response: reqwest::Response,
    size_error: &'static str,
) -> Result<Vec<u8>, CatalogError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_CATALOG_BYTES {
            return Err(CatalogError::Invalid(size_error));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("llmshim-catalog/", env!("CARGO_PKG_VERSION")))
        .build()
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

impl CatalogHandle {
    /// Starts background refresh only when a Tokio runtime is already present.
    /// Does not create a runtime or make synchronous network requests.
    pub fn refresh_in_background(&self) -> Option<tokio::task::JoinHandle<RefreshOutcome>> {
        if self.inner().options.offline {
            return None;
        }
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let handle = self.clone();
        Some(runtime.spawn(async move { handle.refresh(false).await }))
    }

    pub async fn refresh(&self, force: bool) -> RefreshOutcome {
        if self.inner().options.offline {
            return RefreshOutcome::Offline;
        }
        let _guard = self.inner().refresh_lock.lock().await;
        match self.try_refresh(force).await {
            Ok(outcome) => outcome,
            Err(e) => RefreshOutcome::Stale {
                reason: e.to_string(),
            },
        }
    }

    async fn try_refresh(&self, force: bool) -> Result<RefreshOutcome, CatalogError> {
        let options = &self.inner().options;
        // Disk work is independent of request snapshots and never takes their lock.
        let opts = options.clone();
        let cache = tokio::task::spawn_blocking(move || read_cache(&opts))
            .await
            .map_err(|_| CatalogError::Invalid("catalog cache reader stopped"))?;
        // A valid ETag is tied to the exact validated body, not just a sidecar.
        let cache = cache.filter(|(data, _)| crate::parse::models_dev(data, None).is_ok());
        let meta = cache.as_ref().and_then(|(_, m)| m.as_ref());
        let last_refresh = *self
            .inner()
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
        let response = request.send().await.map_err(transport_error)?;
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
            if response.status().is_redirection() {
                return Err(CatalogError::Invalid("catalog redirects are not allowed"));
            }
            let response = response.error_for_status().map_err(transport_error)?;
            if !response.status().is_success() {
                return Err(CatalogError::Invalid(
                    "catalog returned an unexpected status",
                ));
            }
            let bytes = bounded_response_body(response, "catalog exceeds size limit").await?;
            String::from_utf8(bytes).map_err(|_| CatalogError::Invalid("catalog is not UTF-8"))?
        };
        let fetched_at = Utc::now();
        // Parse and validate before anything is stored, so a rejected body
        // cannot modify the handle; the merge into a catalog waits for a read.
        let models = crate::parse::models_dev(&data, Some(fetched_at))?;
        let count = models.len();
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
        // Staging under the fold lock, past every await: a fold that is already
        // running stores before this runs, so its snapshot cannot hide these rows.
        let _stage = self.fold_guard();
        *self
            .inner()
            .remote
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Some(models);
        *self
            .inner()
            .last_refresh
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Some(fetched_at);
        self.forget_snapshot();
        Ok(if not_modified {
            RefreshOutcome::NotModified
        } else {
            RefreshOutcome::Updated { models: count }
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
        if self.inner().options.offline {
            return Ok(RefreshOutcome::Offline);
        }
        let _guard = self.inner().refresh_lock.lock().await;
        let mut request = http_client()?.get(models_url);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(transport_error)?;
        if response.status().is_redirection() {
            return Err(CatalogError::Invalid(
                "provider catalog redirects are not allowed",
            ));
        }
        let response = response.error_for_status().map_err(transport_error)?;
        if !response.status().is_success() {
            return Err(CatalogError::Invalid(
                "provider catalog returned an unexpected status",
            ));
        }
        let bytes = bounded_response_body(response, "provider catalog exceeds size limit").await?;
        let value: Value =
            crate::bounded_json::parse_slice(&bytes, crate::bounded_json::Limits::CATALOG)
                .map_err(|_| {
                    CatalogError::Invalid("provider catalog JSON is invalid or too complex")
                })?;
        let at = Utc::now();
        // Validate against a throwaway catalog: the import budget is a property
        // of the listing, not of what the catalog already holds, and a refused
        // listing must leave the retained layers exactly as they were.
        let mut validation = Catalog::empty();
        let models = validation.merge_provider_models(provider, &value, at)?;
        let _stage = self.fold_guard();
        self.inner()
            .provider_layers
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(provider.into(), (value, at));
        self.forget_snapshot();
        Ok(RefreshOutcome::Updated { models })
    }
}

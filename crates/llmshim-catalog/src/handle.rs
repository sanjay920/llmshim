//! The resident handle. A snapshot is built on the first read and kept; the
//! layers a refresh or a provider discovery fetch are staged here and folded in
//! when someone asks, so no startup path and no fetch builds a catalog.
use crate::{
    cache::{apply_overrides, CatalogOptions},
    Catalog, CatalogError,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, MutexGuard, RwLock},
};

pub(crate) struct Inner {
    pub(crate) options: CatalogOptions,
    /// Local policy, validated once when the handle was built. It is the one
    /// layer whose *validity* is a construction concern; every other layer is
    /// discovered data, and none of it is folded until someone reads it.
    pub(crate) local: Catalog,
    /// `None` until the first read, and again after a refresh or a provider
    /// discovery stages a newer layer.
    snapshot: RwLock<Option<Arc<Catalog>>>,
    /// Serializes the one fold against itself and against every stage, so
    /// concurrent first reads build once and a fold in flight cannot store a
    /// snapshot that predates a layer staged while it ran.
    fold_lock: std::sync::Mutex<()>,
    /// A refresh's rows, staged rather than merged. Supersedes the cache file
    /// they were written to, which holds the same body.
    pub(crate) remote: RwLock<Option<Vec<crate::ModelInfo>>>,
    pub(crate) refresh_lock: tokio::sync::Mutex<()>,
    pub(crate) provider_layers: RwLock<BTreeMap<String, (Value, DateTime<Utc>)>>,
    pub(crate) last_refresh: RwLock<Option<DateTime<Utc>>>,
}

/// A resident catalog. Requests borrow a snapshot immediately while detached
/// refreshes stage a replacement; a network failure never removes the old view.
#[derive(Clone)]
pub struct CatalogHandle(Arc<Inner>);

impl CatalogHandle {
    /// Builds a handle, not a catalog: the vendored snapshot and the disk cache
    /// wait for [`snapshot`](Self::snapshot), so a process that never looks up a
    /// model never pays for the 4.6 MB fold. `refresh` and `discover_provider`
    /// are the only readers of the network, and neither runs on its own.
    ///
    /// Local policy is the deliberate exception. A malformed override is a
    /// configuration error the caller must see here, not on the first request
    /// that happens to want a price, and the files are a few kilobytes of TOML.
    pub fn load(options: CatalogOptions) -> Result<Self, CatalogError> {
        let mut local = Catalog::empty();
        apply_overrides(&mut local, &options)?;
        Ok(Self(Arc::new(Inner {
            options,
            local,
            snapshot: RwLock::new(None),
            fold_lock: std::sync::Mutex::new(()),
            remote: RwLock::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
            provider_layers: RwLock::new(BTreeMap::new()),
            last_refresh: RwLock::new(None),
        })))
    }

    /// The resident view, built on the first call. This is the catalog's one
    /// blocking fold — a 4.6 MB disk read and parse on the calling thread — so a
    /// caller on a Tokio worker should warm it once at startup rather than let
    /// the first request pay for it. Every later call is a lock and an `Arc`
    /// clone until a staged layer drops it.
    pub fn snapshot(&self) -> Arc<Catalog> {
        if let Some(snapshot) = self.built() {
            return snapshot;
        }
        let _fold = self.fold_guard();
        if let Some(snapshot) = self.built() {
            return snapshot;
        }
        let catalog = Arc::new(self.fold());
        self.store(catalog.clone());
        catalog
    }

    /// Hold the fold lock. A caller that stages a layer does so under this lock,
    /// so its rows cannot be lost under a fold that started before it.
    pub(crate) fn fold_guard(&self) -> MutexGuard<'_, ()> {
        self.0
            .fold_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The handle's shared state, for the refresh paths that stage into it.
    pub(crate) fn inner(&self) -> &Inner {
        &self.0
    }

    fn built(&self) -> Option<Arc<Catalog>> {
        self.0
            .snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn store(&self, catalog: Arc<Catalog>) {
        *self
            .0
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(catalog);
    }

    /// Forget the built snapshot so the next read picks up a newly staged layer.
    /// The caller holds [`fold_guard`](Self::fold_guard).
    pub(crate) fn forget_snapshot(&self) {
        *self
            .0
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// The one place a `Catalog` is built, and it touches only the disk. The
    /// network layers are staged by whoever fetched them.
    fn fold(&self) -> Catalog {
        let remote = self
            .0
            .remote
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut catalog = match remote.as_ref() {
            // A staged layer is the cache file's own body, newer than the file.
            Some(models) => {
                let mut floor = Catalog::vendored();
                for model in models {
                    floor.merge_model(model.clone());
                }
                floor
            }
            None => Catalog::vendored_with_cache(&self.0.options),
        };
        drop(remote);
        for (provider, (value, at)) in self
            .0
            .provider_layers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
        {
            // `discover_provider` refuses a listing before staging it, so a
            // layer that is here has already passed the import budget.
            catalog
                .merge_provider_models(provider, value, *at)
                .expect("a staged provider layer is validated before it is stored");
        }
        catalog.merge_layer(&self.0.local);
        catalog
    }
}

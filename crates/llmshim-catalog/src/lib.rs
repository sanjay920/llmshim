//! Model metadata with no dependency on the translation crate.
//! All loading is local. Network refresh is explicit or detached from requests.
pub mod aliases;
pub mod builtin;
mod capabilities;
mod merge;
mod parse;
mod refresh;
mod types;

pub use capabilities::{ModelCapabilities, Support};
use chrono::{DateTime, Utc};
pub use refresh::{CatalogHandle, CatalogOptions, RefreshOutcome};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock};
pub use types::*;

pub const VENDORED_SNAPSHOT: &str = include_str!("../data/models.dev.json");
pub const CATALOG_URL: &str = "https://models.dev/api.json";

static GLOBAL: LazyLock<Result<CatalogHandle, CatalogError>> =
    LazyLock::new(|| CatalogHandle::load(CatalogOptions::default()));

/// Shared resident catalog. Initialization reads only local files; callers may
/// schedule `refresh_in_background` after startup. Invalid local policy stays
/// visible as an error instead of silently reverting to community defaults.
pub fn global() -> Result<&'static CatalogHandle, &'static CatalogError> {
    GLOBAL.as_ref()
}

/// Resolve against the current snapshot. The owned result remains valid across
/// a concurrent refresh; `Catalog::resolve` offers a borrowed equivalent.
pub fn resolve(id: &str) -> Option<Arc<ModelInfo>> {
    global().ok()?.snapshot().resolve(id).cloned().map(Arc::new)
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("catalog I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("catalog JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("catalog TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("catalog HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid catalog: {0}")]
    Invalid(&'static str),
}

/// An immutable-to-read snapshot. Cloning a handle is cheap; use `snapshot()` to
/// hold this view consistently across a request and its continuations.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    models: BTreeMap<String, ModelInfo>,
    aliases: BTreeMap<String, String>,
    spellings: BTreeMap<String, BTreeSet<String>>,
    bare_spellings: BTreeMap<String, BTreeSet<String>>,
}

impl Catalog {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn vendored() -> Self {
        let mut catalog = Self::empty();
        // The embedded artifact is validated before shipping and in CI.
        catalog
            .merge_models_dev(VENDORED_SNAPSHOT, None)
            .expect("validated vendored model catalog");
        catalog.merge_builtins();
        catalog
    }

    pub fn models(&self) -> impl Iterator<Item = &ModelInfo> {
        self.models.values()
    }

    pub fn resolve(&self, id: &str) -> Option<&ModelInfo> {
        let mut key = id;
        let mut seen = BTreeSet::new();
        while let Some(alias) = self.aliases.get(key) {
            if !seen.insert(key) {
                return None;
            }
            key = alias;
        }
        if let Some(model) = self.models.get(key) {
            return Some(model);
        }
        // Verified direct routes disambiguate bare ids also sold by resellers.
        if let Some(builtin) = builtin::spec(key) {
            if let Some(model) = self.models.get(builtin.id) {
                return Some(model);
            }
        }
        // Full routing aliases take precedence over a reseller's bare name
        // that happens to contain the same vendor/model slash.
        let matches = self
            .spellings
            .get(key)
            .or_else(|| self.bare_spellings.get(key))?;
        if matches.len() != 1 {
            return None;
        }
        self.models.get(matches.first()?)
    }

    pub fn alias(&mut self, alias: impl Into<String>, target: impl Into<String>) {
        self.aliases.insert(alias.into(), target.into());
    }

    pub fn merge_model(&mut self, model: ModelInfo) {
        let target = self.models.entry(model.id.clone()).or_insert_with(|| {
            let mut entry = ModelInfo::new(&model.provider, &model.name);
            entry.source = model.source;
            entry
        });
        merge::merge(target, &model);
        self.index_model(&model);
    }

    fn index_model(&mut self, model: &ModelInfo) {
        let mut names = vec![model.name.clone()];
        names.extend(aliases::claude_version_spellings(&model.name));
        // Profile prefixes are aliases for lookup only. Keep the exact wire id.
        for prefix in aliases::REGION_PREFIXES {
            if let Some(name) = model.name.strip_prefix(prefix) {
                names.push(name.into());
            }
        }
        for name in names {
            self.bare_spellings
                .entry(name.clone())
                .or_default()
                .insert(model.id.clone());
            self.spellings
                .entry(format!("{}/{name}", model.provider))
                .or_default()
                .insert(model.id.clone());
            for (alias, canonical) in aliases::PROVIDER_ALIASES {
                if model.provider == *canonical {
                    self.spellings
                        .entry(format!("{alias}/{name}"))
                        .or_default()
                        .insert(model.id.clone());
                }
            }
        }
    }

    pub fn merge_builtins(&mut self) {
        for m in builtin::all() {
            let mut model = ModelInfo::new(m.provider, m.name);
            model.label = m.label.into();
            model.context_window_tokens = m.context_window_tokens;
            model.max_output_tokens = m.max_output_tokens;
            model.capabilities = m.capabilities;
            model.family = m.family;
            if m.provider == "anthropic" {
                model.reasoning_options = builtin::anthropic_reasoning_options(m.name);
            }
            model.source = CatalogSource::Builtin;
            self.merge_model(model);
        }
    }

    pub fn merge_models_dev(
        &mut self,
        data: &str,
        fetched_at: Option<DateTime<Utc>>,
    ) -> Result<usize, CatalogError> {
        let models = parse::models_dev(data, fetched_at)?;
        let count = models.len();
        for model in models {
            self.merge_model(model);
        }
        Ok(count)
    }

    /// OpenAI-compatible `/models` discovery. Only identity/capability fields
    /// are accepted, never pricing, even if a server includes cost fields.
    pub fn merge_provider_models(
        &mut self,
        provider: &str,
        response: &Value,
        fetched_at: DateTime<Utc>,
    ) -> Result<usize, CatalogError> {
        let data = response["data"].as_array().ok_or(CatalogError::Invalid(
            "provider models must contain a data array",
        ))?;
        let mut models = Vec::new();
        for value in data {
            let name = value["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(CatalogError::Invalid("provider model has no id"))?;
            let mut model = parse::model_from_value(
                provider,
                name,
                value,
                CatalogSource::ProviderApi,
                Some(fetched_at),
            );
            if let Some(caps) = value.get("capabilities") {
                model.capabilities = serde_json::from_value(caps.clone())?;
            }
            models.push(model);
        }
        let count = models.len();
        for model in models {
            self.merge_model(model);
        }
        Ok(count)
    }

    /// Local TOML accepts `[models."provider/model"]` tables and `[aliases]`.
    /// Omitted fields never erase facts from another layer.
    pub fn merge_local_toml(&mut self, data: &str) -> Result<(), CatalogError> {
        let toml: toml::Value = toml::from_str(data)?;
        let value = serde_json::to_value(toml)?;
        let mut pending = Vec::new();
        if let Some(models) = value.get("models") {
            let models = models
                .as_object()
                .ok_or(CatalogError::Invalid("local models must be a table"))?;
            for (id, v) in models {
                let (provider, name) = id
                    .split_once('/')
                    .filter(|(p, n)| !p.is_empty() && !n.is_empty())
                    .ok_or(CatalogError::Invalid(
                        "local model id must be provider/model",
                    ))?;
                let mut model =
                    parse::model_from_value(provider, name, v, CatalogSource::Local, None);
                if let Some(label) = v["label"].as_str() {
                    model.label = label.into();
                }
                if let Some(caps) = v.get("capabilities") {
                    model.capabilities = serde_json::from_value(caps.clone())?;
                }
                if v.get("family").is_some() && model.family.is_none() {
                    return Err(CatalogError::Invalid("unrecognized local model family"));
                }
                pending.push(model);
            }
        }
        let mut aliases = Vec::new();
        if let Some(values) = value.get("aliases") {
            for (alias, target) in values
                .as_object()
                .ok_or(CatalogError::Invalid("aliases must be a table"))?
            {
                aliases.push((
                    alias.clone(),
                    target
                        .as_str()
                        .ok_or(CatalogError::Invalid("alias target must be a string"))?
                        .to_string(),
                ));
            }
        }
        // Parse everything before applying, so errors cannot leave a half layer.
        for model in pending {
            self.merge_model(model);
        }
        for (alias, target) in aliases {
            self.alias(alias, target);
        }
        Ok(())
    }
}

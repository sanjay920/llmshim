use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Coarse reasoning compatibility key, independent of the serving provider.
/// Unknown families remain absent; two unknown models must not become compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ModelFamily {
    Claude,
    Gpt,
    Gemini,
    Grok,
    Deepseek,
    Qwen,
    Llama,
    Kimi,
    Mistral,
    Glm,
    Minimax,
    Command,
    Nova,
    Gemma,
    Phi,
    Nemotron,
    Olmo,
    Seed,
    Doubao,
}

impl ModelFamily {
    /// Normalize a catalog's product-level family (e.g. `claude-sonnet`).
    /// This intentionally does not guess from arbitrary model names.
    pub fn from_catalog_key(key: &str) -> Option<Self> {
        let lower = key.to_ascii_lowercase();
        let base = lower.rsplit('/').next()?;
        let family = base.split(['-', '_', '.', ' ']).next()?;
        Some(match family {
            "claude" => Self::Claude,
            "gpt" | "o1" | "o3" | "o4" => Self::Gpt,
            "gemini" => Self::Gemini,
            "grok" => Self::Grok,
            "deepseek" => Self::Deepseek,
            "qwen" | "qwen2" | "qwen3" => Self::Qwen,
            "llama" | "llama2" | "llama3" | "llama4" => Self::Llama,
            "kimi" => Self::Kimi,
            "mistral" | "mixtral" | "codestral" | "devstral" | "magistral" => Self::Mistral,
            "glm" | "glm4" | "glm5" => Self::Glm,
            "minimax" => Self::Minimax,
            "command" => Self::Command,
            "nova" => Self::Nova,
            "gemma" | "gemma2" | "gemma3" => Self::Gemma,
            "phi" => Self::Phi,
            "nemotron" => Self::Nemotron,
            "olmo" => Self::Olmo,
            "seed" => Self::Seed,
            "doubao" => Self::Doubao,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    #[default]
    Builtin,
    ModelsDev,
    Local,
    ProviderApi,
}

/// USD per million tokens. Missing prices are unknown, never free.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Cost {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

/// Rates for the entire request when total input (including cached input)
/// exceeds `above_input_tokens`. Missing rates inherit the previous tier.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContextCostTier {
    pub above_input_tokens: u64,
    pub cost: Cost,
}

/// Media types are strings so new catalog modalities survive without a release.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Modalities {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    Effort { values: Vec<EffortLevel> },
    BudgetTokens { min: Option<u32>, max: Option<u32> },
}

/// Owned metadata, usable without the translation crate or a network connection.
/// `field_sources` records provenance per asserted field after merging layers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    /// Exact upstream spelling, including region/profile prefixes where required.
    pub name: String,
    pub label: String,
    pub context_window_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub capabilities: crate::ModelCapabilities,
    pub family: Option<ModelFamily>,
    pub cost: Option<Cost>,
    /// None means unspecified; an explicit empty list disables inherited tiers.
    #[serde(default)]
    pub context_cost_tiers: Option<Vec<ContextCostTier>>,
    pub reasoning_options: Vec<ReasoningOption>,
    pub modalities: Modalities,
    pub knowledge_cutoff: Option<NaiveDate>,
    pub release_date: Option<NaiveDate>,
    pub open_weights: Option<bool>,
    pub source: CatalogSource,
    pub fetched_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub field_sources: BTreeMap<String, CatalogSource>,
}

impl ModelInfo {
    pub fn new(provider: impl Into<String>, name: impl Into<String>) -> Self {
        let provider = provider.into();
        let name = name.into();
        Self {
            id: format!("{provider}/{name}"),
            provider,
            label: name.clone(),
            name,
            context_window_tokens: None,
            max_output_tokens: None,
            capabilities: crate::ModelCapabilities::unknown(),
            family: None,
            cost: None,
            context_cost_tiers: None,
            reasoning_options: Vec::new(),
            modalities: Modalities::default(),
            knowledge_cutoff: None,
            release_date: None,
            open_weights: None,
            source: CatalogSource::Builtin,
            fetched_at: None,
            field_sources: BTreeMap::new(),
        }
    }

    /// Resolve context-dependent rates without changing the base `Cost` API.
    /// Higher-priority per-field base overrides also override lower-priority
    /// tier rates. Tier lists are replaced atomically by a stronger source.
    pub fn cost_for_input_tokens(&self, input_tokens: u64) -> Option<Cost> {
        let mut cost = self.cost?;
        let mut tiers: Vec<_> = self
            .context_cost_tiers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|t| input_tokens > t.above_input_tokens)
            .collect();
        tiers.sort_by_key(|t| t.above_input_tokens);
        let tier_source = self
            .field_sources
            .get("context_cost_tiers")
            .copied()
            .unwrap_or(self.source);
        for tier in tiers {
            for (key, rate, slot) in [
                ("cost.input", tier.cost.input, &mut cost.input),
                ("cost.output", tier.cost.output, &mut cost.output),
                (
                    "cost.cache_read",
                    tier.cost.cache_read,
                    &mut cost.cache_read,
                ),
                (
                    "cost.cache_write",
                    tier.cost.cache_write,
                    &mut cost.cache_write,
                ),
            ] {
                let base_source = self.field_sources.get(key).copied().unwrap_or(self.source);
                if rate.is_some_and(|n| n.is_finite() && n >= 0.0)
                    && crate::merge::rank(tier_source) >= crate::merge::rank(base_source)
                {
                    *slot = rate;
                }
            }
        }
        Some(cost)
    }
}

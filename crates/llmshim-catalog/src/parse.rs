use crate::{
    CatalogError, CatalogSource, ContextCostTier, Cost, ModelCapabilities, ModelFamily, ModelInfo,
    Support,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;

pub(crate) fn provider_key(key: &str) -> &str {
    crate::aliases::provider_key(key)
}

fn support(value: &Value) -> Support {
    match value.as_bool() {
        Some(true) => Support::Supported,
        Some(false) => Support::Unsupported,
        None => serde_json::from_value(value.clone()).unwrap_or_default(),
    }
}

fn count(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|n| n.try_into().ok())
}

fn date(value: &Value) -> Option<NaiveDate> {
    // A month-only or year-only date does not assert a particular day.
    NaiveDate::parse_from_str(value.as_str()?, "%Y-%m-%d").ok()
}

fn cost(value: &Value) -> Option<Cost> {
    let obj = value.as_object()?;
    let price = |key| {
        obj.get(key)
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    Some(Cost {
        input: price("input"),
        output: price("output"),
        cache_read: price("cache_read"),
        cache_write: price("cache_write"),
    })
}

pub(crate) fn model_from_value(
    provider: &str,
    name: &str,
    v: &Value,
    source: CatalogSource,
    fetched_at: Option<DateTime<Utc>>,
) -> ModelInfo {
    let mut m = ModelInfo::new(provider_key(provider), name);
    // Dotted Claude versions are catalog aliases, not native Anthropic API ids.
    if m.provider == "anthropic" && name.starts_with("claude-") {
        m.name = name.replace('.', "-");
        m.id = format!("{}/{}", m.provider, m.name);
    }
    m.label = v["name"].as_str().unwrap_or(name).into();
    m.context_window_tokens =
        count(&v["limit"]["context"]).or_else(|| count(&v["context_window_tokens"]));
    m.max_output_tokens = count(&v["limit"]["output"]).or_else(|| count(&v["max_output_tokens"]));
    m.family = v["family"].as_str().and_then(ModelFamily::from_catalog_key);
    m.capabilities = ModelCapabilities {
        tools: support(&v["tool_call"]),
        reasoning: support(&v["reasoning"]),
        structured_output: support(&v["structured_output"]),
        streaming: support(&v["streaming"]),
        images: Support::Unknown,
        prompt_cache: support(&v["prompt_cache"]),
        parallel_tool_calls: support(&v["parallel_tool_calls"]),
        forced_tool_choice: support(&v["forced_tool_choice"]),
    };
    m.modalities = serde_json::from_value(v["modalities"].clone()).unwrap_or_default();
    if v["modalities"]["input"].is_array() {
        m.capabilities.images = if m.modalities.input.iter().any(|i| i == "image") {
            Support::Supported
        } else {
            Support::Unsupported
        };
    }
    m.cost = cost(&v["cost"]);
    m.context_cost_tiers = if let Some(tiers) = v.get("context_cost_tiers") {
        serde_json::from_value(tiers.clone()).ok()
    } else {
        v["cost"]["tiers"].as_array().map(|tiers| {
            tiers
                .iter()
                .filter_map(|tier| {
                    if tier["tier"]["type"] != "context" {
                        return None;
                    }
                    Some(ContextCostTier {
                        above_input_tokens: tier["tier"]["size"].as_u64()?,
                        cost: cost(tier)?,
                    })
                })
                .collect()
        })
    };
    m.reasoning_options = v["reasoning_options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| serde_json::from_value(o.clone()).ok())
        .collect();
    m.knowledge_cutoff = date(&v["knowledge"]);
    m.release_date = date(&v["release_date"]);
    m.open_weights = v["open_weights"].as_bool();
    m.source = source;
    m.fetched_at = fetched_at;
    m
}

pub(crate) fn models_dev(
    data: &str,
    fetched_at: Option<DateTime<Utc>>,
) -> Result<Vec<ModelInfo>, CatalogError> {
    let root: Value = serde_json::from_str(data)?;
    let providers = root
        .as_object()
        .ok_or(CatalogError::Invalid("catalog must be an object"))?;
    let mut models = Vec::new();
    for (provider, p) in providers {
        let Some(entries) = p["models"].as_object() else {
            continue;
        };
        for (name, v) in entries {
            if !v.is_object() || name.is_empty() {
                continue;
            }
            models.push(model_from_value(
                provider,
                name,
                v,
                CatalogSource::ModelsDev,
                fetched_at,
            ));
        }
    }
    if models.is_empty() {
        return Err(CatalogError::Invalid("catalog contains no models"));
    }
    Ok(models)
}

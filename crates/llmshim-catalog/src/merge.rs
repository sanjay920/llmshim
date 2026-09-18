use crate::{CatalogSource, Cost, ModelInfo, Support};

fn rank(source: CatalogSource) -> u8 {
    match source {
        CatalogSource::ModelsDev => 1,
        CatalogSource::Builtin => 2,
        CatalogSource::ProviderApi => 3,
        CatalogSource::Local => 4,
    }
}

pub(crate) fn merge(target: &mut ModelInfo, incoming: &ModelInfo) {
    let source = incoming.source;
    let accepts = |field: &str, target: &ModelInfo| {
        target
            .field_sources
            .get(field)
            .is_none_or(|old| rank(source) >= rank(*old))
    };
    let mut changed = false;
    macro_rules! field {
        ($name:ident, $asserts:expr) => {
            if $asserts && accepts(stringify!($name), target) {
                target.$name = incoming.$name.clone();
                target
                    .field_sources
                    .insert(stringify!($name).into(), source);
                changed = true;
            }
        };
    }
    // Provider listings assert identity and capabilities, never prices or billing facts.
    field!(label, incoming.label != incoming.name);
    field!(
        context_window_tokens,
        incoming.context_window_tokens.is_some()
    );
    field!(max_output_tokens, incoming.max_output_tokens.is_some());
    field!(family, incoming.family.is_some());
    field!(reasoning_options, !incoming.reasoning_options.is_empty());
    if source != CatalogSource::ProviderApi {
        field!(knowledge_cutoff, incoming.knowledge_cutoff.is_some());
        field!(release_date, incoming.release_date.is_some());
        field!(open_weights, incoming.open_weights.is_some());
    }
    macro_rules! capability {
        ($name:ident) => {
            let key = concat!("capabilities.", stringify!($name));
            if incoming.capabilities.$name != Support::Unknown && accepts(key, target) {
                target.capabilities.$name = incoming.capabilities.$name;
                target.field_sources.insert(key.into(), source);
                changed = true;
            }
        };
    }
    capability!(tools);
    capability!(streaming);
    capability!(images);
    capability!(prompt_cache);
    capability!(structured_output);
    capability!(parallel_tool_calls);
    capability!(reasoning);
    capability!(forced_tool_choice);
    for (key, values) in [
        ("modalities.input", &incoming.modalities.input),
        ("modalities.output", &incoming.modalities.output),
    ] {
        if !values.is_empty() && accepts(key, target) {
            if key == "modalities.input" {
                target.modalities.input = values.clone();
            } else {
                target.modalities.output = values.clone();
            }
            target.field_sources.insert(key.into(), source);
            changed = true;
        }
    }
    if source != CatalogSource::ProviderApi {
        if let Some(cost) = incoming.cost {
            let mut merged = target.cost.unwrap_or_default();
            for (key, value, slot) in [
                ("cost.input", cost.input, &mut merged.input),
                ("cost.output", cost.output, &mut merged.output),
                ("cost.cache_read", cost.cache_read, &mut merged.cache_read),
                (
                    "cost.cache_write",
                    cost.cache_write,
                    &mut merged.cache_write,
                ),
            ] {
                if value.is_some_and(|n| n.is_finite() && n >= 0.0) && accepts(key, target) {
                    *slot = value;
                    target.field_sources.insert(key.into(), source);
                    changed = true;
                }
            }
            if merged != Cost::default() {
                target.cost = Some(merged);
            }
        }
    }
    if changed {
        if rank(source) >= rank(target.source) {
            target.source = source;
        }
        if let Some(at) = incoming.fetched_at {
            target.fetched_at = Some(at);
        }
    }
}

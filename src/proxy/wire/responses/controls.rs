//! CLI controls, including delivery metadata that cannot affect generated content.
use super::Result;
use serde_json::{json, Map, Value};

pub(super) fn controls(native: &Value, controls: &mut Map<String, Value>) -> Result<()> {
    if let Some(metadata) = native.get("client_metadata") {
        if !metadata
            .as_object()
            .is_some_and(|object| object.values().all(Value::is_string))
        {
            return Err("client_metadata must contain strings".into());
        }
    }
    if let Some(key) = native.get("prompt_cache_key") {
        if !key.is_string() {
            return Err("prompt_cache_key must be a string".into());
        }
        controls.insert("prompt_cache_key".into(), key.clone());
    }
    if let Some(parallel) = native.get("parallel_tool_calls") {
        if !parallel.is_boolean() {
            return Err("parallel_tool_calls must be a boolean".into());
        }
        controls.insert("parallel_tool_calls".into(), parallel.clone());
    }
    if let Some(tier) = native.get("service_tier") {
        if !matches!(
            tier.as_str(),
            Some("auto" | "default" | "flex" | "priority")
        ) {
            return Err("invalid service_tier".into());
        }
        controls.insert("service_tier".into(), tier.clone());
    }
    if let Some(options) = native.get("stream_options") {
        if !options.as_object().is_some_and(|options| {
            options.iter().all(|(key, value)| {
                key == "reasoning_summary_delivery" && value == "sequential_cutoff"
            })
        }) {
            return Err("unsupported stream_options".into());
        }
    }
    if let Some(programs) = native
        .get("access_programs")
        .filter(|value| !value.is_null())
    {
        if !programs.as_object().is_some_and(|object| {
            object.len() == 1
                && matches!(
                    object.get("cyber").and_then(Value::as_str),
                    Some("standard" | "daybreak_blue" | "daybreak_red")
                )
        }) {
            return Err("invalid access_programs".into());
        }
    }
    let mut native_controls = Map::new();
    for key in [
        "reasoning",
        "text",
        "service_tier",
        "parallel_tool_calls",
        "access_programs",
    ] {
        if let Some(value) = native.get(key).filter(|value| !value.is_null()) {
            native_controls.insert(key.into(), value.clone());
        }
    }
    controls.insert("x-responses-controls".into(), json!(native_controls));
    Ok(())
}

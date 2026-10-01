//! Stateless Responses requests over the shared completion handler.
mod config;
mod controls;
mod events;
mod input;
mod replay;
mod response;
mod stream;
mod tools;
pub(super) use replay::{output_options, replay_metadata, restore, retain};
pub(crate) use stream::stream_identity;
pub(super) use stream::stream_response;

use super::{array, Result};
use serde_json::{json, Map, Value};

pub(super) use response::{response, OutputItem, Response};

/// Accepts the stateless Responses subset and refuses state and unsupported fields.
pub(super) fn request(native: &Value) -> Result<Value> {
    stateless_fields(native)?;
    let messages = input::messages(native)?;
    let mut controls = Map::new();
    controls::controls(native, &mut controls)?;
    config::generation_controls(native, &mut controls)?;
    config::reasoning(native, &mut controls)?;
    config::text_format(native, &mut controls)?;
    tools::tools(native, &mut controls)?;
    config::tool_choice(native, &mut controls)?;
    let mut chat = Map::from_iter([
        ("model".into(), native["model"].clone()),
        ("messages".into(), json!(messages)),
        (
            "stream".into(),
            json!(native["stream"].as_bool().unwrap_or(false)),
        ),
    ]);
    chat.extend(controls);
    Ok(Value::Object(chat))
}

/// Accepts known stateless fields and refuses stored history and unknown fields.
fn stateless_fields(native: &Value) -> Result<()> {
    let object = native.as_object().ok_or("request must be an object")?;
    for field in ["previous_response_id", "conversation"] {
        if object.contains_key(field) {
            return Err(format!(
                "{field} is unsupported: this endpoint is stateless"
            ));
        }
    }
    if object.get("store").is_some_and(|value| value != false) {
        return Err("store must be false: this endpoint is stateless".into());
    }
    if object
        .get("stream")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("stream must be a boolean".into());
    }
    if let Some(include) = object.get("include") {
        for item in array(include, "include")? {
            if item != "reasoning.encrypted_content" {
                return Err(format!("unsupported include: {item}"));
            }
        }
    }
    for field in object.keys() {
        if !matches!(
            field.as_str(),
            "model"
                | "input"
                | "instructions"
                | "tools"
                | "tool_choice"
                | "max_output_tokens"
                | "temperature"
                | "top_p"
                | "reasoning"
                | "text"
                | "store"
                | "stream"
                | "include"
                | "client_metadata"
                | "prompt_cache_key"
                | "parallel_tool_calls"
                | "service_tier"
                | "stream_options"
                | "access_programs"
        ) {
            return Err(format!("unsupported Responses request field: {field}"));
        }
    }
    Ok(())
}

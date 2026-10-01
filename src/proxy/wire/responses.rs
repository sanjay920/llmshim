//! Stateless Responses requests over the shared completion handler.
mod config;
mod input;
mod response;

use super::{array, Result};
use serde_json::{json, Map, Value};

pub(super) use response::response;

/// Accepts the non-streaming Responses subset and refuses state and unsupported fields.
pub(super) fn request(native: &Value) -> Result<Value> {
    stateless_fields(native)?;
    let messages = input::messages(native)?;
    let mut controls = Map::new();
    config::generation_controls(native, &mut controls)?;
    config::reasoning(native, &mut controls)?;
    config::text_format(native, &mut controls)?;
    config::tools(native, &mut controls)?;
    config::tool_choice(native, &mut controls)?;
    let mut chat = Map::from_iter([
        ("model".into(), native["model"].clone()),
        ("messages".into(), json!(messages)),
        ("stream".into(), json!(false)),
    ]);
    chat.extend(controls);
    Ok(Value::Object(chat))
}

/// Accepts known stateless fields and refuses stored history, streaming, and unknown fields.
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
    if object.get("stream").is_some_and(|value| value != false) {
        return Err("stream must be false: Responses streaming is not supported yet".into());
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
        ) {
            return Err(format!("unsupported Responses request field: {field}"));
        }
    }
    Ok(())
}

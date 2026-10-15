//! Vertex prompt blocking has no candidate; project it onto the shared terminal convention.
use serde_json::{json, Value};
pub(super) fn prepare(mut native: Value) -> Value {
    if native
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
        .is_some_and(|reason| reason != "BLOCK_REASON_UNSPECIFIED")
        && native["candidates"].as_array().is_none_or(Vec::is_empty)
    {
        native["candidates"] = json!([{"content":{"role":"model","parts":[]},
            "finishReason":"SAFETY"}]);
    }
    native
}
pub(super) fn project_usage(native: &Value, result: &mut Value) {
    let usage = native.get("usageMetadata").or_else(|| native.get("usage"));
    if result["usage"].is_object() {
        if let Some(usage) = usage {
            result["usage"]["vertex"] = usage.clone();
        }
    }
}

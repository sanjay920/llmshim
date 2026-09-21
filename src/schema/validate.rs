//! Instance validation never retrieves a network or filesystem resource.
use crate::error::{Result, ShimError};
use serde_json::Value;

struct NoExternal;
impl jsonschema::Retrieve for NoExternal {
    fn retrieve(
        &self,
        _uri: &jsonschema::Uri<String>,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external schema retrieval is disabled".into())
    }
}

pub fn compile(schema: &Value) -> Result<jsonschema::Validator> {
    let mut pending = vec![(schema, 0usize)];
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    bytes = bytes.saturating_add(key.len());
                    pending.push((value, depth + 1));
                }
            }
            Value::Array(values) => pending.extend(values.iter().map(|value| (value, depth + 1))),
            Value::String(text) => bytes = bytes.saturating_add(text.len()),
            _ => {}
        }
        if depth > 96 || nodes > 32768 || bytes > 8 * 1024 * 1024 {
            return Err(ShimError::ProviderError {
                status: 400,
                body: "JSON schema exceeds validation limits".into(),
                retry_after: None,
            });
        }
    }
    jsonschema::options()
        .with_retriever(NoExternal)
        .should_validate_formats(true)
        .build(schema)
        .map_err(|_| ShimError::ProviderError {
            status: 400,
            body: "invalid or unresolved JSON schema".into(),
            retry_after: None,
        })
}

pub fn errors(validator: &jsonschema::Validator, value: &Value) -> Vec<String> {
    validator
        .iter_errors(value)
        .take(8)
        .map(|error| {
            format!(
                "{}: failed schema constraint {}",
                error.instance_path(),
                error.schema_path()
            )
        })
        .collect()
}

/// Strict transports represent absent optional values as null. Restore omission
/// only where the original property schema forbids null; explicit nulls survive.
pub fn restore_optional_omissions(schema: &Value, value: &mut Value) {
    if let (Some(properties), Some(object)) =
        (schema["properties"].as_object(), value.as_object_mut())
    {
        for (name, property) in properties {
            let required = schema["required"]
                .as_array()
                .is_some_and(|a| a.iter().any(|n| n == name));
            let omit = !required
                && object.get(name) == Some(&Value::Null)
                && compile(property).is_ok_and(|validator| !validator.is_valid(&Value::Null));
            if omit {
                object.remove(name);
            } else if let Some(value) = object.get_mut(name) {
                restore_optional_omissions(property, value);
            }
        }
    } else if let Some(items) = value.as_array_mut() {
        if let Some(schema) = schema.get("items").filter(|s| s.is_object()) {
            for value in items {
                restore_optional_omissions(schema, value);
            }
        }
    }
}

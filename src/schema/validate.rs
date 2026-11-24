//! Instance validation never retrieves a network or filesystem resource.
use crate::error::{Result, ShimError};
use serde_json::Value;
use std::collections::BTreeMap;

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

#[derive(Debug, Default)]
pub(crate) enum OptionalOmissions {
    Object(BTreeMap<String, OptionalProperty>),
    Array(Box<OptionalOmissions>),
    #[default]
    None,
}

#[derive(Debug)]
pub(crate) struct OptionalProperty {
    omit_null: bool,
    nested: OptionalOmissions,
}

impl OptionalOmissions {
    pub(crate) fn compile(schema: &Value, budget: &mut super::RequestBudget) -> Result<Self> {
        if let Some(properties) = schema["properties"].as_object() {
            let mut planned_properties = BTreeMap::new();
            for (name, property_schema) in properties {
                let required = schema["required"]
                    .as_array()
                    .is_some_and(|names| names.iter().any(|required_name| required_name == name));
                let omit_null = if required {
                    false
                } else {
                    budget.reserve_validator(property_schema)?;
                    compile(property_schema)
                        .is_ok_and(|validator| !validator.is_valid(&Value::Null))
                };
                planned_properties.insert(
                    name.clone(),
                    OptionalProperty {
                        omit_null,
                        nested: Self::compile(property_schema, budget)?,
                    },
                );
            }
            Ok(Self::Object(planned_properties))
        } else if let Some(item_schema) = schema.get("items").filter(|value| value.is_object()) {
            Ok(Self::Array(Box::new(Self::compile(item_schema, budget)?)))
        } else {
            Ok(Self::None)
        }
    }

    pub(crate) fn restore(&self, value: &mut Value) {
        match (self, value) {
            (Self::Object(properties), Value::Object(object)) => {
                for (name, property) in properties {
                    if property.omit_null && object.get(name) == Some(&Value::Null) {
                        object.remove(name);
                    } else if let Some(value) = object.get_mut(name) {
                        property.nested.restore(value);
                    }
                }
            }
            (Self::Array(item_plan), Value::Array(items)) => {
                for item in items {
                    item_plan.restore(item);
                }
            }
            _ => {}
        }
    }
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
                .is_some_and(|names| names.iter().any(|required_name| required_name == name));
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
        if let Some(item_schema) = schema.get("items").filter(|value| value.is_object()) {
            for value in items {
                restore_optional_omissions(item_schema, value);
            }
        }
    }
}

//! Responses generation controls and function tool declarations.
use super::{array, Result};
use serde_json::{json, Map, Value};

/// Accepts positive output limits, temperature 0..=2, and top_p 0..=1; refuses other values.
pub(super) fn generation_controls(native: &Value, controls: &mut Map<String, Value>) -> Result<()> {
    if let Some(value) = native.get("max_output_tokens") {
        if !value.as_u64().is_some_and(|tokens| tokens > 0) {
            return Err("invalid max_output_tokens".into());
        }
        controls.insert("max_tokens".into(), value.clone());
    }
    if let Some(value) = native.get("temperature") {
        if !value
            .as_f64()
            .is_some_and(|number| (0.0..=2.0).contains(&number))
        {
            return Err("invalid temperature".into());
        }
        controls.insert("temperature".into(), value.clone());
    }
    if let Some(value) = native.get("top_p") {
        if !value
            .as_f64()
            .is_some_and(|number| (0.0..=1.0).contains(&number))
        {
            return Err("invalid top_p".into());
        }
        controls.insert("top_p".into(), value.clone());
    }
    Ok(())
}

/// Accepts reasoning.effort and refuses unknown reasoning fields or effort values.
pub(super) fn reasoning(native: &Value, controls: &mut Map<String, Value>) -> Result<()> {
    let Some(reasoning) = native.get("reasoning") else {
        return Ok(());
    };
    let reasoning = reasoning.as_object().ok_or("reasoning must be an object")?;
    for key in reasoning.keys() {
        if key != "effort" {
            return Err(format!("unsupported reasoning field: {key}"));
        }
    }
    if let Some(effort) = reasoning.get("effort") {
        if !matches!(
            effort.as_str(),
            Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max")
        ) {
            return Err("invalid reasoning.effort".into());
        }
        controls.insert("reasoning_effort".into(), effort.clone());
    }
    Ok(())
}

/// Accepts text, JSON object, or named JSON Schema formats and refuses unsupported text fields.
pub(super) fn text_format(native: &Value, controls: &mut Map<String, Value>) -> Result<()> {
    let Some(text) = native.get("text") else {
        return Ok(());
    };
    let text = text.as_object().ok_or("text must be an object")?;
    if text.keys().any(|key| key != "format") {
        return Err("unsupported text field".into());
    }
    let Some(format) = text.get("format") else {
        return Ok(());
    };
    let translated = match format["type"].as_str() {
        Some("text") => return Ok(()),
        Some("json_object") => json!({"type": "json_object"}),
        Some("json_schema") => {
            if !format["schema"].is_object() || !format["name"].is_string() {
                return Err("text.format requires schema and name".into());
            }
            let fields = format
                .as_object()
                .ok_or("text.format requires schema and name")?;
            let schema: Map<String, Value> = fields
                .iter()
                .filter(|(key, _)| key.as_str() != "type")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            json!({"type": "json_schema", "json_schema": schema})
        }
        _ => return Err("unsupported text.format type".into()),
    };
    controls.insert("response_format".into(), translated);
    Ok(())
}

/// Accepts function tools with name/parameters and refuses hosted tools by type.
pub(super) fn tools(native: &Value, controls: &mut Map<String, Value>) -> Result<()> {
    let Some(tools) = native.get("tools") else {
        return Ok(());
    };
    let mut translated = Vec::new();
    for tool in array(tools, "tools")? {
        if tool["type"] != "function" {
            return Err(format!("unsupported hosted tool: {}", tool["type"]));
        }
        if !tool["name"].as_str().is_some_and(|name| !name.is_empty())
            || !tool["parameters"].is_object()
        {
            return Err("function tool requires name and parameters".into());
        }
        let fields = tool
            .as_object()
            .ok_or("function tool requires name and parameters")?;
        let function: Map<String, Value> = fields
            .iter()
            .filter(|(key, _)| key.as_str() != "type")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        translated.push(json!({"type": "function", "function": function}));
    }
    controls.insert("tools".into(), json!(translated));
    Ok(())
}

/// Accepts auto/none/required or a named function choice and refuses other choices.
pub(super) fn tool_choice(native: &Value, controls: &mut Map<String, Value>) -> Result<()> {
    let Some(choice) = native.get("tool_choice") else {
        return Ok(());
    };
    let translated = match choice.as_str() {
        Some("auto" | "none" | "required") => choice.clone(),
        _ if choice["type"] == "function"
            && choice["name"].as_str().is_some_and(|name| !name.is_empty()) =>
        {
            json!({"type": "function", "function": {"name": choice["name"]}})
        }
        _ => return Err("unsupported tool_choice".into()),
    };
    controls.insert("tool_choice".into(), translated);
    Ok(())
}

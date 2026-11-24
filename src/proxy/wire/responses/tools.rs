//! Native declarations and their reversible function transport equivalents.
use super::{array, Result};
use crate::responses_tools::{declarations, name};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

pub(super) fn tools(native: &Value, controls: &mut Map<String, Value>) -> Result<()> {
    let mut native_tools = Vec::new();
    if let Some(tools) = native.get("tools") {
        native_tools.extend(array(tools, "tools")?.iter().cloned());
    }
    for item in native["input"].as_array().into_iter().flatten() {
        if item["type"] == "additional_tools" {
            if item["role"] != "developer" {
                return Err("additional_tools requires developer role".into());
            }
            native_tools.extend(
                array(&item["tools"], "additional_tools.tools")?
                    .iter()
                    .cloned(),
            );
        }
    }
    let initial_tools = native_tools.clone();
    for item in native["input"].as_array().into_iter().flatten() {
        if item["type"] == "tool_search_output" {
            for tool in array(&item["tools"], "tool_search_output.tools")? {
                if !native_tools.contains(tool) {
                    native_tools.push(tool.clone());
                }
            }
        }
    }
    for tool in &native_tools {
        if tool["type"] == "namespace" {
            if !tool["name"].as_str().is_some_and(|name| !name.is_empty()) {
                return Err("namespace requires name".into());
            }
            array(&tool["tools"], "namespace.tools")?;
        }
    }
    let mut translated = Vec::new();
    let mut names = BTreeSet::new();
    for tool in declarations(&json!(native_tools)) {
        if matches!(tool["type"].as_str(), Some("web_search" | "tool_search")) {
            if tool.get("namespace").is_some() {
                return Err("hosted tools cannot be namespaced".into());
            }
            if tool["type"] == "tool_search"
                && (!matches!(tool["execution"].as_str(), Some("client" | "server"))
                    || !tool["parameters"].is_object()
                    || !tool["description"].is_string())
            {
                return Err("tool_search requires execution, description and parameters".into());
            }
            continue;
        }
        let tool_name = tool["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or("function tool requires name and parameters")?;
        let wire_name = name(tool_name, tool["namespace"].as_str());
        if !names.insert(wire_name.clone()) {
            return Err("duplicate tool name".into());
        }
        let mut function = match tool["type"].as_str() {
            Some("function") if tool["parameters"].is_object() => {
                let mut function = tool.as_object().cloned().ok_or("malformed function tool")?;
                for field in ["type", "namespace", "defer_loading"] {
                    function.remove(field);
                }
                Value::Object(function)
            }
            Some("custom") => {
                let format = &tool["format"];
                if format["type"] != "text"
                    && !(format["type"] == "grammar"
                        && matches!(format["syntax"].as_str(), Some("lark" | "regex"))
                        && format["definition"].is_string())
                {
                    return Err("custom tool requires text or grammar format".into());
                }
                let description = tool["description"].as_str().unwrap_or("");
                let description = format!(
                    "{description}\nPass the exact tool input in the input string. Format: {format}"
                );
                json!({
                    "description": description,
                    "parameters": {
                        "type": "object",
                        "properties": {"input": {"type": "string"}},
                        "required": ["input"],
                        "additionalProperties": false,
                    },
                    "strict": true,
                })
            }
            Some("function") => return Err("function tool requires name and parameters".into()),
            _ => return Err(format!("unsupported hosted tool: {}", tool["type"])),
        };
        if let Some(namespace) = tool["namespace"].as_str() {
            let namespace_description = native_tools
                .iter()
                .find(|tool| tool["type"] == "namespace" && tool["name"] == namespace)
                .and_then(|tool| tool["description"].as_str())
                .unwrap_or("");
            let description = function["description"].as_str().unwrap_or("");
            function["description"] = json!(format!(
                "{namespace}.{tool_name}: {namespace_description}\n{description}"
            ));
        }
        function["name"] = json!(wire_name);
        translated.push(json!({"type": "function", "function": function}));
    }
    if !native_tools.is_empty() || native.get("tools").is_some() {
        controls.insert("tools".into(), json!(translated));
        controls.insert("x-responses-tools".into(), json!(initial_tools));
        controls.insert("x-responses-loaded-tools".into(), json!(native_tools));
    }
    Ok(())
}

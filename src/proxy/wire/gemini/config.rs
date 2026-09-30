//! Gemini's request fields beyond `contents`, as canonical config.
//!
//! A field this facade cannot express is refused by name. Forwarding it would
//! produce a body Google rejects or, worse, silently ignores, and a caller
//! cannot tell either from a successful request.

use super::array;
use super::Result;
use serde_json::{json, Map, Value};

pub(super) fn canonical_config(native: &Value) -> Result<Map<String, Value>> {
    let mut config = Map::new();
    for (key, value) in native.as_object().ok_or("request must be an object")? {
        match key.as_str() {
            "model" | "contents" | "systemInstruction" | "stream" | "fallback" => {}
            "tools" => {
                config.insert("tools".into(), json!(function_tools(value)?));
            }
            "toolConfig" => {
                if let Some(choice) = function_choice(value)? {
                    config.insert("tool_choice".into(), choice);
                }
            }
            "generationConfig" => generation_config(value, &mut config)?,
            // Verbatim to Google through the adapter's native namespace; the
            // adapter writes these at the top level of the upstream body.
            "safetySettings" | "cachedContent" => {
                config
                    .entry("x-gemini".to_owned())
                    .or_insert_with(|| json!({}))[key] = value.clone();
            }
            other => return Err(format!("unsupported Gemini request field: {other}")),
        }
    }
    Ok(config)
}

fn generation_config(value: &Value, config: &mut Map<String, Value>) -> Result<()> {
    let generation = value
        .as_object()
        .ok_or("generationConfig must be an object")?;
    // A JSON schema implies a JSON body, so it decides alone: reading the two
    // fields in whatever order the caller happened to write them would let key
    // order choose between a schema and bare object mode.
    let schema = generation.get("responseSchema");
    let mut json_mime = false;
    for (key, value) in generation {
        match key.as_str() {
            "temperature" => insert(config, "temperature", value),
            "topP" => insert(config, "top_p", value),
            "topK" => insert(config, "top_k", value),
            "maxOutputTokens" => insert(config, "max_tokens", value),
            "stopSequences" => insert(config, "stop", value),
            "candidateCount" => {
                // One candidate per request, for the same reason the shared
                // reader refuses `n > 1`: the engine returns one message.
                if value.as_u64() != Some(1) {
                    return Err("Unsupported value: 'candidateCount' must be 1".into());
                }
            }
            "responseMimeType" => match value.as_str() {
                Some("application/json") => json_mime = true,
                Some("text/plain") => {}
                Some(other) => return Err(format!("unsupported responseMimeType: {other}")),
                None => return Err("responseMimeType must be a string".into()),
            },
            "responseSchema" => {}
            "thinkingConfig" => thinking_config(value, config)?,
            other => return Err(format!("unsupported generationConfig field: {other}")),
        }
    }
    // The adapter applies a schema through `response_format`, and the shim turns
    // that into `generationConfig.responseSchema`.
    if let Some(schema) = schema {
        insert(
            config,
            "response_format",
            &json!({"type":"json_schema","json_schema":{"schema":schema}}),
        );
    } else if json_mime {
        insert(config, "response_format", &json!({"type":"json_object"}));
    }
    Ok(())
}

fn insert(config: &mut Map<String, Value>, key: &str, value: &Value) {
    config.insert(key.to_owned(), value.clone());
}

/// Gemini 3.x selects depth with `thinkingLevel`, the same four-step scale the
/// unified `reasoning_effort` names, so the levels map one to one and each
/// adapter clamps them back for a model that cannot reach an end of the scale.
fn thinking_config(value: &Value, config: &mut Map<String, Value>) -> Result<()> {
    let thinking = value
        .as_object()
        .ok_or("thinkingConfig must be an object")?;
    for (key, value) in thinking {
        match key.as_str() {
            "thinkingLevel" => {
                let effort = match value.as_str() {
                    Some("minimal") => "none",
                    Some("low") => "low",
                    Some("medium") => "medium",
                    Some("high") => "high",
                    Some(other) => return Err(format!("unsupported thinkingLevel: {other}")),
                    None => return Err("thinkingLevel must be a string".into()),
                };
                config.insert("reasoning_effort".into(), json!(effort));
            }
            // Thoughts are exported as thought parts whenever the model returns
            // them, so a caller asking for none would receive them regardless.
            "includeThoughts" => {
                if value.as_bool() != Some(true) {
                    return Err("Unsupported value: 'includeThoughts' must be true".into());
                }
            }
            other => return Err(format!("unsupported thinkingConfig field: {other}")),
        }
    }
    Ok(())
}

/// `functionDeclarations` become canonical function tools; every other Google
/// tool family (hosted search, retrieval, …) is refused rather than turned into
/// a client-executed function.
fn function_tools(tools: &Value) -> Result<Vec<Value>> {
    let mut declared = Vec::new();
    for entry in array(tools, "tools")? {
        let declarations = entry
            .get("functionDeclarations")
            .ok_or("this endpoint supports custom function tools")?;
        for declaration in array(declarations, "functionDeclarations")? {
            declared.push(json!({
                "type": "function",
                "function": {
                    "name": declaration["name"],
                    "description": declaration["description"],
                    "parameters": declaration
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({"type":"object","properties":{}})),
                },
            }));
        }
    }
    Ok(declared)
}

fn function_choice(config: &Value) -> Result<Option<Value>> {
    let calling = config
        .get("functionCallingConfig")
        .ok_or("this endpoint supports function calling configuration only")?;
    if calling.get("allowedFunctionNames").is_some() {
        return Err("Unsupported value: 'allowedFunctionNames' cannot be expressed".into());
    }
    Ok(Some(match calling["mode"].as_str() {
        Some("AUTO") => json!("auto"),
        Some("ANY") => json!("required"),
        Some("NONE") => json!("none"),
        Some(other) => return Err(format!("unsupported function calling mode: {other}")),
        None => return Err("functionCallingConfig.mode is required".into()),
    }))
}

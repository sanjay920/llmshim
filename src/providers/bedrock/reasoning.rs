//! Use the shared Anthropic reasoning profile for Converse native controls.
use crate::{
    error::{provider_error, Result},
    providers::{anthropic, anthropic_constraints as constraints, anthropic_reasoning::Profile},
};
use serde_json::{json, Value};

pub(super) fn apply(model: &str, request: &Value, body: &mut Value) -> Result<()> {
    let model_id = anthropic_model(model);
    map_controls(model_id, request, body)?;
    let Some(model) = model_id else {
        return Ok(());
    };
    let mut controls = body
        .get("additionalModelRequestFields")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (native, common) in [("temperature", "temperature"), ("topP", "top_p")] {
        if let Some(value) = body["inferenceConfig"].get(native) {
            controls.insert(common.into(), value.clone());
        }
    }
    let last_role = body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["role"].as_str());
    let choice = &body["toolConfig"]["toolChoice"];
    controls.insert(
        "tool_choice".into(),
        if choice.get("any").is_some() {
            json!({"type":"any"})
        } else if choice.get("tool").is_some() {
            json!({"type":"tool"})
        } else {
            Value::Null
        },
    );
    constraints::apply(model, &mut controls, true, last_role)?;
    if let Some(inference) = body["inferenceConfig"].as_object_mut() {
        for (native, common) in [("temperature", "temperature"), ("topP", "top_p")] {
            if !controls.contains_key(common) {
                inference.remove(native);
            }
        }
    }
    if let Some(native) = body["additionalModelRequestFields"].as_object_mut() {
        for field in ["temperature", "top_p", "top_k"] {
            if !controls.contains_key(field) {
                native.remove(field);
            }
        }
    }
    Ok(())
}

fn anthropic_model(model: &str) -> Option<&str> {
    if model.starts_with("arn:") {
        return None;
    }
    let model = model.split_once("anthropic.")?.1;
    let model = model.split(':').next().unwrap_or(model);
    let model = model
        .rsplit_once("-v")
        .filter(|(_, v)| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        .map(|(model, _)| model)
        .unwrap_or(model);
    Some(
        model
            .rsplit_once('-')
            .filter(|(_, date)| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()))
            .map(|(model, _)| model)
            .unwrap_or(model),
    )
}

fn map_controls(model: Option<&str>, request: &Value, body: &mut Value) -> Result<()> {
    let controls = ["reasoning_effort", "reasoning_mode", "reasoning_summary"];
    if !controls
        .iter()
        .any(|field| request.get(field).is_some_and(|v| !v.is_null()))
    {
        return Ok(());
    }
    let profile = model
        .map(Profile::for_model)
        .unwrap_or(Profile::Unsupported);
    if !profile.supported() {
        let field = controls
            .iter()
            .find(|field| request.get(**field).is_some_and(|v| !v.is_null()))
            .copied()
            .unwrap_or("reasoning_effort");
        return Err(provider_error(
            400,
            &format!("Bedrock model does not support {field}"),
        ));
    }
    let Some(requested_effort) = request["reasoning_effort"].as_str() else {
        return Ok(());
    };
    let native = body
        .as_object_mut()
        .ok_or_else(|| provider_error(400, "invalid Converse body"))?
        .entry("additionalModelRequestFields")
        .or_insert_with(|| json!({}));
    let native = native
        .as_object_mut()
        .ok_or_else(|| provider_error(400, "additionalModelRequestFields must be an object"))?;
    // Explicit native thinking wins, matching the direct Anthropic adapter.
    if native.contains_key("thinking") {
        return Ok(());
    }
    let effort =
        anthropic::normalize_unified_effort(requested_effort, request["reasoning_mode"] == "pro");
    let effort = constraints::clamp_effort(model.unwrap_or_default(), effort);
    let display = match request["reasoning_summary"].as_str() {
        Some("none" | "omitted") => "omitted",
        _ => "summarized",
    };
    let thinking = if effort == "none" {
        json!({"type":"disabled"})
    } else if profile.adaptive() {
        native.insert(
            "output_config".into(),
            json!({"effort":profile.effort(effort)}),
        );
        json!({"type":"adaptive","display":display})
    } else {
        let max_tokens = request["max_completion_tokens"]
            .as_u64()
            .or(request["max_tokens"].as_u64())
            .unwrap_or(8192);
        let budget = profile.budget(effort, max_tokens)?;
        json!({"type":"enabled","budget_tokens":budget,"display":display})
    };
    native.insert("thinking".into(), thinking);
    if matches!(profile, Profile::Budget { .. }) && effort != "none" {
        if !body["inferenceConfig"].is_object() {
            body["inferenceConfig"] = json!({});
        }
        if body["inferenceConfig"].get("maxTokens").is_none() {
            body["inferenceConfig"]["maxTokens"] = json!(8192);
        }
    }
    Ok(())
}

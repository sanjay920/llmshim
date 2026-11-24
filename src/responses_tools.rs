//! Reversible identities for Responses tools on function-only transports.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub(crate) fn name(name: &str, namespace: Option<&str>) -> String {
    match namespace {
        None => name.to_owned(),
        Some(namespace) => {
            let digest = Sha256::digest(json!([namespace, name]).to_string().as_bytes());
            format!("rt_{digest:x}")[..63].to_owned()
        }
    }
}

pub(crate) fn declarations(tools: &Value) -> Vec<Value> {
    let mut declarations = Vec::new();
    for tool in tools.as_array().into_iter().flatten() {
        if tool["type"] == "namespace" {
            for child in tool["tools"].as_array().into_iter().flatten() {
                let mut child = child.clone();
                child["namespace"] = tool["name"].clone();
                declarations.push(child);
            }
        } else {
            declarations.push(tool.clone());
        }
    }
    declarations
}

pub(crate) fn supports_verbosity(target: &crate::reasoning::ReplayTarget) -> bool {
    target.wire == crate::reasoning::WireFormat::OpenAiResponses && target.provider != "xai"
}

fn tool_name(item: &Value) -> Result<&str, &'static str> {
    item["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .ok_or("tool name must be a nonempty string")
}

pub(crate) fn canonical_call(item: &Value) -> crate::error::Result<Value> {
    if item.get("namespace").is_some_and(|namespace| {
        !namespace
            .as_str()
            .is_some_and(|namespace| !namespace.is_empty())
    }) {
        return Err(crate::toolcall::upstream(
            "call namespace must be a nonempty string",
        ));
    }
    let tool_name = tool_name(item).map_err(crate::toolcall::upstream)?;
    let arguments = if item["type"] == "custom_tool_call" {
        let input = item["input"]
            .as_str()
            .ok_or_else(|| crate::toolcall::upstream("custom tool input must be text"))?;
        json!({"input": input}).to_string()
    } else {
        item["arguments"].as_str().unwrap_or("{}").to_owned()
    };
    Ok(json!({
        "id": item["call_id"],
        "type": "function",
        "function": {
            "name": name(tool_name, item["namespace"].as_str()),
            "arguments": arguments,
        }
    }))
}

pub(crate) fn native_request(body: &mut Value, request: &Value) -> crate::error::Result<()> {
    if let Some(controls) = request["x-responses-controls"].as_object() {
        let object = body
            .as_object_mut()
            .ok_or(crate::error::ShimError::MissingModel)?;
        object.extend(
            controls
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
    let tools = request
        .get("x-responses-tools")
        .cloned()
        .unwrap_or(Value::Null);
    for tool in tools.as_array().into_iter().flatten() {
        if tool["type"] == "namespace" {
            tool_name(tool).map_err(crate::toolcall::invalid)?;
        }
    }
    let declarations = declarations(&tools);
    let mut identities = Vec::new();
    for tool in &declarations {
        if matches!(tool["type"].as_str(), Some("web_search" | "tool_search")) {
            continue;
        }
        let tool_name = tool_name(tool).map_err(crate::toolcall::invalid)?;
        identities.push((name(tool_name, tool["namespace"].as_str()), tool));
    }
    if tools.is_array() {
        body["tools"] = tools;
    }
    if let Some(choice) = body.get("tool_choice") {
        if let Some(wire_name) = choice["name"].as_str() {
            if let Some((_, tool)) = identities.iter().find(|(name, _)| name == wire_name) {
                let mut choice = json!({"type": tool["type"], "name": tool["name"]});
                if let Some(namespace) = tool.get("namespace") {
                    choice["namespace"] = namespace.clone();
                }
                body["tool_choice"] = choice;
            }
        }
    }
    let mut custom_ids = std::collections::BTreeSet::new();
    if let Some(items) = body["input"].as_array_mut() {
        for item in items {
            if item["type"] == "custom_tool_call" {
                tool_name(item).map_err(crate::toolcall::invalid)?;
                custom_ids.insert(item["call_id"].to_string());
                continue;
            }
            if item["type"] == "function_call" {
                let wire_name = tool_name(item).map_err(crate::toolcall::invalid)?;
                if let Some((_, tool)) = identities.iter().find(|(name, _)| name == wire_name) {
                    item["name"] = tool["name"].clone();
                    if let Some(namespace) = tool.get("namespace") {
                        item["namespace"] = namespace.clone();
                    }
                    if tool["type"] == "custom" {
                        let arguments = item["arguments"].as_str().unwrap_or("");
                        let input = custom_input(arguments)
                            .map_err(|message| crate::toolcall::invalid(&message))?;
                        item["input"] = json!(input);
                        item["type"] = json!("custom_tool_call");
                        if let Some(object) = item.as_object_mut() {
                            object.remove("arguments");
                        }
                        custom_ids.insert(item["call_id"].clone().to_string());
                    }
                }
            } else if item["type"] == "function_call_output"
                && custom_ids.contains(&item["call_id"].to_string())
            {
                item["type"] = json!("custom_tool_call_output");
            }
        }
    }
    Ok(())
}

pub(crate) fn prepare_transport(
    request: &Value,
    target: &crate::reasoning::ReplayTarget,
) -> crate::error::Result<Value> {
    if (target.wire == crate::reasoning::WireFormat::GoogleGenerateContent
        || target.provider == "xai")
        && (request.get("parallel_tool_calls").is_some()
            || request["x-responses-controls"]
                .get("parallel_tool_calls")
                .is_some())
    {
        return Err(crate::toolcall::invalid(
            "parallel_tool_calls is unsupported by this provider",
        ));
    }
    if target.wire != crate::reasoning::WireFormat::OpenAiResponses {
        let controls = &request["x-responses-controls"];
        let native_only = request["messages"].as_array().is_some_and(|messages| {
            messages
                .iter()
                .any(|message| message.get("x-responses-item").is_some())
        }) || controls.get("access_programs").is_some()
            || controls["reasoning"]["effort"].is_u64()
            || matches!(
                controls["reasoning"]["summary"].as_str(),
                Some("concise" | "detailed")
            )
            || request["x-responses-tools"]
                .as_array()
                .is_some_and(|tools| {
                    tools.iter().any(|tool| {
                        matches!(tool["type"].as_str(), Some("web_search" | "tool_search"))
                    })
                });
        if native_only {
            return Err(crate::toolcall::invalid(
                "requested Responses controls or hosted tools require a native provider",
            ));
        }
    }
    let mut request = request.clone();
    if !supports_verbosity(target) {
        if let Some(text) = request
            .get_mut("x-responses-controls")
            .and_then(|controls| controls.get_mut("text"))
            .and_then(Value::as_object_mut)
        {
            text.remove("verbosity");
        }
    }
    if target.wire != crate::reasoning::WireFormat::OpenAiResponses
        && request["x-responses-controls"]["reasoning"]["context"] == "current_turn"
    {
        if let Some(messages) = request["messages"].as_array_mut() {
            let current = messages
                .iter()
                .rposition(|message| message["role"] == "user");
            for message in messages.iter_mut().take(current.unwrap_or(0)) {
                crate::reasoning::strip_fields(message);
            }
        }
    }
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return Ok(request);
    };
    if target.wire != crate::reasoning::WireFormat::OpenAiResponses {
        for message in messages.iter_mut() {
            if let Some(output) = message.get("x-responses-output") {
                if let Some(parts) = output.as_array() {
                    if parts.iter().any(|part| {
                        matches!(
                            part["type"].as_str(),
                            Some("input_audio" | "encrypted_content")
                        )
                    }) || (target.wire == crate::reasoning::WireFormat::GoogleGenerateContent
                        && parts.iter().any(|part| part["type"] != "input_text"))
                    {
                        return Err(crate::toolcall::invalid(
                            "tool output content requires a native Responses provider",
                        ));
                    }
                    if parts.iter().all(|part| part["type"] == "input_text") {
                        message["content"] = json!(parts
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n"));
                    }
                }
            }
            for call in message
                .get_mut("tool_calls")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                if let Some(object) = call.as_object_mut() {
                    object.remove("x-responses-call");
                }
            }
            if let Some(object) = message.as_object_mut() {
                object.remove("x-responses-output");
            }
        }
    }
    Ok(request)
}

pub(crate) fn auxiliary_output(native: &Value) -> Vec<Value> {
    let items = native["output"].as_array().cloned().unwrap_or_default();
    if items.iter().any(|item| {
        matches!(
            item["type"].as_str(),
            Some("tool_search_call" | "tool_search_output" | "web_search_call")
        )
    }) {
        items
    } else {
        Vec::new()
    }
}

pub(crate) fn custom_input(arguments: &str) -> Result<String, String> {
    let value = crate::json_bounds::parse_str(arguments, crate::json_bounds::Limits::SSE)
        .map_err(|_| "custom tool arguments must be valid JSON")?;
    if !value.as_object().is_some_and(|object| object.len() == 1) {
        return Err("custom tool arguments must contain only input".into());
    }
    value["input"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "custom tool input must be a string".into())
}

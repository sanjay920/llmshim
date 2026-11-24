//! Canonical history becomes Converse blocks after shared replay/tool validation.
use super::content::{blocks as content, tool_result as tool_content};
use super::{Bedrock, BedrockError};
use crate::{
    error::Result,
    provider::{Provider, ProviderRequest},
    reasoning::WireFormat,
};
use serde_json::{json, Value};

pub(super) fn invalid(message: &str) -> crate::error::ShimError {
    crate::error::provider_error(400, message)
}

pub(super) fn prepare(provider: &Bedrock, model: &str, request: &Value) -> Result<ProviderRequest> {
    if model.is_empty() {
        return Err(invalid("Bedrock model must not be empty"));
    }
    let mut budget = crate::schema::RequestBudget::new();
    let request = crate::schema::prepare_request(request, &mut budget)?;
    let request = crate::cache::prepare_request(&request, WireFormat::AnthropicMessages)?;
    let target = provider.replay_target(model);
    let mut request = request;
    // Shared replay filtering checks family/format, but signed text also needs an
    // account boundary here: a signature issued to another AWS identity cannot replay.
    if let Some(messages) = request["messages"].as_array_mut() {
        for message in messages {
            let belongs = |origin: &Value| {
                origin["provider"] == target.provider
                    && origin["account"].as_str() == target.account.as_deref()
            };
            if let Some(blocks) = message["reasoning"].as_array_mut() {
                blocks.retain(|block| belongs(&block["origin"]));
            }
            if message
                .get("reasoning_origin")
                .is_some_and(|origin| !belongs(origin))
            {
                if let Some(object) = message.as_object_mut() {
                    object.remove("reasoning_origin");
                }
            }
        }
    }
    let request = crate::reasoning::prepare_request(&request, &target)?;
    let request = crate::toolcall::prepare_request(&request, &target)?;
    for field in [
        "response_format",
        "logprobs",
        "top_logprobs",
        "presence_penalty",
        "frequency_penalty",
        "seed",
    ] {
        if request.get(field).is_some_and(|value| !value.is_null()) {
            return Err(invalid(&format!(
                "Bedrock does not support {field} on this transport"
            )));
        }
    }
    if request
        .get("n")
        .is_some_and(|value| !value.is_null() && value != 1)
    {
        return Err(invalid("Bedrock supports one response per request"));
    }
    if request
        .pointer("/x-cache/segments")
        .as_ref()
        .is_some_and(|value| {
            value
                .as_array()
                .is_some_and(|segments| !segments.is_empty())
        })
    {
        return Err(invalid(
            "Bedrock cache breakpoints require native cachePoint blocks",
        ));
    }
    let messages = request["messages"]
        .as_array()
        .ok_or_else(|| invalid("messages must be an array"))?;
    #[derive(serde::Serialize)]
    struct Turn<'a> {
        role: &'a str,
        content: Vec<Value>,
    }
    let mut turns: Vec<Turn<'_>> = Vec::new();
    let mut system = Vec::new();
    for message in messages {
        let role = message["role"]
            .as_str()
            .ok_or_else(|| invalid("message role is required"))?;
        if matches!(role, "system" | "developer") {
            if !turns.is_empty() {
                return Err(invalid("Bedrock system instructions must precede turns"));
            }
            system.extend(content(&message["content"])?);
            continue;
        }
        let (role, blocks) = if role == "tool" {
            (
                "user",
                vec![json!({"toolResult":{
                    "toolUseId":message["tool_call_id"],
                    "content":tool_content(&message["content"])?
                }})],
            )
        } else if matches!(role, "assistant" | "user") {
            let mut blocks = Vec::new();
            for reasoning in crate::reasoning::anthropic_blocks(message) {
                blocks.push(match reasoning["type"].as_str() {
                    Some("thinking") => json!({"reasoningContent":{"reasoningText":{
                        "text":reasoning["thinking"],"signature":reasoning["signature"]}}}),
                    Some("redacted_thinking") => {
                        json!({"reasoningContent":{"redactedContent":reasoning["data"]}})
                    }
                    _ => return Err(invalid("unsupported Bedrock reasoning block")),
                });
            }
            blocks.extend(content(&message["content"])?);
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                let input: Value = serde_json::from_str(
                    call["function"]["arguments"]
                        .as_str()
                        .ok_or_else(|| invalid("tool arguments must be JSON"))?,
                )?;
                blocks.push(json!({"toolUse":{"toolUseId":call["id"],
                    "name":call["function"]["name"],"input":input}}));
            }
            (role, blocks)
        } else {
            return Err(invalid("unsupported Bedrock message role"));
        };
        if let Some(previous) = turns.last_mut().filter(|previous| previous.role == role) {
            previous.content.extend(blocks);
        } else {
            turns.push(Turn {
                role,
                content: blocks,
            });
        }
    }
    let mut body = json!({"messages":&turns});
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    let mut inference = serde_json::Map::new();
    for (source, destination) in [
        ("max_tokens", "maxTokens"),
        ("max_completion_tokens", "maxTokens"),
        ("temperature", "temperature"),
        ("top_p", "topP"),
    ] {
        if let Some(value) = request.get(source).filter(|value| !value.is_null()) {
            inference.insert(destination.into(), value.clone());
        }
    }
    if let Some(stop) = request.get("stop").filter(|value| !value.is_null()) {
        inference.insert(
            "stopSequences".into(),
            if stop.is_string() {
                json!([stop])
            } else {
                stop.clone()
            },
        );
    }
    if !inference.is_empty() {
        body["inferenceConfig"] = Value::Object(inference);
    }
    let tools = request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|tool| {
            if tool["type"] != "function" {
                return Err(invalid("Bedrock requires function tools"));
            }
            let function = &tool["function"];
            let schema = function
                .get("parameters")
                .filter(|value| !value.is_null())
                .cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{}}));
            let mut spec = json!({"name":function["name"],"inputSchema":{"json":schema}});
            if let Some(description) = function.get("description") {
                spec["description"] = description.clone();
            }
            Ok(json!({"toolSpec":spec}))
        })
        .collect::<Result<Vec<_>>>()?;
    let choice = &request["tool_choice"];
    let has_tool_history = turns.iter().any(|turn| {
        turn.content
            .iter()
            .any(|block| block.get("toolUse").is_some() || block.get("toolResult").is_some())
    });
    if has_tool_history && (tools.is_empty() || choice == "none") {
        return Err(invalid(
            "Bedrock tool history requires enabled toolConfig.tools",
        ));
    }
    if !tools.is_empty() && choice != "none" {
        body["toolConfig"] = json!({"tools":tools});
        if !choice.is_null() {
            body["toolConfig"]["toolChoice"] = match choice.as_str() {
                Some("auto") => json!({"auto":{}}),
                Some("required") => json!({"any":{}}),
                _ if choice["type"] == "function" => {
                    json!({"tool":{"name":choice["function"]["name"]}})
                }
                _ => return Err(invalid("unsupported Bedrock tool choice")),
            };
        }
    }
    if let Some(extension) = request.get("x-bedrock") {
        let fields = extension
            .as_object()
            .ok_or_else(|| invalid("x-bedrock must be an object"))?;
        for (field, value) in fields {
            if !matches!(
                field.as_str(),
                "additionalModelRequestFields"
                    | "guardrailConfig"
                    | "performanceConfig"
                    | "requestMetadata"
                    | "serviceTier"
            ) {
                return Err(invalid("unsupported or protected x-bedrock field"));
            }
            body[field] = value.clone();
        }
    }
    super::reasoning::apply(model, &request, &mut body)?;
    let encoded: String = model
        .as_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
                (*byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    let operation = if request["stream"] == true {
        "converse-stream"
    } else {
        "converse"
    };
    let url = format!(
        "{}/model/{encoded}/{operation}",
        provider.endpoint()?.trim_end_matches('/')
    );
    if reqwest::Url::parse(&url).is_err() {
        return Err(BedrockError::protocol().into_shim(None));
    }
    Ok(ProviderRequest {
        url,
        headers: vec![("content-type".into(), "application/json".into())],
        body,
    })
}

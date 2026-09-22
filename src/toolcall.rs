//! Owned tool-call identities, persisted wire mappings, and conversation checks.
mod streaming;
pub use streaming::{ToolDelta, ToolStream, ToolUpdate};

use crate::{
    error::{Result, ShimError},
    reasoning::{ReplayTarget, WireFormat},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const PREFIX: &str = "call_ls_";
fn mint() -> String {
    format!("{PREFIX}{}", uuid::Uuid::new_v4().simple())
}
pub(crate) fn invalid(message: &str) -> ShimError {
    ShimError::ProviderError {
        status: 400,
        body: format!("invalid tool history: {message}"),
        retry_after: None,
    }
}
pub(crate) fn upstream(message: &str) -> ShimError {
    ShimError::ProviderError {
        status: 502,
        body: format!("invalid upstream tool call: {message}"),
        retry_after: None,
    }
}

/// A correlation id is distinct from a Responses output item's id. `None` is
/// meaningful for Gemini: do not add an id to a signed call that had none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireToolId {
    pub provider: String,
    pub wire: WireFormat,
    pub scope: String,
    pub part_id: String,
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_field: Option<String>,
}
impl WireToolId {
    fn key(&self) -> String {
        let correlation = match &self.id {
            Some(id) => json!(["id", id]),
            None => json!(["part", self.part_id]),
        };
        serde_json::to_string(&(&self.provider, self.wire, &self.scope, correlation))
            .expect("serializable wire identity")
    }
}

/// Bidirectional map, reconstructed from `tool_calls[].wire_ids` on replay.
#[derive(Debug, Default, Clone)]
pub struct ToolCallMap {
    by_internal: BTreeMap<String, Vec<WireToolId>>,
    by_wire: BTreeMap<String, String>,
}
impl ToolCallMap {
    pub fn register(&mut self, binding: WireToolId) -> Result<String> {
        if let Some(id) = self.by_wire.get(&binding.key()) {
            return Ok(id.clone());
        }
        let id = mint();
        self.insert(&id, vec![binding])?;
        Ok(id)
    }
    pub fn insert(&mut self, id: &str, bindings: Vec<WireToolId>) -> Result<()> {
        if !id.starts_with(PREFIX) || bindings.is_empty() {
            return Err(invalid("owned call is missing its wire mapping"));
        }
        for binding in &bindings {
            if binding.provider.is_empty() || binding.part_id.is_empty() || binding.scope.is_empty()
            {
                return Err(invalid("wire mapping is incomplete"));
            }
            if self
                .by_wire
                .get(&binding.key())
                .is_some_and(|old| old != id)
            {
                return Err(invalid("wire identity belongs to two calls"));
            }
        }
        let entries = self.by_internal.entry(id.into()).or_default();
        for binding in bindings {
            self.by_wire.insert(binding.key(), id.into());
            if !entries.contains(&binding) {
                entries.push(binding);
            }
        }
        Ok(())
    }
    pub fn internal_id(&self, wire: &WireToolId) -> Option<&str> {
        self.by_wire.get(&wire.key()).map(String::as_str)
    }
    pub fn wire_ids(&self, internal: &str) -> Option<&[WireToolId]> {
        self.by_internal.get(internal).map(Vec::as_slice)
    }
    pub fn for_target(&mut self, id: &str, target: &ReplayTarget) -> Result<WireToolId> {
        let entries = self
            .by_internal
            .get(id)
            .ok_or_else(|| invalid("unknown owned call id"))?;
        if let Some(binding) = entries
            .iter()
            .find(|b| b.provider == target.provider && b.wire == target.wire)
        {
            return Ok(binding.clone());
        }
        let binding = WireToolId {
            provider: target.provider.clone(),
            wire: target.wire,
            scope: format!("replay:{id}"),
            part_id: id.into(),
            id: Some(id.into()),
            item_id: None,
            signature_field: None,
        };
        self.insert(id, vec![binding.clone()])?;
        Ok(binding)
    }
}

pub(crate) fn binding(
    target: &ReplayTarget,
    scope: &str,
    part: usize,
    id: Option<&str>,
    item: Option<&str>,
) -> WireToolId {
    WireToolId {
        provider: target.provider.clone(),
        wire: target.wire,
        scope: scope.into(),
        part_id: part.to_string(),
        id: id.filter(|s| !s.is_empty()).map(str::to_owned),
        item_id: item.filter(|s| !s.is_empty()).map(str::to_owned),
        signature_field: None,
    }
}

/// Capture the original correlation ids before any provider adapter projection
/// can confuse a Responses `id` with `call_id` or synthesize a Gemini id.
pub fn capture_response(target: &ReplayTarget, native: &Value, response: &mut Value) -> Result<()> {
    // Upstreams may reuse response/call ids across completed turns. Only a
    // locally owned response scope can make their wire identities unambiguous.
    let scope = uuid::Uuid::new_v4().to_string();
    let Some(choices) = response.get_mut("choices").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for (choice_index, choice) in choices.iter_mut().enumerate() {
        let sources: Vec<WireToolId> = match target.wire {
            WireFormat::AnthropicMessages => native["content"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .filter(|(_, p)| p["type"] == "tool_use")
                .map(|(i, p)| binding(target, &scope, i, p["id"].as_str(), None))
                .collect(),
            WireFormat::OpenAiResponses => native["output"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .filter(|(_, p)| p["type"] == "function_call")
                .map(|(i, p)| binding(target, &scope, i, p["call_id"].as_str(), p["id"].as_str()))
                .collect(),
            WireFormat::OpenAiChat => native["choices"][choice_index]["message"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(i, p)| {
                    binding(
                        target,
                        &format!("{scope}/choice:{choice_index}"),
                        i,
                        p["id"].as_str(),
                        None,
                    )
                })
                .collect(),
            WireFormat::GoogleGenerateContent => native["candidates"][choice_index]["content"]
                ["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .filter(|(_, p)| p.get("functionCall").is_some())
                .map(|(i, p)| {
                    binding(
                        target,
                        &format!("{scope}/choice:{choice_index}"),
                        i,
                        p["functionCall"]["id"].as_str(),
                        None,
                    )
                })
                .collect(),
        };
        let Some(calls) = choice
            .pointer_mut("/message/tool_calls")
            .and_then(Value::as_array_mut)
        else {
            if !sources.is_empty() {
                return Err(upstream("call projection lost its wire identity"));
            }
            continue;
        };
        if calls.len() != sources.len() {
            return Err(upstream("call projection lost its wire identity"));
        }
        let mut map = ToolCallMap::default();
        let mut wire_ids = BTreeSet::new();
        for (call, mut binding) in calls.iter_mut().zip(sources) {
            if call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(upstream("call has no function name"));
            }
            if call.get("type").is_some_and(|t| t != "function") {
                return Err(upstream("unsupported call type"));
            }
            call["type"] = json!("function");
            if let Some(sig) = call
                .pointer("/extra_content/google/thought_signature")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                binding.signature_field = Some("extra_content.google.thought_signature".into());
                call["thought_signature"] = json!(crate::reasoning::ThoughtSignature {
                    data: sig,
                    origin: target.origin()
                });
            }
            remove_nested_signature(call);
            if target.wire != WireFormat::GoogleGenerateContent && binding.id.is_none() {
                return Err(upstream("missing correlation id"));
            }
            if let Some(id) = binding.id.as_ref() {
                if !wire_ids.insert(id.clone()) {
                    return Err(upstream("duplicate correlation id"));
                }
            }
            validate_arguments(
                call.pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                true,
            )?;
            let id = map.register(binding.clone())?;
            call["id"] = json!(id);
            call["wire_ids"] = json!([binding]);
        }
        if !calls.is_empty() && choice["finish_reason"] == "stop" {
            choice["finish_reason"] = json!("tool_calls");
        }
    }
    Ok(())
}

pub(crate) fn validate_arguments(arguments: &str, is_upstream: bool) -> Result<()> {
    let limits = if is_upstream {
        crate::json_bounds::Limits::SSE
    } else {
        crate::json_bounds::Limits::INBOUND
    };
    match crate::json_bounds::parse_str(arguments, limits) {
        Ok(_) => Ok(()),
        Err(crate::json_bounds::ParseError::Complexity) if is_upstream => {
            Err(upstream("tool arguments exceed JSON complexity limit"))
        }
        Err(crate::json_bounds::ParseError::Complexity) => {
            Err(invalid("tool arguments exceed JSON complexity limit"))
        }
        Err(crate::json_bounds::ParseError::Malformed(_)) if is_upstream => {
            Err(upstream("arguments are not complete JSON"))
        }
        Err(crate::json_bounds::ParseError::Malformed(_)) => {
            Err(invalid("arguments are not complete JSON"))
        }
    }
}

/// Check every call/result pairing. Pending calls at the end are invalid when
/// preparing a request that asks the model to produce the next assistant turn.
pub fn validate_history(messages: &[Value]) -> Result<()> {
    let mut pending = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for message in messages {
        if message
            .get("tool_calls")
            .is_some_and(|calls| !calls.is_null() && !calls.is_array())
        {
            return Err(invalid("tool_calls must be an array or null"));
        }
        if message["role"] == "assistant" {
            if !pending.is_empty() {
                return Err(invalid("an assistant turn follows unanswered tool calls"));
            }
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                let id = call["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("call has no id"))?;
                if pending.contains(id)
                    || (!seen.insert(id.to_owned())
                        && (id.starts_with(PREFIX) || call.get("wire_ids").is_some()))
                {
                    return Err(invalid("duplicate call id"));
                }
                if call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(invalid("call has no function name"));
                }
                validate_arguments(
                    call.pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    false,
                )?;
                pending.insert(id.to_owned());
            }
        } else if message["role"] == "tool" {
            let id = message["tool_call_id"]
                .as_str()
                .ok_or_else(|| invalid("result has no call id"))?;
            if !pending.remove(id) {
                return Err(invalid("tool result has no unanswered matching call"));
            }
        }
    }
    if !pending.is_empty() {
        return Err(invalid(
            "tool calls must be answered before requesting another assistant turn",
        ));
    }
    Ok(())
}

/// Resolve both sides to their target wire ids. The returned clone contains
/// only wire-ready identities; the caller's persisted normalized log is untouched.
pub(crate) fn prepare_request(request: &Value, target: &ReplayTarget) -> Result<Value> {
    let mut request = request.clone();
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return Ok(request);
    };
    validate_history(messages)?;
    let current_turn = messages.iter().rposition(|m| m["role"] == "user");
    let mut map = ToolCallMap::default();
    let mut calls_by_logical = BTreeMap::new();
    let scope = uuid::Uuid::new_v4().to_string();
    let mut call_order = 0usize;
    for (message_index, message) in messages.iter_mut().enumerate() {
        if message["role"] == "assistant" {
            let mut first = true;
            if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for (i, call) in calls.iter_mut().enumerate() {
                    let logical = call["id"].as_str().unwrap().to_owned();
                    let internal = if let Some(bindings) = call.get("wire_ids") {
                        let bindings: Vec<WireToolId> = serde_json::from_value(bindings.clone())
                            .map_err(|_| invalid("malformed wire mapping"))?;
                        map.insert(&logical, bindings)?;
                        logical.clone()
                    } else {
                        if logical.starts_with(PREFIX) {
                            return Err(invalid(
                                "owned call is missing wire_ids; preserve the complete tool call",
                            ));
                        }
                        map.register(binding(
                            target,
                            &format!("{scope}/{message_index}"),
                            i,
                            if target.wire == WireFormat::GoogleGenerateContent {
                                None
                            } else {
                                Some(&logical)
                            },
                            None,
                        ))?
                    };
                    let wire = map.for_target(&internal, target)?;
                    let name = call["function"]["name"].as_str().unwrap().to_owned();
                    // Gemini's first parallel call carries the required signature;
                    // the following calls must preserve their original absence.
                    if target.wire == WireFormat::GoogleGenerateContent
                        && first
                        && current_turn.is_none_or(|start| message_index > start)
                        && !target.model.starts_with("gemini-2")
                        && call["thought_signature"].as_str().is_none_or(str::is_empty)
                    {
                        return Err(invalid(
                            "first Gemini tool call is missing a compatible thought signature",
                        ));
                    }
                    first = false;
                    calls_by_logical
                        .insert(logical, (wire.clone(), name, internal.clone(), call_order));
                    call_order += 1;
                    call["id"] = json!(wire.id.as_deref().unwrap_or(&internal));
                    let obj = call.as_object_mut().unwrap();
                    obj.remove("wire_ids");
                    obj.remove("index");
                    if target.wire == WireFormat::OpenAiChat
                        && wire.signature_field.as_deref()
                            == Some("extra_content.google.thought_signature")
                    {
                        if let Some(sig) = obj.remove("thought_signature") {
                            let extra = obj.entry("extra_content").or_insert(json!({}));
                            if !extra.is_object() {
                                return Err(invalid("signature container must be an object"));
                            }
                            if extra.get("google").is_none() {
                                extra["google"] = json!({});
                            }
                            if !extra["google"].is_object() {
                                return Err(invalid("signature container must be an object"));
                            }
                            extra["google"]["thought_signature"] = sig;
                        }
                    }
                    if target.wire == WireFormat::OpenAiResponses {
                        if let Some(item) = wire.item_id {
                            obj.insert("_llmshim_item_id".into(), json!(item));
                        }
                    }
                    if target.wire == WireFormat::GoogleGenerateContent {
                        obj.insert("_llmshim_wire_id".into(), json!(wire.id));
                    }
                }
            }
        } else if message["role"] == "tool" {
            if target.wire != WireFormat::AnthropicMessages
                && message.as_object_mut().unwrap().remove("is_error") == Some(json!(true))
            {
                match &mut message["content"] {
                    Value::String(text) => text.insert_str(0, "Tool error: "),
                    Value::Array(blocks) => {
                        blocks.insert(0, json!({"type":"text","text":"Tool error:"}))
                    }
                    content => *content = json!(format!("Tool error: {content}")),
                }
            }
            let logical = message["tool_call_id"].as_str().unwrap().to_owned();
            let (wire, name, internal, order) = calls_by_logical
                .get(&logical)
                .ok_or_else(|| invalid("result mapping is missing"))?;
            if message["name"].as_str().is_some_and(|n| n != name) {
                return Err(invalid("result name does not match the call"));
            }
            message["tool_call_id"] = json!(wire.id.as_deref().unwrap_or(internal));
            if target.wire == WireFormat::GoogleGenerateContent {
                message["name"] = json!(name);
                message["_llmshim_call_order"] = json!(order);
                message["_llmshim_wire_id"] = json!(wire.id);
            }
        }
    }
    if target.wire == WireFormat::GoogleGenerateContent {
        let mut start = 0;
        while start < messages.len() {
            if messages[start]["role"] != "tool" {
                start += 1;
                continue;
            }
            let mut end = start + 1;
            while end < messages.len() && messages[end]["role"] == "tool" {
                end += 1;
            }
            messages[start..end].sort_by_key(|m| m["_llmshim_call_order"].as_u64().unwrap_or(0));
            for m in &mut messages[start..end] {
                m.as_object_mut().unwrap().remove("_llmshim_call_order");
            }
            start = end;
        }
    }
    Ok(request)
}

/// Central check after native overrides as well as ordinary translation.
pub(crate) fn validate_native(body: &Value, target: &ReplayTarget) -> Result<()> {
    let mut canonical = Vec::new();
    match target.wire {
        WireFormat::OpenAiChat => {
            return validate_history(
                body["messages"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            )
        }
        WireFormat::OpenAiResponses => {
            let mut assistant: Option<Value> = None;
            for item in body["input"].as_array().into_iter().flatten() {
                if item["role"] == "assistant"
                    || matches!(item["type"].as_str(), Some("reasoning" | "function_call"))
                {
                    let message = assistant
                        .get_or_insert_with(|| json!({"role":"assistant","tool_calls":[]}));
                    if item["type"] == "function_call" {
                        message["tool_calls"].as_array_mut().unwrap().push(json!({"id":item["call_id"],"function":{"name":item["name"],"arguments":item["arguments"]}}));
                    }
                } else {
                    if let Some(message) = assistant.take() {
                        canonical.push(message);
                    }
                    if item["type"] == "function_call_output" {
                        canonical.push(json!({"role":"tool","tool_call_id":item["call_id"]}));
                    } else {
                        canonical.push(json!({"role":"user"}));
                    }
                }
            }
            if let Some(message) = assistant {
                canonical.push(message);
            }
        }
        WireFormat::AnthropicMessages => {
            for message in body["messages"].as_array().into_iter().flatten() {
                let mut calls = Vec::new();
                let mut ordinary = false;
                let mut results = Vec::new();
                for block in message["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("thinking" | "redacted_thinking") => {
                            if ordinary {
                                return Err(invalid(
                                    "thinking blocks must precede text and tool calls",
                                ));
                            }
                        }
                        Some("tool_use") => {
                            if message["role"] != "assistant" {
                                return Err(invalid("tool_use must be assistant content"));
                            }
                            if !block["input"].is_object() {
                                return Err(invalid("Anthropic tool input must be an object"));
                            }
                            ordinary = true;
                            calls.push(json!({"id":block["id"],"function":{"name":block["name"],"arguments":block["input"].to_string()}}));
                        }
                        Some("tool_result") => {
                            if message["role"] != "user" {
                                return Err(invalid("tool_result must be user content"));
                            }
                            ordinary = true;
                            results
                                .push(json!({"role":"tool","tool_call_id":block["tool_use_id"]}));
                        }
                        _ => ordinary = true,
                    }
                }
                if message["role"] == "assistant" {
                    canonical.push(json!({"role":"assistant","tool_calls":calls}));
                }
                canonical.extend(results);
            }
        }
        WireFormat::GoogleGenerateContent => {
            let contents = body["contents"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let current = contents.iter().rposition(|m| {
                m["role"] == "user"
                    && m["parts"]
                        .as_array()
                        .is_some_and(|p| p.iter().any(|p| p.get("functionResponse").is_none()))
            });
            let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
            let mut by_id = BTreeMap::new();
            let mut ordinal = 0;
            for (mi, message) in contents.iter().enumerate() {
                let mut calls = Vec::new();
                let mut results = Vec::new();
                for part in message["parts"].as_array().into_iter().flatten() {
                    if let Some(call) = part.get("functionCall") {
                        if mi == 0 {
                            return Err(invalid("Gemini tool history must begin with a user turn"));
                        }
                        if message["role"] != "model" {
                            return Err(invalid("functionCall must be model content"));
                        }
                        if !target.model.starts_with("gemini-2")
                            && current.is_none_or(|start| mi > start)
                            && calls.is_empty()
                            && part["thoughtSignature"].as_str().is_none_or(str::is_empty)
                        {
                            return Err(invalid(
                                "first Gemini functionCall is missing its signature",
                            ));
                        }
                        let name = call["name"].as_str().unwrap_or("").to_owned();
                        let id = call["id"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("native_gemini_{ordinal}"));
                        ordinal += 1;
                        names.entry(name.clone()).or_default().push(id.clone());
                        by_id.insert(id.clone(), name);
                        calls.push(json!({"id":id,"function":{"name":call["name"],"arguments":call.get("args").cloned().unwrap_or(json!({})).to_string()}}));
                    }
                    if let Some(result) = part.get("functionResponse") {
                        if message["role"] != "user" {
                            return Err(invalid("functionResponse must be user content"));
                        }
                        let name = result["name"].as_str().unwrap_or("");
                        let id = if let Some(id) = result["id"].as_str() {
                            if by_id.get(id).is_none_or(|n| n != name) {
                                return Err(invalid("Gemini result does not match a call"));
                            }
                            if let Some(ids) = names.get_mut(name) {
                                ids.retain(|v| v != id);
                            }
                            id.to_owned()
                        } else {
                            let ids = names
                                .get_mut(name)
                                .filter(|v| !v.is_empty())
                                .ok_or_else(|| invalid("Gemini result does not match a call"))?;
                            ids.remove(0)
                        };
                        results.push(json!({"role":"tool","tool_call_id":id}));
                    }
                }
                if message["role"] == "model" {
                    canonical.push(json!({"role":"assistant","tool_calls":calls}));
                }
                canonical.extend(results);
            }
        }
    }
    validate_history(&canonical)
}

pub(crate) fn bind_response_context(response: &mut Value, target: &ReplayTarget) {
    let Some(choices) = response.get_mut("choices").and_then(Value::as_array_mut) else {
        return;
    };
    for choice in choices {
        for field in ["message", "delta"] {
            for call in choice
                .get_mut(field)
                .and_then(|m| m.get_mut("tool_calls"))
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                if let Some(bindings) = call.get_mut("wire_ids").and_then(Value::as_array_mut) {
                    for b in bindings {
                        b["provider"] = json!(target.provider);
                        b["wire"] = json!(target.wire);
                    }
                }
            }
        }
    }
}

/// Remove the known native nested signature, including empty carrier objects.
pub(crate) fn remove_nested_signature(call: &mut Value) -> Option<Value> {
    let removed = call
        .pointer_mut("/extra_content/google")
        .and_then(Value::as_object_mut)
        .and_then(|o| o.remove("thought_signature"));
    if call
        .pointer("/extra_content/google")
        .and_then(Value::as_object)
        .is_some_and(|o| o.is_empty())
    {
        call["extra_content"]
            .as_object_mut()
            .unwrap()
            .remove("google");
    }
    if call
        .get("extra_content")
        .and_then(Value::as_object)
        .is_some_and(|o| o.is_empty())
    {
        call.as_object_mut().unwrap().remove("extra_content");
    }
    removed
}

//! Capability-based request plans. Internal protocols are decoded and validated
//! before a response, stream event, or log entry can reach the caller.
use crate::{
    catalog::{ModelCapabilities, Support},
    error::{Result, ShimError},
    reasoning::{ReasoningBlock, ReplayTarget, WireFormat},
    schema::{self, OutputSchema, Target},
    toolcall::{ToolCallMap, WireToolId},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutput {
    #[default]
    Auto,
    Native,
    ForcedTool,
    Prompt,
}
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCalling {
    #[default]
    Auto,
    Native,
    Prompt,
}
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningCapture {
    #[default]
    Off,
    ForcedTool,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub structured_output: StructuredOutput,
    pub tool_calling: ToolCalling,
    pub reasoning_capture: ReasoningCapture,
}
fn invalid(message: &str) -> ShimError {
    ShimError::ProviderError {
        status: 400,
        body: message.into(),
        retry_after: None,
    }
}
pub(crate) fn failed() -> ShimError {
    ShimError::ProviderError {
        status: 502,
        body: "response did not satisfy the requested output contract".into(),
        retry_after: None,
    }
}
fn target(wire: WireFormat) -> Target {
    match wire {
        WireFormat::OpenAiChat => Target::OpenAiChat,
        WireFormat::OpenAiResponses => Target::OpenAiResponses,
        WireFormat::AnthropicMessages => Target::Anthropic,
        WireFormat::GoogleGenerateContent => Target::Google,
    }
}

struct Output {
    original: Value,
    resolved: Value,
    validator: jsonschema::Validator,
    wire: OutputSchema,
}
struct Tool {
    schema: Value,
    validator: jsonschema::Validator,
}

/// A plan holds one immutable capability decision for all attempts of a request.
/// Construction and rendering use local data only.
pub struct Plan {
    request: Value,
    structured: StructuredOutput,
    prompt_tools: bool,
    capture: bool,
    capture_native: bool,
    output: Option<Output>,
    tools: BTreeMap<String, Tool>,
    synthetic: String,
    strip_reasoning: bool,
}
impl Plan {
    pub fn new(provider: &str, model: &str, wire: WireFormat, request: &Value) -> Result<Self> {
        let handle = crate::catalog::global()
            .map_err(|_| invalid("model catalog configuration is invalid"))?;
        let snapshot = handle.snapshot();
        let caps = snapshot
            .resolve(&format!("{provider}/{model}"))
            .map(|m| m.capabilities)
            .unwrap_or_default();
        Self::with_capabilities(wire, request, caps)
    }
    /// Explicit capability input for embedded callers with their own snapshot.
    pub fn with_capabilities(
        wire: WireFormat,
        request: &Value,
        caps: ModelCapabilities,
    ) -> Result<Self> {
        if !request.is_object() {
            return Err(invalid("request must be an object"));
        }
        let mut request = request.clone();
        if request["response_format"]["type"] == "json_object" {
            request["response_format"] = json!({"type":"json_schema","json_schema":{"schema":{"type":"object"},"strict":false}});
        }
        let request = &request;
        let config: Config = request
            .get("x-shim")
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .map_err(|_| invalid("invalid x-shim configuration"))?
            .unwrap_or_default();
        let mut structured = match config.structured_output {
            StructuredOutput::Auto if caps.structured_output == Support::Supported => {
                StructuredOutput::Native
            }
            StructuredOutput::Auto
                if caps.tools == Support::Supported
                    && caps.forced_tool_choice != Support::Unsupported =>
            {
                StructuredOutput::ForcedTool
            }
            StructuredOutput::Auto => StructuredOutput::Prompt,
            mode => mode,
        };
        let output = if request["response_format"]["type"] == "json_schema" {
            let original = request
                .pointer("/response_format/json_schema/schema")
                .cloned()
                .ok_or_else(|| invalid("response_format.json_schema.schema is required"))?;
            let validator = schema::validate::compile(&original)?;
            let mut resolved = original.clone();
            schema::normalize_for(Target::Mcp, &mut resolved);
            let strict = request["response_format"]["json_schema"]["strict"]
                .as_bool()
                .unwrap_or(true);
            let output = schema::normalize_output_for(target(wire), &original, strict);
            Some(Output {
                original,
                resolved,
                validator,
                wire: output,
            })
        } else {
            None
        };
        if config.structured_output == StructuredOutput::Auto
            && output
                .as_ref()
                .is_some_and(|o| o.wire.normalization.used_fallback)
        {
            structured = StructuredOutput::Prompt;
        }
        let capture = config.reasoning_capture == ReasoningCapture::ForcedTool;
        let prompt_tools = config.tool_calling == ToolCalling::Prompt
            || (config.tool_calling == ToolCalling::Auto && caps.tools == Support::Unsupported);
        let capture_native = capture
            && caps.tools != Support::Unsupported
            && caps.forced_tool_choice != Support::Unsupported;
        let mut tools = BTreeMap::new();
        if capture || prompt_tools {
            if let Some(declarations) = request.get("tools") {
                for tool in declarations
                    .as_array()
                    .ok_or_else(|| invalid("tools must be an array"))?
                {
                    let function = tool.get("function").unwrap_or(tool);
                    let name = function["name"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| invalid("tool name is required"))?;
                    let raw = function
                        .get("parameters")
                        .or_else(|| function.get("inputSchema"))
                        .cloned()
                        .unwrap_or(json!({"type":"object"}));
                    let validator = schema::validate::compile(&raw)?;
                    if tools
                        .insert(
                            name.to_owned(),
                            Tool {
                                schema: raw,
                                validator,
                            },
                        )
                        .is_some()
                    {
                        return Err(invalid("tool names must be unique"));
                    }
                }
            }
        }
        let mut synthetic = if capture { "think" } else { "final_output" }.to_string();
        // Scan only declarations; user text never determines a protocol name.
        while request["tools"].as_array().is_some_and(|a| {
            a.iter()
                .any(|t| t["function"]["name"] == synthetic || t["name"] == synthetic)
        }) {
            synthetic.push('_');
        }
        Ok(Self {
            request: request.clone(),
            structured,
            prompt_tools,
            capture,
            capture_native,
            output,
            tools,
            synthetic,
            strip_reasoning: caps.reasoning == Support::Unsupported,
        })
    }
    pub fn buffered(&self) -> bool {
        self.output.is_some() || self.capture || (self.prompt_tools && !self.tools.is_empty())
    }
    pub(crate) fn can_repair(&self, response: &Value) -> bool {
        response["choices"].as_array().is_some_and(|choices| {
            choices
                .iter()
                .all(|c| matches!(c["finish_reason"].as_str(), Some("stop" | "tool_calls")))
        })
    }
    pub(crate) fn dispatch_error(&self, error: ShimError) -> ShimError {
        if !self.buffered() {
            return error;
        }
        match error {
            ShimError::ProviderError {
                status,
                retry_after,
                ..
            } => ShimError::ProviderError {
                status,
                body: "provider could not complete the requested output contract".into(),
                retry_after,
            },
            ShimError::Stream(_) => ShimError::Stream(
                "provider could not complete the requested output contract".into(),
            ),
            error => error,
        }
    }
    fn protocol(&self) -> bool {
        self.capture || (self.prompt_tools && !self.tools.is_empty())
    }
    fn synthetic_call(&self) -> bool {
        self.capture_native
            || (!self.capture
                && self.output.is_some()
                && self.structured == StructuredOutput::ForcedTool
                && !self.protocol())
    }

    pub fn render(&self) -> Result<Value> {
        let mut request = self.request.clone();
        let obj = request.as_object_mut().unwrap();
        obj.remove("x-shim");
        if self.strip_reasoning {
            strip_reasoning(obj);
        }
        if !self.buffered() {
            return Ok(request);
        }
        // Native extension overrides cannot replace a managed output contract or
        // inject internal calls outside the plan's validated protocol.
        for (key, value) in obj.iter_mut().filter(|(k, _)| k.starts_with("x-")) {
            if key == "x-cache" {
                continue;
            }
            if let Some(ext) = value.as_object_mut() {
                for field in [
                    "tools",
                    "tool_choice",
                    "toolConfig",
                    "response_format",
                    "responseSchema",
                    "responseMimeType",
                    "output_config",
                    "output_format",
                    "text",
                    "messages",
                    "input",
                    "contents",
                    "system",
                    "systemInstruction",
                    "instructions",
                ] {
                    ext.remove(field);
                }
                if let Some(config) = ext
                    .get_mut("generationConfig")
                    .and_then(Value::as_object_mut)
                {
                    config.remove("responseSchema");
                    config.remove("responseMimeType");
                }
            }
        }
        if self.output.is_some() && self.structured != StructuredOutput::Native || self.protocol() {
            obj.remove("response_format");
        }
        let mut instruction = String::new();
        if self.protocol() {
            obj.remove("tools");
            obj.remove("tool_choice");
            obj.remove("parallel_tool_calls");
            let declarations: Vec<Value> = self.tools.iter().map(|(name,tool)| json!({"name":name,"parameters":tool.schema,
                "description":self.request["tools"].as_array().and_then(|a|a.iter().find(|t|t["name"]==*name || t["function"]["name"]==*name)).map(|t|t.get("function").unwrap_or(t)["description"].clone())})).collect();
            instruction = format!("Return one JSON object with content (the final answer), tool_calls (an array of objects with name and arguments), and {}. Tools available: {}. Tool selection: {}. Do not execute tools. Use an empty tool_calls array when answering. Do not include markdown fences.",
                if self.capture {"reasoning (a brief explanation of the answer; do not provide private deliberation)"} else {"no other keys"},json!(declarations),self.request.get("tool_choice").unwrap_or(&json!("auto")));
            if let Some(output) = &self.output {
                instruction.push_str(&format!(
                    " When answering, content must be a JSON value satisfying this schema: {}.",
                    output.original
                ));
            }
            if self.request["parallel_tool_calls"] == false {
                instruction.push_str(" Return at most one tool call.");
            }
            textual_history(&mut request)?;
        } else if let Some(output) = &self.output {
            if self.structured == StructuredOutput::Prompt {
                instruction = format!(
                    "Return only a JSON value matching this schema, without markdown fences: {}",
                    output.original
                );
            }
        }
        if self.synthetic_call() {
            let parameters = if self.capture {
                json!({"type":"object","properties":{"text":{"type":"string","description":instruction}},"required":["text"],"additionalProperties":false})
            } else {
                self.output.as_ref().unwrap().wire.schema.clone()
            };
            request["tools"] = json!([{"type":"function","function":{"name":self.synthetic,"description":"Return the completed answer in the specified format.","parameters":parameters}}]);
            request["tool_choice"] = json!({"type":"function","function":{"name":self.synthetic}});
            request["parallel_tool_calls"] = json!(false);
            // Forced choices and provider-internal thinking are not composable on
            // some transports. The requested rationale has its own public field.
            strip_reasoning(request.as_object_mut().unwrap());
            instruction.clear();
        }
        if !instruction.is_empty() {
            prepend_instruction(&mut request, &instruction)?;
        }
        Ok(request)
    }

    /// A single corrective instruction is appended to a fresh rendering. Failed
    /// hidden calls are never added as unpaired native tool history.
    pub fn repair(&self, feedback: &[String]) -> Result<Value> {
        let mut request = self.render()?;
        let messages = request["messages"]
            .as_array_mut()
            .ok_or_else(|| invalid("messages must be an array"))?;
        messages.push(json!({"role":"user","content":format!("The previous attempt did not satisfy the output contract. Generate the complete answer again, correcting these validation errors: {}",feedback.join("; "))}));
        Ok(request)
    }

    /// Internal diagnostics go only to the one repair request. Public failures
    /// have a fixed message without provider output or protocol names.
    pub fn finish(
        &self,
        response: &mut Value,
        origin: &ReplayTarget,
    ) -> std::result::Result<(), Vec<String>> {
        if !self.buffered() {
            return Ok(());
        }
        let choices = response["choices"]
            .as_array_mut()
            .ok_or_else(|| vec!["missing choices".into()])?;
        if choices.is_empty() {
            return Err(vec!["missing choices".into()]);
        }
        for choice in choices {
            if choice["finish_reason"] == "content_filter"
                || choice["message"]
                    .get("refusal")
                    .is_some_and(|r| !r.is_null())
            {
                // Respect refusal without attempting to turn it into a schema result.
                if let Some(message) = choice["message"].as_object_mut() {
                    message.remove("tool_calls");
                    if self.protocol() || self.synthetic_call() {
                        message.remove("reasoning");
                    }
                }
                continue;
            }
            if !matches!(
                choice["finish_reason"].as_str(),
                Some("stop" | "tool_calls")
            ) {
                return Err(vec!["incomplete output".into()]);
            }
            let message = &mut choice["message"];
            let mut value = if self.synthetic_call() {
                let calls = message["tool_calls"]
                    .as_array()
                    .filter(|a| a.len() == 1)
                    .ok_or_else(|| vec!["return exactly one completed answer".into()])?;
                if calls[0]["function"]["name"] != self.synthetic {
                    return Err(vec!["return the requested answer format".into()]);
                }
                let parsed = parse_json(calls[0]["function"]["arguments"].as_str())?;
                if self.capture {
                    parse_json(parsed["text"].as_str())?
                } else {
                    self.output
                        .as_ref()
                        .unwrap()
                        .wire
                        .unwrap(&parsed)
                        .ok_or_else(|| vec!["missing response value".into()])?
                }
            } else if self.protocol() || self.output.is_some() {
                // Ordinary native user tool calls are an intermediate turn, not a
                // final structured answer. Do not manufacture a repair for them.
                if !self.protocol()
                    && message["tool_calls"]
                        .as_array()
                        .is_some_and(|a| !a.is_empty())
                {
                    continue;
                }
                let parsed = parse_json(message["content"].as_str())?;
                if !self.protocol() && self.structured == StructuredOutput::Native {
                    self.output
                        .as_ref()
                        .unwrap()
                        .wire
                        .unwrap(&parsed)
                        .ok_or_else(|| vec!["missing response value".into()])?
                } else {
                    parsed
                }
            } else {
                continue;
            };
            let mut user_calls = Vec::new();
            let mut rationale = None;
            if self.protocol() {
                let envelope = value
                    .as_object()
                    .ok_or_else(|| vec!["expected an answer object".into()])?;
                if envelope
                    .keys()
                    .any(|k| !matches!(k.as_str(), "content" | "tool_calls" | "reasoning"))
                {
                    return Err(vec!["unexpected answer field".into()]);
                }
                if self.capture {
                    rationale = Some(
                        value["reasoning"]
                            .as_str()
                            .filter(|s| !s.trim().is_empty())
                            .ok_or_else(|| vec!["missing brief explanation".into()])?
                            .to_owned(),
                    );
                }
                user_calls = self.parse_calls(&value, origin)?;
                value = value
                    .get("content")
                    .cloned()
                    .ok_or_else(|| vec!["missing content".into()])?;
            }
            if user_calls.is_empty() {
                if let Some(output) = &self.output {
                    if !self.protocol()
                        && self.structured != StructuredOutput::Prompt
                        && output.wire.normalization.strict
                    {
                        schema::validate::restore_optional_omissions(&output.resolved, &mut value);
                    }
                    let errors = schema::validate::errors(&output.validator, &value);
                    if !errors.is_empty() {
                        return Err(errors);
                    }
                    message["content"] = json!(value.to_string());
                } else if value.is_null() || value.is_string() {
                    message["content"] = value;
                } else {
                    return Err(vec!["content must be text or null".into()]);
                }
            } else {
                if !value.is_null() && !value.is_string() {
                    return Err(vec!["tool-call content must be text or null".into()]);
                }
                message["content"] = value;
            }
            let object = message
                .as_object_mut()
                .ok_or_else(|| vec!["missing message".into()])?;
            object.remove("tool_calls");
            if self.protocol() || self.synthetic_call() {
                // Native hidden-protocol deliberation is not public answer data,
                // and its signatures would bind an internal conversation prefix.
                object.remove("reasoning");
            }
            if let Some(text) = rationale {
                object.insert(
                    "reasoning".into(),
                    json!([ReasoningBlock::text(text, origin.origin())]),
                );
            }
            let has_calls = !user_calls.is_empty();
            if has_calls {
                object.insert("tool_calls".into(), json!(user_calls));
            }
            choice["finish_reason"] = json!(if has_calls { "tool_calls" } else { "stop" });
        }
        Ok(())
    }
    fn parse_calls(
        &self,
        envelope: &Value,
        target: &ReplayTarget,
    ) -> std::result::Result<Vec<Value>, Vec<String>> {
        let calls = envelope["tool_calls"]
            .as_array()
            .ok_or_else(|| vec!["tool_calls must be an array".into()])?;
        if calls.len() > 128 || (self.request["parallel_tool_calls"] == false && calls.len() > 1) {
            return Err(vec!["too many tool calls".into()]);
        }
        let choice = self
            .request
            .get("tool_choice")
            .cloned()
            .unwrap_or(json!("auto"));
        let pinned = choice
            .pointer("/function/name")
            .and_then(Value::as_str)
            .or_else(|| choice["name"].as_str());
        if choice == "none" && !calls.is_empty()
            || (choice == "required" || pinned.is_some()) && calls.is_empty()
        {
            return Err(vec!["tool selection does not match the request".into()]);
        }
        let mut map = ToolCallMap::default();
        let scope = uuid::Uuid::new_v4().to_string();
        let mut result = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            let name = call["name"]
                .as_str()
                .ok_or_else(|| vec!["tool name is required".into()])?;
            let tool = self
                .tools
                .get(name)
                .ok_or_else(|| vec!["unknown tool name".into()])?;
            if pinned.is_some_and(|p| p != name) {
                return Err(vec!["tool selection does not match the request".into()]);
            }
            let args = call
                .get("arguments")
                .ok_or_else(|| vec!["tool arguments are required".into()])?;
            let errors = schema::validate::errors(&tool.validator, args);
            if !errors.is_empty() {
                return Err(errors);
            }
            let id = map
                .register(WireToolId {
                    provider: target.provider.clone(),
                    wire: target.wire,
                    scope: scope.clone(),
                    part_id: index.to_string(),
                    id: None,
                    item_id: None,
                    signature_field: None,
                })
                .map_err(|_| vec!["invalid tool identity".into()])?;
            result.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()},"wire_ids":map.wire_ids(&id)}));
        }
        Ok(result)
    }
}
fn parse_json(text: Option<&str>) -> std::result::Result<Value, Vec<String>> {
    serde_json::from_str(text.ok_or_else(|| vec!["missing JSON answer".into()])?)
        .map_err(|_| vec!["answer is not valid JSON".into()])
}
fn prepend_instruction(request: &mut Value, instruction: &str) -> Result<()> {
    let messages = request["messages"]
        .as_array_mut()
        .ok_or_else(|| invalid("messages must be an array"))?;
    // Merge into an existing instruction so x-cache source indices stay stable.
    if let Some(message) = messages
        .iter_mut()
        .find(|m| matches!(m["role"].as_str(), Some("system" | "developer")))
    {
        match &mut message["content"] {
            Value::String(s) => {
                s.push_str("\n\n");
                s.push_str(instruction);
            }
            Value::Array(a) => a.push(json!({"type":"text","text":instruction})),
            _ => return Err(invalid("system content must be text or blocks")),
        }
    } else {
        messages.insert(0, json!({"role":"system","content":instruction}));
        if let Some(segments) = request
            .pointer_mut("/x-cache/segments")
            .and_then(Value::as_array_mut)
        {
            for segment in segments {
                if let Some(index) = segment["upto_message"].as_u64() {
                    segment["upto_message"] = json!(index.saturating_add(1));
                }
            }
        }
    }
    Ok(())
}
fn textual_history(request: &mut Value) -> Result<()> {
    let messages = request["messages"]
        .as_array_mut()
        .ok_or_else(|| invalid("messages must be an array"))?;
    crate::toolcall::validate_history(messages)?;
    for message in messages {
        if message["role"] == "tool" {
            let content =
                json!({"tool_call_id":message["tool_call_id"],"result":message["content"]})
                    .to_string();
            *message = json!({"role":"user","content":content});
        } else if let Some(calls) = message["tool_calls"].as_array() {
            let calls: Vec<Value>=calls.iter().map(|c|json!({"id":c["id"],"name":c["function"]["name"],"arguments":serde_json::from_str::<Value>(c["function"]["arguments"].as_str().unwrap_or("{}")).unwrap_or(Value::Null)})).collect();
            let content = json!({"content":message["content"],"tool_calls":calls}).to_string();
            *message = json!({"role":"assistant","content":content});
        }
    }
    Ok(())
}
fn strip_reasoning(obj: &mut serde_json::Map<String, Value>) {
    for key in [
        "reasoning_effort",
        "reasoning_mode",
        "reasoning_summary",
        "reasoning",
        "thinking",
    ] {
        obj.remove(key);
    }
    if let Some(output) = obj.get_mut("output_config").and_then(Value::as_object_mut) {
        output.remove("effort");
    }
    for (key, value) in obj.iter_mut().filter(|(k, _)| k.starts_with("x-")) {
        if key == "x-cache" {
            continue;
        }
        if let Some(ext) = value.as_object_mut() {
            if let Some(output) = ext.get_mut("output_config").and_then(Value::as_object_mut) {
                output.remove("effort");
            }
            for key in [
                "reasoning_effort",
                "reasoning_mode",
                "reasoning_summary",
                "reasoning",
                "thinking",
                "thinkingConfig",
            ] {
                ext.remove(key);
            }
            if let Some(config) = ext
                .get_mut("generationConfig")
                .and_then(Value::as_object_mut)
            {
                config.remove("thinkingConfig");
            }
        }
    }
}

/// Apply canonical response-format fields after provider-native overrides.
/// This also serves direct transform_request users who select native formats.
pub(crate) fn native_format(request: &Value, wire: WireFormat, body: &mut Value) {
    if let Some(object) = body.as_object_mut() {
        object.remove("x-shim");
    }
    let format = &request["response_format"];
    if format["type"] != "json_schema" {
        return;
    }
    let Some(schema) = format.pointer("/json_schema/schema") else {
        return;
    };
    let output = schema::normalize_output_for(
        target(wire),
        schema,
        format["json_schema"]["strict"].as_bool().unwrap_or(true),
    );
    let name = format["json_schema"]["name"].as_str().unwrap_or("response");
    match wire {
        WireFormat::OpenAiChat => {
            body["response_format"] = json!({"type":"json_schema","json_schema":{"name":name,"schema":output.schema,"strict":output.normalization.strict}})
        }
        WireFormat::OpenAiResponses => {
            if !body["text"].is_object() {
                body["text"] = json!({});
            }
            body["text"]["format"] = json!({"type":"json_schema","name":name,"schema":output.schema,"strict":output.normalization.strict});
            body.as_object_mut().unwrap().remove("response_format");
        }
        WireFormat::AnthropicMessages => {
            if !body["output_config"].is_object() {
                body["output_config"] = json!({});
            }
            body["output_config"]["format"] = json!({"type":"json_schema","schema":output.schema});
        }
        WireFormat::GoogleGenerateContent => {
            if !body["generationConfig"].is_object() {
                body["generationConfig"] = json!({});
            }
            body["generationConfig"]["responseMimeType"] = json!("application/json");
            body["generationConfig"]["responseSchema"] = output.schema;
        }
    }
}

pub(crate) fn add_usage(total: &mut Value, response: &Value) {
    if !total.is_object() {
        *total = json!({});
    }
    for key in [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        // Cost is charged on this, so a repaired answer that drops it would be
        // priced on the whole prompt and double-charge its cached input.
        "uncached_input_tokens",
        "reasoning_tokens",
    ] {
        if let Some(count) = response["usage"][key].as_u64() {
            total[key] = json!(total[key].as_u64().unwrap_or(0).saturating_add(count));
        }
    }
}
/// Buffered, validated output uses the same normalized chunk shape as native streams.
pub(crate) fn chunks(response: Value) -> Vec<Result<String>> {
    let mut chunk = response;
    chunk["object"] = json!("chat.completion.chunk");
    if let Some(choices) = chunk["choices"].as_array_mut() {
        for choice in choices {
            if let Some(message) = choice.as_object_mut().and_then(|c| c.remove("message")) {
                choice["delta"] = message;
            }
        }
    }
    vec![Ok(chunk.to_string())]
}

/// Collect already-normalized chunks; native tool arguments are assembled only
/// by ToolStream, never a second time here.
pub(crate) async fn collect(
    mut stream: std::pin::Pin<Box<dyn futures::Stream<Item = Result<String>> + Send>>,
) -> Result<Value> {
    use futures::StreamExt;
    #[derive(Default)]
    struct Choice {
        message: Value,
        reasoning: crate::reasoning::ReasoningAccumulator,
        finish: Value,
    }
    let mut choices: BTreeMap<u64, Choice> = BTreeMap::new();
    let mut response = json!({"object":"chat.completion","choices":[]});
    let mut bytes = 0usize;
    while let Some(data) = stream.next().await {
        let data = data?;
        bytes = bytes.saturating_add(data.len());
        if bytes > 32 * 1024 * 1024 {
            return Err(ShimError::Stream(
                "buffered response exceeded size limit".into(),
            ));
        }
        let chunk: Value = serde_json::from_str(&data)?;
        for field in ["id", "created", "model", "usage", "system_fingerprint"] {
            if let Some(value) = chunk.get(field) {
                response[field] = value.clone();
            }
        }
        if let Some(incoming) = chunk["choices"].as_array() {
            for item in incoming {
                let choice = choices
                    .entry(item["index"].as_u64().unwrap_or(0))
                    .or_default();
                if !choice.message.is_object() {
                    choice.message = json!({"role":"assistant","content":null});
                }
                let delta = &item["delta"];
                if let Some(text) = delta["content"].as_str() {
                    if !choice.message["content"].is_string() {
                        choice.message["content"] = json!("");
                    }
                    let mut text_out = choice.message["content"].as_str().unwrap().to_owned();
                    text_out.push_str(text);
                    choice.message["content"] = json!(text_out);
                }
                choice.reasoning.push(delta);
                if let Some(calls) = delta["tool_calls"].as_array() {
                    if !choice.message["tool_calls"].is_array() {
                        choice.message["tool_calls"] = json!([]);
                    }
                    choice.message["tool_calls"]
                        .as_array_mut()
                        .unwrap()
                        .extend(calls.iter().cloned());
                }
                if let Some(refusal) = delta.get("refusal").filter(|v| !v.is_null()) {
                    let old = choice.message["refusal"].as_str().unwrap_or("");
                    choice.message["refusal"] =
                        json!(format!("{old}{}", refusal.as_str().unwrap_or("")));
                }
                if item["finish_reason"].is_string() {
                    choice.finish = item["finish_reason"].clone();
                }
            }
        }
    }
    let mut output = Vec::new();
    for (index, mut choice) in choices {
        let blocks = choice.reasoning.blocks();
        if !blocks.is_empty() {
            choice.message["reasoning"] = json!(blocks);
        }
        output.push(json!({"index":index,"message":choice.message,"finish_reason":choice.finish}));
    }
    response["choices"] = json!(output);
    Ok(response)
}

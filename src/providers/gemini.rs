use crate::error::{Result, ShimError};
use crate::provider::{Provider, ProviderRequest};
use crate::vision;
use serde_json::{json, Value};

pub struct Gemini {
    pub api_key: String,
    pub base_url: String,
}

impl Gemini {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
        }
    }

    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }
}

// -- Request transformation helpers --

/// Convert OpenAI messages to Gemini contents + optional systemInstruction.
fn transform_messages(messages: &[Value]) -> Result<(Option<Value>, Vec<Value>)> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    // Track tool_call_id → function name from assistant messages.
    let mut call_id_to_name: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "system" | "developer" => {
                if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                    system_parts.push(text.to_string());
                }
            }
            "assistant" => {
                // Record tool_call_id → name mapping for subsequent tool results.
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tool_calls {
                        if let (Some(id), Some(name)) = (
                            tc.get("id").and_then(|v| v.as_str()),
                            tc.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str()),
                        ) {
                            call_id_to_name.insert(id.to_string(), name.to_string());
                        }
                    }
                }
                // Gemini uses "model" role for assistant
                let mut parts = build_parts(msg);
                sanitize_parts(&mut parts);
                contents.push(json!({
                    "role": "model",
                    "parts": parts,
                }));
            }
            "tool" => {
                // OpenAI tool result → Gemini functionResponse
                let call_id = msg
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let name = msg
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| call_id_to_name.get(call_id).cloned())
                    .unwrap_or_else(|| "function".to_string());
                let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                // Gemini requires response to be an object, never an array or primitive.
                let parsed: Value = match crate::json_bounds::parse_str(
                    content,
                    crate::json_bounds::Limits::INBOUND,
                ) {
                    Ok(value) => value,
                    Err(crate::json_bounds::ParseError::Malformed(_)) => {
                        json!({"result": content})
                    }
                    Err(crate::json_bounds::ParseError::Complexity) => {
                        return Err(ShimError::ProviderError {
                            status: 400,
                            body: "tool result exceeds JSON complexity limit".into(),
                            retry_after: None,
                        })
                    }
                };
                let response = if parsed.is_object() {
                    parsed
                } else {
                    json!({"result": parsed})
                };
                let mut part = json!({"functionResponse":{"name":name,"response":response}});
                if let Some(id) = msg.get("_llmshim_wire_id").filter(|v| v.is_string()) {
                    part["functionResponse"]["id"] = id.clone();
                }
                contents.push(json!({"role":"user","parts":[part]}));
            }
            _ => {
                // "user" and anything else
                let parts = build_parts(msg);
                contents.push(json!({
                    "role": "user",
                    "parts": parts,
                }));
            }
        }
    }

    let system_instruction = if system_parts.is_empty() {
        None
    } else {
        Some(json!({
            "parts": [{"text": system_parts.join("\n\n")}]
        }))
    };

    // Post-process: enforce Gemini's strict turn ordering requirements.
    let contents = merge_same_role(contents);

    Ok((system_instruction, contents))
}

/// Gemini's own repair, not a shared one. This is the only wire in the crate
/// known to reject adjacent same-role turns, so it is the only adapter that
/// folds them together. Every other adapter passes adjacency through — each
/// says why on its own message pass — because a merge destroys message
/// boundaries a caller may depend on, and only a wire that would otherwise
/// fail the request earns that.
fn merge_same_role(turns: Vec<Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for turn in turns {
        if turn["role"] == "model"
            && turn["parts"].as_array().is_some_and(|parts| {
                parts.iter().all(|p| {
                    p.as_object().is_some_and(|o| o.len() == 1) && p["text"].as_str() == Some("")
                })
            })
        {
            continue;
        }
        let role = turn.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let last_role = merged
            .last()
            .and_then(|t| t.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("");
        if role == last_role {
            if let Some(new_parts) = turn.get("parts").and_then(|p| p.as_array()) {
                if let Some(last) = merged.last_mut() {
                    if let Some(existing) = last.get_mut("parts").and_then(|p| p.as_array_mut()) {
                        existing.extend(new_parts.clone());
                    }
                }
            }
        } else {
            merged.push(turn);
        }
    }
    merged
}

/// Build parts array from an OpenAI message.
fn build_parts(msg: &Value) -> Vec<Value> {
    let mut parts = crate::reasoning::gemini_parts(msg);

    // Text content (string or array of content blocks)
    match msg.get("content") {
        Some(Value::String(text)) if !text.is_empty() => {
            parts.push(json!({"text": text}));
        }
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            parts.push(json!({"text": text}));
                        }
                    }
                    Some("image_url" | "input_image" | "image") => {
                        if let Some(gemini_part) = vision::to_gemini(block) {
                            parts.push(gemini_part);
                        }
                    }
                    _ => {} // skip unknown block types
                }
            }
        }
        _ => {}
    }

    // Tool calls (OpenAI format) → Gemini functionCall parts
    if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            if let Some(func) = tc.get("function") {
                let name = func.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args: Value = func
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(json!({}));
                let mut fc_part = json!({"functionCall": {"name": name, "args": args}});
                if let Some(id) = tc.get("_llmshim_wire_id").filter(|v| v.is_string()) {
                    fc_part["functionCall"]["id"] = id.clone();
                }
                // Echo thought_signature back — Gemini requires it for tool roundtrips
                if let Some(sig) = tc.get("thought_signature") {
                    fc_part["thoughtSignature"] = sig.clone();
                }
                parts.push(fc_part);
            }
        }
    }

    if parts.is_empty() {
        parts.push(json!({"text": ""}));
    }
    parts
}

/// Remove cross-provider fields that Gemini won't understand.
fn sanitize_parts(parts: &mut Vec<Value>) {
    // Remove any text parts that contain only thoughtSignature artifacts
    parts.retain(|p| {
        // Keep all non-text parts
        if p.get("text").is_none() && p.get("functionCall").is_none() {
            return true;
        }
        true
    });
}

/// Convert OpenAI tools array to Gemini functionDeclarations.
/// Handles both nested (Chat Completions) and flat (Responses API) tool formats.
/// Sanitizes JSON Schema for Gemini compatibility.
fn transform_tools(tools: &[Value]) -> Value {
    let empty = json!("");
    let default_params = json!({"type": "object", "properties": {}});
    let declarations: Vec<Value> = tools
        .iter()
        .filter_map(|tool| {
            // Support both nested {"function": {"name": ...}} and flat {"name": ...}
            let source = tool.get("function").unwrap_or(tool);
            let name = source.get("name")?;
            let description = source.get("description").unwrap_or(&empty);
            let parameters = source.get("parameters").unwrap_or(&default_params).clone();
            Some(json!({
                "name": name,
                "description": description,
                "parameters": parameters,
            }))
        })
        .collect();
    json!([{ "functionDeclarations": declarations }])
}

/// Translate OpenAI tool_choice to Gemini toolConfig.
fn translate_tool_choice(tc: &Value) -> Option<Value> {
    let mode = if let Some(s) = tc.as_str() {
        match s {
            "auto" => "AUTO",
            "required" => "ANY",
            "none" => "NONE",
            _ => return None,
        }
    } else {
        let obj = tc.as_object()?;
        match obj.get("type").and_then(|t| t.as_str()) {
            Some("auto") => "AUTO",
            Some("any" | "required") => "ANY",
            Some("none") => "NONE",
            _ => return None,
        }
    };
    Some(json!({"functionCallingConfig": {"mode": mode}}))
}

// -- Response transformation helpers --

fn normalized_gemini_usage(usage: &Value) -> Value {
    let mut result = json!({
        "prompt_tokens": usage.get("promptTokenCount").cloned().unwrap_or(json!(0)),
        "completion_tokens": usage.get("candidatesTokenCount").cloned().unwrap_or(json!(0)),
        "total_tokens": usage.get("totalTokenCount").cloned().unwrap_or(json!(0)),
    });
    crate::usage::normalize_cache(usage, &mut result);
    result
}

fn transform_response_to_openai(model: &str, resp: &Value) -> Result<Value> {
    let candidate = resp
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| ShimError::ProviderError {
            status: 500,
            body: format!("no candidates in response: {}", resp),
            retry_after: None,
        })?;

    let parts = candidate
        .pointer("/content/parts")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for part in &parts {
        let is_thought = part
            .get("thought")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
            if !text.is_empty() && !is_thought {
                text_parts.push(text.to_string());
            }
        }
        if let Some(fc) = part.get("functionCall") {
            let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let id = fc
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("call_{}", tool_calls.len()));
            let args_str = fc
                .get("args")
                .filter(|a| !a.is_null())
                .map(|a| serde_json::to_string(a).unwrap_or_else(|_| "{}".to_string()))
                .unwrap_or_else(|| "{}".to_string());
            let mut tc = json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": args_str,
                }
            });
            // Preserve thought_signature — Gemini requires it echoed back in follow-up requests
            if let Some(sig) = part.get("thoughtSignature") {
                tc["thought_signature"] = sig.clone();
            }
            tool_calls.push(tc);
        }
    }

    let content = if text_parts.is_empty() {
        Value::Null
    } else {
        json!(text_parts.join(""))
    };

    let finish_reason = match candidate.get("finishReason").and_then(Value::as_str) {
        Some("STOP") => "stop",
        Some("MAX_TOKENS") => "length",
        Some("SAFETY") => "content_filter",
        _ => {
            return Err(ShimError::ProviderError {
                status: 502,
                body: "Gemini response has no supported terminal finish reason".into(),
                retry_after: None,
            })
        }
    };

    let usage = resp.get("usageMetadata").cloned().unwrap_or(json!({}));

    let mut message = json!({
        "role": "assistant",
        "content": content,
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    let result = json!({
        "id": resp.get("responseId").cloned().unwrap_or(json!("")),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
        "usage": normalized_gemini_usage(&usage)
    });
    Ok(result)
}

/// Models that cannot turn thinking off: gemini-3.1-pro, gemini-3.7-flash, and
/// gemini-3.8-flash reject thinkingLevel "minimal" (and thinkingBudget 0) —
/// verified live. Unified effort "none" clamps to "low" for them.
fn cannot_disable_thinking(model: &str) -> bool {
    let m = model.to_lowercase();
    // gemini-3.1-pro, gemini-3.7-flash and gemini-3.8-flash reject thinkingLevel
    // "minimal" (and thinkingBudget 0), verified live — unified "none" clamps to
    // "low". Other flash models (3.5/3.6/3.5-lite) accept "minimal" and can
    // disable thinking.
    m.contains("3.1-pro") || m.contains("3.7-flash") || m.contains("3.8-flash")
}

impl Provider for Gemini {
    fn name(&self) -> &str {
        "gemini"
    }

    fn request_admission_policy(&self) -> crate::provider::RequestAdmissionPolicy {
        crate::provider::RequestAdmissionPolicy::namespaced(
            "x-gemini",
            &["contents"],
            &["systemInstruction", "tools", "toolConfig", "thinkingConfig"],
            &[],
        )
    }

    fn replay_target(&self, model: &str) -> crate::reasoning::ReplayTarget {
        crate::reasoning::ReplayTarget::new(
            self.name(),
            model,
            crate::reasoning::WireFormat::GoogleGenerateContent,
        )
        .bind_account(&self.base_url, Some(&self.api_key))
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        crate::reasoning::preflight_request(request)?;
        let mut schema_budget = crate::schema::RequestBudget::new();
        let request = crate::schema::prepare_request(request, &mut schema_budget)?;
        let request = crate::cache::prepare_request(
            &request,
            crate::reasoning::WireFormat::GoogleGenerateContent,
        )?;
        let request = crate::reasoning::prepare_request(&request, &self.replay_target(model))?;
        let request = crate::toolcall::prepare_request(&request, &self.replay_target(model))?;
        let obj = request.as_object().ok_or(ShimError::MissingModel)?;

        let messages = obj
            .get("messages")
            .and_then(|m| m.as_array())
            .ok_or_else(|| {
                ShimError::Json(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing messages array",
                )))
            })?;

        let (system_instruction, contents) = transform_messages(messages)?;

        let mut body = json!({"contents": contents});
        let body_obj = body.as_object_mut().unwrap();

        if let Some(si) = system_instruction {
            body_obj.insert("systemInstruction".to_string(), si);
        }

        // Build generationConfig
        let mut gen_config = json!({});
        let gc = gen_config.as_object_mut().unwrap();

        if let Some(v) = obj.get("temperature") {
            gc.insert("temperature".to_string(), v.clone());
        }
        if let Some(v) = obj.get("top_p") {
            gc.insert("topP".to_string(), v.clone());
        }
        if let Some(v) = obj.get("top_k") {
            gc.insert("topK".to_string(), v.clone());
        }
        if let Some(v) = obj.get("max_tokens").or(obj.get("max_completion_tokens")) {
            gc.insert("maxOutputTokens".to_string(), v.clone());
        }
        if let Some(v) = obj.get("stop") {
            gc.insert("stopSequences".to_string(), v.clone());
        }

        // Thinking config: translate the unified reasoning controls
        // (reasoning_effort + reasoning_mode) onto Gemini 3.x's 4-level
        // thinkingLevel enum (minimal < low < medium < high).
        let effort = obj
            .get("reasoning_effort")
            .and_then(|e| e.as_str())
            .or_else(|| {
                obj.get("output_config")
                    .and_then(|oc| oc.get("effort"))
                    .and_then(|e| e.as_str())
            });

        // mode:"pro" has no Gemini analog (Deep Think is not API-accessible);
        // map it to a one-tier bump toward the enum ceiling, same policy as
        // the other providers without a native mode. Explicit "none" wins.
        let pro = obj
            .get("reasoning_mode")
            .and_then(|m| m.as_str())
            .map(|m| m == "pro")
            .unwrap_or(false);

        let level = effort.map(|e| {
            // "none" disables thinking via level "minimal" — except
            // gemini-3.1-pro, which cannot disable thinking (rejects minimal
            // AND budget 0, verified live); clamp to its floor.
            if e == "none" {
                return if cannot_disable_thinking(model) {
                    "low"
                } else {
                    "minimal"
                };
            }
            let base = match e {
                "minimal" | "low" => "low",
                "medium" => "medium",
                // Gemini's enum tops out at "high": xhigh/max clamp to it.
                "high" | "xhigh" | "max" => "high",
                _ => "medium",
            };
            if pro {
                match base {
                    "low" => "medium",
                    _ => "high",
                }
            } else {
                base
            }
        });

        {
            let mut thinking_config = json!({"includeThoughts": true});
            if let Some(lvl) = level {
                thinking_config["thinkingLevel"] = json!(lvl);
            }
            gc.insert("thinkingConfig".to_string(), thinking_config);
        }

        // Direct thinkingConfig passthrough via x-gemini
        if let Some(ext) = obj.get("x-gemini").and_then(|e| e.as_object()) {
            if let Some(tc) = ext.get("thinkingConfig") {
                gc.insert("thinkingConfig".to_string(), tc.clone());
            }
            // Pass through any other x-gemini fields to body (e.g., safetySettings)
            for (k, v) in ext {
                if k != "thinkingConfig" {
                    body_obj.insert(k.clone(), v.clone());
                }
            }
        }

        if !gc.is_empty() {
            body_obj.insert("generationConfig".to_string(), gen_config);
        }

        // Tools
        if let Some(tools) = obj.get("tools").and_then(|t| t.as_array()) {
            body_obj.insert("tools".to_string(), transform_tools(tools));
        }

        // tool_choice → toolConfig
        if let Some(tc) = obj.get("tool_choice") {
            if let Some(config) = translate_tool_choice(tc) {
                body_obj.insert("toolConfig".to_string(), config);
            }
        }

        // Determine if streaming for URL
        let is_stream = obj.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
        let method = if is_stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let mut url = format!(
            "{}/models/{}:{}?key={}",
            self.base_url, model, method, self.api_key
        );
        if is_stream {
            url.push_str("&alt=sse");
        }

        crate::toolcall::validate_native(&body, &self.replay_target(model))?;
        crate::schema::normalize_native_tools(
            crate::schema::Target::Google,
            &mut body,
            &mut schema_budget,
        )?;
        crate::shim::native_format(
            &request,
            crate::reasoning::WireFormat::GoogleGenerateContent,
            &mut body,
            &mut schema_budget,
        )?;
        crate::cache::finish_request(
            &request,
            &mut body,
            crate::reasoning::WireFormat::GoogleGenerateContent,
        )?;
        Ok(ProviderRequest {
            url,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body,
        })
    }

    fn transform_response(&self, model: &str, response: Value) -> Result<Value> {
        let native = response.clone();
        let mut result = self.transform_response_native(model, response)?;
        crate::derived_response::capture_unary(&self.replay_target(model), &native, &mut result)?;
        Ok(result)
    }

    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        crate::json_bounds::enforce_sse_complexity(chunk)?;
        let result = self.transform_stream_chunk_native(model, chunk)?;
        let native: Value = match serde_json::from_str(chunk) {
            Ok(v) => v,
            Err(_) => return Ok(result),
        };
        crate::reasoning::capture_stream(&self.replay_target(model), &native, result)
    }
}

impl Gemini {
    fn transform_response_native(&self, model: &str, response: Value) -> Result<Value> {
        if let Some(err) = response.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            let code = err.get("code").and_then(|c| c.as_u64()).unwrap_or(400) as u16;
            return Err(ShimError::ProviderError {
                status: code,
                body: msg.to_string(),
                retry_after: None,
            });
        }
        transform_response_to_openai(model, &response)
    }
}

impl Gemini {
    fn transform_stream_chunk_native(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        let parsed: Value = serde_json::from_str(trimmed)?;

        // Check for error
        if parsed.get("error").is_some() {
            return self.transform_response(model, parsed).map(|_| None);
        }

        let candidate = match parsed
            .get("candidates")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
        {
            Some(c) => c,
            None => {
                return Ok(parsed
                    .get("usageMetadata")
                    .filter(|u| u.is_object())
                    .map(|usage| {
                        json!({"object":"chat.completion.chunk", "model":model, "choices":[],
                        "usage":normalized_gemini_usage(usage)})
                        .to_string()
                    }));
            }
        };

        let parts = candidate
            .pointer("/content/parts")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();

        // Extract text, thoughts, and tool calls from parts
        let mut text = String::new();

        for part in &parts {
            let is_thought = part
                .get("thought")
                .and_then(|t| t.as_bool())
                .unwrap_or(false);
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                if !t.is_empty() && !is_thought {
                    text.push_str(t);
                }
            }
        }

        let finish_reason =
            candidate
                .get("finishReason")
                .and_then(|f| f.as_str())
                .map(|f| match f {
                    "STOP" => "stop",
                    "MAX_TOKENS" => "length",
                    "SAFETY" => "content_filter",
                    _ => "stop",
                });

        // Build delta
        let mut delta = json!({});
        if !text.is_empty() {
            delta["content"] = json!(text);
        }

        // Skip chunks with no useful content (e.g., thoughtSignature-only)
        if delta.as_object().map(|o| o.is_empty()).unwrap_or(true) && finish_reason.is_none() {
            return Ok(None);
        }

        let mut chunk_json = json!({
            "object": "chat.completion.chunk",
            "model": model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }]
        });

        if let Some(usage) = parsed.get("usageMetadata").filter(|u| u.is_object()) {
            chunk_json["usage"] = normalized_gemini_usage(usage);
        }

        Ok(Some(serde_json::to_string(&chunk_json)?))
    }
}

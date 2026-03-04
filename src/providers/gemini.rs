use crate::error::{Result, ShimError};
use crate::provider::{Provider, ProviderRequest};
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
fn transform_messages(messages: &[Value]) -> (Option<Value>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "system" | "developer" => {
                if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                    system_parts.push(text.to_string());
                }
            }
            "assistant" => {
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
                let name = msg
                    .get("name")
                    .or_else(|| msg.get("tool_call_id"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("function");
                let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                let response: Value =
                    serde_json::from_str(content).unwrap_or_else(|_| json!({"result": content}));
                contents.push(json!({
                    "role": "user",
                    "parts": [{"functionResponse": {"name": name, "response": response}}]
                }));
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

    (system_instruction, contents)
}

/// Build parts array from an OpenAI message.
fn build_parts(msg: &Value) -> Vec<Value> {
    let mut parts = Vec::new();

    // Text content
    if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            parts.push(json!({"text": text}));
        }
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
                parts.push(json!({"functionCall": {"name": name, "args": args}}));
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
fn transform_tools(tools: &[Value]) -> Value {
    let declarations: Vec<Value> = tools.iter().filter_map(|tool| {
        let func = tool.get("function")?;
        Some(json!({
            "name": func.get("name")?,
            "description": func.get("description").unwrap_or(&json!("")),
            "parameters": func.get("parameters").unwrap_or(&json!({"type": "object", "properties": {}})),
        }))
    }).collect();
    json!([{"functionDeclarations": declarations}])
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
    } else if let Some(obj) = tc.as_object() {
        match obj.get("type").and_then(|t| t.as_str()) {
            Some("auto") => "AUTO",
            Some("any" | "required") => "ANY",
            Some("none") => "NONE",
            _ => return None,
        }
    } else {
        return None;
    };
    Some(json!({"functionCallingConfig": {"mode": mode}}))
}

// -- Response transformation helpers --

fn transform_response_to_openai(model: &str, resp: &Value) -> Result<Value> {
    let candidate = resp
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| ShimError::ProviderError {
            status: 500,
            body: format!("no candidates in response: {}", resp),
        })?;

    let parts = candidate
        .pointer("/content/parts")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    let mut text_parts: Vec<String> = Vec::new();
    let mut thought_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for part in &parts {
        let is_thought = part
            .get("thought")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
            if !text.is_empty() {
                if is_thought {
                    thought_parts.push(text.to_string());
                } else {
                    text_parts.push(text.to_string());
                }
            }
        }
        if let Some(fc) = part.get("functionCall") {
            let id = format!("call_{}", tool_calls.len());
            tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": fc.get("name").cloned().unwrap_or(json!("")),
                    "arguments": fc.get("args")
                        .map(|a| serde_json::to_string(a).unwrap_or_default())
                        .unwrap_or_default(),
                }
            }));
        }
    }

    let content = if text_parts.is_empty() {
        Value::Null
    } else {
        json!(text_parts.join(""))
    };

    let finish_reason = candidate
        .get("finishReason")
        .and_then(|f| f.as_str())
        .map(|f| match f {
            "STOP" => "stop",
            "MAX_TOKENS" => "length",
            "SAFETY" => "content_filter",
            _ => "stop",
        })
        .unwrap_or("stop");

    let usage = resp.get("usageMetadata").cloned().unwrap_or(json!({}));

    let mut message = json!({
        "role": "assistant",
        "content": content,
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    if !thought_parts.is_empty() {
        message["reasoning_content"] = json!(thought_parts.join("\n"));
    }

    Ok(json!({
        "id": resp.get("responseId").cloned().unwrap_or(json!("")),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
        "usage": {
            "prompt_tokens": usage.get("promptTokenCount").cloned().unwrap_or(json!(0)),
            "completion_tokens": usage.get("candidatesTokenCount").cloned().unwrap_or(json!(0)),
            "total_tokens": usage.get("totalTokenCount").cloned().unwrap_or(json!(0)),
        }
    }))
}

impl Provider for Gemini {
    fn name(&self) -> &str {
        "gemini"
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
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

        let (system_instruction, contents) = transform_messages(messages);

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

        // Thinking config: always include thoughts, translate reasoning_effort to thinkingLevel
        let effort = obj
            .get("reasoning_effort")
            .and_then(|e| e.as_str())
            .or_else(|| {
                obj.get("output_config")
                    .and_then(|oc| oc.get("effort"))
                    .and_then(|e| e.as_str())
            });

        let level = effort.map(|e| match e {
            "low" | "minimal" => "low",
            "medium" => "medium",
            "high" => "high",
            "none" => "minimal",
            _ => "medium",
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

        Ok(ProviderRequest {
            url,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body,
        })
    }

    fn transform_response(&self, model: &str, response: Value) -> Result<Value> {
        if let Some(err) = response.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            let code = err.get("code").and_then(|c| c.as_u64()).unwrap_or(400) as u16;
            return Err(ShimError::ProviderError {
                status: code,
                body: msg.to_string(),
            });
        }
        transform_response_to_openai(model, &response)
    }

    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
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
            None => return Ok(None),
        };

        let parts = candidate
            .pointer("/content/parts")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();

        // Extract text, thoughts, and tool calls from parts
        let mut text = String::new();
        let mut thought_text = String::new();
        let mut has_function_call = false;
        let mut tool_calls: Vec<Value> = Vec::new();

        for part in &parts {
            let is_thought = part
                .get("thought")
                .and_then(|t| t.as_bool())
                .unwrap_or(false);
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                if !t.is_empty() {
                    if is_thought {
                        thought_text.push_str(t);
                    } else {
                        text.push_str(t);
                    }
                }
            }
            if let Some(fc) = part.get("functionCall") {
                has_function_call = true;
                let id = format!("call_{}", tool_calls.len());
                tool_calls.push(json!({
                    "index": tool_calls.len(),
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": fc.get("name").cloned().unwrap_or(json!("")),
                        "arguments": fc.get("args")
                            .map(|a| serde_json::to_string(a).unwrap_or_default())
                            .unwrap_or_default(),
                    }
                }));
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
        if !thought_text.is_empty() {
            delta["reasoning_content"] = json!(thought_text);
        }
        if !text.is_empty() {
            delta["content"] = json!(text);
        }
        if has_function_call {
            delta["tool_calls"] = json!(tool_calls);
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

        // Add usage on final chunk
        if finish_reason.is_some() {
            if let Some(usage) = parsed.get("usageMetadata") {
                chunk_json["usage"] = json!({
                    "prompt_tokens": usage.get("promptTokenCount").cloned().unwrap_or(json!(0)),
                    "completion_tokens": usage.get("candidatesTokenCount").cloned().unwrap_or(json!(0)),
                });
            }
        }

        Ok(Some(serde_json::to_string(&chunk_json)?))
    }
}

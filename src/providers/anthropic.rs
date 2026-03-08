use crate::error::{Result, ShimError};
use crate::provider::{Provider, ProviderRequest};
use crate::vision;
use serde_json::{json, Value};

pub struct Anthropic {
    pub api_key: String,
    pub base_url: String,
}

impl Anthropic {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: "https://api.anthropic.com/v1".to_string(),
        }
    }

    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }

    fn is_claude_4_6(model: &str) -> bool {
        let m = model.to_lowercase();
        m.contains("4-6") || m.contains("4.6") || m.contains("4_6")
    }

    /// Models that support the 1M context window beta.
    /// Opus 4.6, Sonnet 4.6, Sonnet 4.5, and Sonnet 4.
    fn supports_1m_context(model: &str) -> bool {
        let m = model.to_lowercase();
        m.contains("opus-4") || m.contains("sonnet-4")
    }

    fn supports_thinking(model: &str) -> bool {
        let m = model.to_lowercase();
        // Claude 3.7 Sonnet and all Claude 4+ models support thinking
        m.contains("3-7")
            || m.contains("3.7")
            || m.contains("3_7")
            || m.contains("claude-4")
            || m.contains("claude-sonnet-4")
            || m.contains("claude-opus-4")
            || m.contains("claude-haiku-4")
            || Self::is_claude_4_6(&m)
    }
}

// -- Request transformation helpers --

fn extract_system_message(messages: &[Value]) -> (Option<String>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut rest: Vec<Value> = Vec::new();

    for msg in messages {
        match msg.get("role").and_then(|r| r.as_str()) {
            Some("system" | "developer") => {
                if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                    system_parts.push(content.to_string());
                }
            }
            _ => rest.push(msg.clone()),
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, rest)
}

fn transform_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| {
            let mut out = msg.clone();

            // Sanitize cross-provider fields that Anthropic's API rejects.
            // This enables multi-model conversations (e.g., Cursor-style provider switching).
            if let Some(obj) = out.as_object_mut() {
                obj.remove("reasoning_content"); // our normalized thinking field
                obj.remove("annotations"); // OpenAI returns this on every message
                obj.remove("refusal"); // OpenAI safety refusal field
                obj.remove("audio"); // OpenAI audio response field
                obj.remove("logprobs"); // OpenAI logprobs on message
            }

            // Translate image content blocks from OpenAI format to Anthropic format
            if let Some(content) = out.get("content").cloned() {
                if content.is_array() {
                    out["content"] =
                        vision::translate_content_blocks(&content, vision::to_anthropic);
                }
            }

            // Anthropic doesn't have a "function" role — map to "user" with context
            if out.get("role").and_then(|r| r.as_str()) == Some("function") {
                out["role"] = json!("user");
            }
            // Transform tool_calls from OpenAI format to Anthropic content blocks
            if let Some(tool_calls) = out.get("tool_calls").cloned() {
                if let Some(arr) = tool_calls.as_array() {
                    let mut content_blocks: Vec<Value> = Vec::new();

                    // Preserve any existing text content
                    if let Some(text) = out.get("content").and_then(|c| c.as_str()) {
                        if !text.is_empty() {
                            content_blocks.push(json!({"type": "text", "text": text}));
                        }
                    }

                    for tc in arr {
                        let func = &tc["function"];
                        let input: Value = func
                            .get("arguments")
                            .and_then(|a| a.as_str())
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or(json!({}));

                        content_blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.get("id").cloned().unwrap_or(json!("")),
                            "name": func.get("name").cloned().unwrap_or(json!("")),
                            "input": input,
                        }));
                    }

                    let obj = out.as_object_mut().unwrap();
                    obj.remove("tool_calls");
                    obj.insert("content".to_string(), json!(content_blocks));
                }
            }

            // Transform tool role messages to Anthropic format
            if out.get("role").and_then(|r| r.as_str()) == Some("tool") {
                let content = out.get("content").cloned().unwrap_or(json!(""));
                let tool_use_id = out.get("tool_call_id").cloned().unwrap_or(json!(""));

                out = json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content,
                    }]
                });
            }

            out
        })
        .collect()
}

fn transform_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let func = tool.get("function")?;
            Some(json!({
                "name": func.get("name")?,
                "description": func.get("description").unwrap_or(&json!("")),
                "input_schema": func.get("parameters").unwrap_or(&json!({"type": "object", "properties": {}})),
            }))
        })
        .collect()
}

/// Translate OpenAI-style tool_choice to Anthropic format.
fn translate_tool_choice(tc: &Value) -> Option<Value> {
    // OpenAI accepts strings or objects
    if let Some(s) = tc.as_str() {
        return match s {
            "auto" => Some(json!({"type": "auto"})),
            "required" => Some(json!({"type": "any"})),
            "none" => Some(json!({"type": "none"})),
            _ => None,
        };
    }
    if let Some(obj) = tc.as_object() {
        // If it already has Anthropic-style "type" field (auto/any/tool), pass through
        if let Some(t) = obj.get("type").and_then(|t| t.as_str()) {
            if matches!(t, "auto" | "any" | "none" | "tool") {
                return Some(tc.clone());
            }
        }
        // OpenAI-style: {"type": "function", "function": {"name": "..."}}
        if let Some(func) = obj.get("function") {
            if let Some(name) = func.get("name") {
                return Some(json!({"type": "tool", "name": name}));
            }
        }
    }
    None
}

// -- Response transformation helpers --

fn transform_response_to_openai(model: &str, resp: &Value) -> Value {
    let content_blocks = resp
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut thinking_content: Option<String> = None;

    for block in &content_blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(t.to_string());
                }
            }
            Some("thinking") => {
                if let Some(t) = block.get("thinking").and_then(|t| t.as_str()) {
                    thinking_content = Some(t.to_string());
                }
            }
            Some("tool_use") => {
                tool_calls.push(json!({
                    "id": block.get("id").cloned().unwrap_or(json!("")),
                    "type": "function",
                    "function": {
                        "name": block.get("name").cloned().unwrap_or(json!("")),
                        "arguments": block.get("input")
                            .map(|v| serde_json::to_string(v).unwrap_or_default())
                            .unwrap_or_default(),
                    }
                }));
            }
            _ => {}
        }
    }

    let content = if text_parts.is_empty() {
        Value::Null
    } else {
        json!(text_parts.join(""))
    };

    let stop_reason = resp
        .get("stop_reason")
        .and_then(|r| r.as_str())
        .map(|r| match r {
            "end_turn" => "stop",
            "max_tokens" => "length",
            "tool_use" => "tool_calls",
            other => other,
        })
        .unwrap_or("stop");

    let usage = resp.get("usage").cloned().unwrap_or(json!({}));

    let mut message = json!({
        "role": "assistant",
        "content": content,
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    // Surface thinking content in a way OpenAI SDK consumers can access
    if let Some(thinking) = thinking_content {
        message["reasoning_content"] = json!(thinking);
    }

    json!({
        "id": resp.get("id").cloned().unwrap_or(json!("")),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": stop_reason,
        }],
        "usage": {
            "prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
            "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens":
                usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) +
                usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        }
    })
}

impl Provider for Anthropic {
    fn name(&self) -> &str {
        "anthropic"
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

        let (system, user_messages) = extract_system_message(messages);
        let anthropic_messages = transform_messages(&user_messages);

        let mut body = json!({
            "model": model,
            "messages": anthropic_messages,
        });

        let body_obj = body.as_object_mut().unwrap();

        // System message
        if let Some(sys) = system {
            body_obj.insert("system".to_string(), json!(sys));
        }

        // max_tokens — required by Anthropic
        if let Some(mt) = obj.get("max_tokens").or(obj.get("max_completion_tokens")) {
            body_obj.insert("max_tokens".to_string(), mt.clone());
        } else {
            body_obj.insert("max_tokens".to_string(), json!(8192));
        }

        // Standard params passthrough
        for key in &["temperature", "top_p", "top_k", "stop", "stream"] {
            if let Some(v) = obj.get(*key) {
                body_obj.insert(key.to_string(), v.clone());
            }
        }

        // Tools
        if let Some(tools) = obj.get("tools").and_then(|t| t.as_array()) {
            body_obj.insert("tools".to_string(), json!(transform_tools(tools)));
        }

        // tool_choice translation
        if let Some(tc) = obj.get("tool_choice") {
            if let Some(translated) = translate_tool_choice(tc) {
                body_obj.insert("tool_choice".to_string(), translated);
            }
        }

        // Anthropic-specific extensions (x-anthropic namespace)
        if let Some(ext) = obj.get("x-anthropic").and_then(|e| e.as_object()) {
            for (k, v) in ext {
                body_obj.insert(k.clone(), v.clone());
            }
        }

        // -- Thinking / reasoning support --
        let has_thinking = obj.contains_key("thinking")
            || obj
                .get("x-anthropic")
                .and_then(|x| x.get("thinking"))
                .is_some();

        // Handle reasoning_effort -> Anthropic thinking translation
        if let Some(effort) = obj.get("reasoning_effort").and_then(|e| e.as_str()) {
            if Self::supports_thinking(model) && !has_thinking {
                if Self::is_claude_4_6(model) {
                    // Claude 4.6: use adaptive thinking with output_config.effort
                    body_obj.insert("thinking".to_string(), json!({"type": "adaptive"}));
                    let anthropic_effort = match effort {
                        "low" | "minimal" => "low",
                        "medium" => "medium",
                        "high" => "high",
                        _ => "medium",
                    };
                    body_obj.insert(
                        "output_config".to_string(),
                        json!({"effort": anthropic_effort}),
                    );
                } else {
                    // Pre-4.6: use enabled thinking with a budget based on effort
                    let max_tokens = body_obj
                        .get("max_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(8192);
                    let budget = match effort {
                        "low" | "minimal" => 1024_u64.max(max_tokens / 4),
                        "medium" => max_tokens / 2,
                        "high" => max_tokens.saturating_sub(1),
                        _ => max_tokens / 2,
                    };
                    let budget = budget.max(1024); // Anthropic minimum
                    body_obj.insert(
                        "thinking".to_string(),
                        json!({
                            "type": "enabled",
                            "budget_tokens": budget
                        }),
                    );
                }
                // Thinking requires temperature=1, so remove any custom temperature
                body_obj.remove("temperature");
                body_obj.remove("top_k");
            }
        }

        // If user passed thinking directly (via x-anthropic or top-level), handle constraints
        if body_obj.contains_key("thinking") {
            let thinking_type = body_obj
                .get("thinking")
                .and_then(|t| t.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if thinking_type == "enabled" || thinking_type == "adaptive" {
                // Temperature must be 1 (default) when thinking is enabled
                body_obj.remove("temperature");
                body_obj.remove("top_k");
            }
        }

        // Pass through top-level thinking if user provided it directly
        if let Some(thinking) = obj.get("thinking") {
            if !body_obj.contains_key("thinking") {
                body_obj.insert("thinking".to_string(), thinking.clone());
            }
        }

        // Pass through output_config if user provided it directly
        if let Some(output_config) = obj.get("output_config") {
            if !body_obj.contains_key("output_config") {
                body_obj.insert("output_config".to_string(), output_config.clone());
            }
        }

        let url = format!("{}/messages", self.base_url);

        // Build headers — include 1M context beta by default for supported models
        let mut headers = vec![
            ("x-api-key".into(), self.api_key.clone()),
            ("anthropic-version".into(), "2023-06-01".into()),
            ("content-type".into(), "application/json".into()),
        ];

        // Add 1M context window beta header by default (can be disabled via x-anthropic)
        let disable_1m = obj
            .get("x-anthropic")
            .and_then(|x| x.get("disable_1m_context"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !disable_1m && Self::supports_1m_context(model) {
            headers.push(("anthropic-beta".into(), "context-1m-2025-08-07".into()));
        }

        Ok(ProviderRequest { url, headers, body })
    }

    fn transform_response(&self, model: &str, response: Value) -> Result<Value> {
        // Check for API error
        if let Some(err) = response.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(ShimError::ProviderError {
                status: 400,
                body: msg.to_string(),
            });
        }

        Ok(transform_response_to_openai(model, &response))
    }

    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        let parsed: Value = serde_json::from_str(trimmed)?;
        let event_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            "message_start" => {
                let id = parsed
                    .pointer("/message/id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let chunk = json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": { "role": "assistant", "content": "" },
                        "finish_reason": null,
                    }]
                });
                Ok(Some(serde_json::to_string(&chunk)?))
            }
            "content_block_delta" => {
                let delta = &parsed["delta"];
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        let chunk = json!({
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": { "content": text },
                                "finish_reason": null,
                            }]
                        });
                        Ok(Some(serde_json::to_string(&chunk)?))
                    }
                    Some("thinking_delta") => {
                        let thinking = delta.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                        let chunk = json!({
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": { "reasoning_content": thinking },
                                "finish_reason": null,
                            }]
                        });
                        Ok(Some(serde_json::to_string(&chunk)?))
                    }
                    Some("input_json_delta") => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        let chunk = json!({
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": {
                                    "tool_calls": [{
                                        "index": 0,
                                        "function": { "arguments": partial }
                                    }]
                                },
                                "finish_reason": null,
                            }]
                        });
                        Ok(Some(serde_json::to_string(&chunk)?))
                    }
                    // signature_delta: skip (opaque verification, not useful to consumers)
                    Some("signature_delta") => Ok(None),
                    _ => Ok(None),
                }
            }
            "content_block_start" => {
                if let Some(cb) = parsed.get("content_block") {
                    if cb.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        let chunk = json!({
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": {
                                    "tool_calls": [{
                                        "index": 0,
                                        "id": cb.get("id").cloned().unwrap_or(json!("")),
                                        "type": "function",
                                        "function": {
                                            "name": cb.get("name").cloned().unwrap_or(json!("")),
                                            "arguments": ""
                                        }
                                    }]
                                },
                                "finish_reason": null,
                            }]
                        });
                        return Ok(Some(serde_json::to_string(&chunk)?));
                    }
                }
                Ok(None)
            }
            "message_delta" => {
                let stop = parsed
                    .pointer("/delta/stop_reason")
                    .and_then(|r| r.as_str())
                    .map(|r| match r {
                        "end_turn" => "stop",
                        "max_tokens" => "length",
                        "tool_use" => "tool_calls",
                        other => other,
                    });

                if let Some(reason) = stop {
                    let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
                    let chunk = json!({
                        "object": "chat.completion.chunk",
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": {},
                            "finish_reason": reason,
                        }],
                        "usage": {
                            "prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
                            "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)),
                        }
                    });
                    Ok(Some(serde_json::to_string(&chunk)?))
                } else {
                    Ok(None)
                }
            }
            "message_stop" | "ping" => Ok(None),
            _ => Ok(None),
        }
    }
}

use crate::error::{Result, ShimError};
use crate::provider::{Provider, ProviderRequest};
use crate::vision;
use serde_json::{json, Value};

pub struct OpenAi {
    pub api_key: String,
    pub base_url: String,
}

impl OpenAi {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }

    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }
}

/// Sanitize messages for OpenAI Responses API: strip cross-provider fields,
/// translate images, and convert tool call/result messages to Responses API format.
///
/// The Responses API expects:
/// - Assistant messages with tool_calls → split into the assistant message +
///   separate `function_call` items
/// - `role: "tool"` messages → `function_call_output` items
fn sanitize_messages(messages: &[Value]) -> Vec<Value> {
    let mut result = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

        match role {
            "assistant" => {
                // Emit the assistant message (content part).
                let mut out = msg.clone();
                if let Some(obj) = out.as_object_mut() {
                    obj.remove("reasoning_content");
                    obj.remove("annotations");
                    obj.remove("refusal");
                    obj.remove("tool_calls"); // Handled separately below.
                }
                if let Some(content) = out.get("content").cloned() {
                    if content.is_array() {
                        let translated =
                            vision::translate_content_blocks(&content, vision::to_openai);
                        out["content"] = vision::text_blocks_to_openai(&translated);
                    }
                }
                // Only emit the assistant message if it has non-empty content.
                let has_content = out
                    .get("content")
                    .map(|c| !c.is_null() && c.as_str().map(|s| !s.is_empty()).unwrap_or(true))
                    .unwrap_or(false);
                if has_content {
                    result.push(out);
                }

                // Emit function_call items for each tool call.
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|tc| tc.as_array()) {
                    for tc in tool_calls {
                        let call_id = tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("{}")
                            .to_string();
                        result.push(json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                        }));
                    }
                }
            }
            "tool" => {
                // Convert to function_call_output for Responses API.
                let call_id = msg
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let output = msg
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                result.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            }
            _ => {
                // user, system, developer — standard sanitization.
                let mut out = msg.clone();
                if let Some(obj) = out.as_object_mut() {
                    obj.remove("reasoning_content");
                    obj.remove("annotations");
                    obj.remove("refusal");
                }
                if let Some(content) = out.get("content").cloned() {
                    if content.is_array() {
                        let translated =
                            vision::translate_content_blocks(&content, vision::to_openai);
                        out["content"] = vision::text_blocks_to_openai(&translated);
                    }
                }
                result.push(out);
            }
        }
    }
    result
}

/// Translate Chat Completions tool definitions to Responses API format.
/// Chat Completions: `{"type": "function", "function": {"name": ..., "description": ..., "parameters": ...}}`
/// Responses API:    `{"type": "function", "name": ..., "description": ..., "parameters": ...}`
fn translate_tools(tools: &Value) -> Value {
    if let Some(arr) = tools.as_array() {
        let translated: Vec<Value> = arr
            .iter()
            .map(|tool| {
                // If it has a nested "function" object, flatten it.
                if let Some(func) = tool.get("function") {
                    let mut flat = json!({"type": "function"});
                    if let Some(obj) = func.as_object() {
                        for (k, v) in obj {
                            flat[k] = v.clone();
                        }
                    }
                    // Preserve any extra top-level fields besides "type" and "function"
                    if let Some(obj) = tool.as_object() {
                        for (k, v) in obj {
                            if k != "type" && k != "function" {
                                flat[k] = v.clone();
                            }
                        }
                    }
                    flat
                } else {
                    // Already in flat format, pass through.
                    tool.clone()
                }
            })
            .collect();
        json!(translated)
    } else {
        tools.clone()
    }
}

/// Translate Anthropic-style tool_choice to OpenAI format.
fn translate_tool_choice(tc: &Value) -> Value {
    if let Some(tc_obj) = tc.as_object() {
        if let Some(tc_type) = tc_obj.get("type").and_then(|t| t.as_str()) {
            return match tc_type {
                "auto" => json!("auto"),
                "any" => json!("required"),
                "none" => json!("none"),
                "tool" => {
                    if let Some(name) = tc_obj.get("name") {
                        json!({"type": "function", "function": {"name": name}})
                    } else {
                        tc.clone()
                    }
                }
                _ => tc.clone(),
            };
        }
    }
    tc.clone()
}

impl Provider for OpenAi {
    fn name(&self) -> &str {
        "openai"
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        let obj = request.as_object().ok_or(ShimError::MissingModel)?;

        let messages = obj
            .get("messages")
            .and_then(|m| m.as_array())
            .ok_or(ShimError::MissingModel)?;

        let clean_messages = sanitize_messages(messages);

        // Build Responses API request
        let mut body = json!({
            "model": model,
            "input": clean_messages,
        });
        let body_obj = body.as_object_mut().unwrap();

        // max_output_tokens (Responses API name)
        if let Some(v) = obj.get("max_tokens").or(obj.get("max_completion_tokens")) {
            body_obj.insert("max_output_tokens".to_string(), v.clone());
        }

        // Build reasoning config
        let effort = obj
            .get("reasoning_effort")
            .and_then(|e| e.as_str())
            .or_else(|| {
                obj.get("output_config")
                    .and_then(|oc| oc.get("effort"))
                    .and_then(|e| e.as_str())
            });

        let mut reasoning = json!({
            "effort": effort.unwrap_or("high"),
            "summary": "auto",
        });

        // If explicit reasoning_effort or effort is "none", disable summary
        if effort == Some("none") {
            reasoning["summary"] = json!(null);
        }

        body_obj.insert("reasoning".to_string(), reasoning);

        // Stream flag
        if let Some(v) = obj.get("stream") {
            body_obj.insert("stream".to_string(), v.clone());
        }

        // Tools — translate Chat Completions format to Responses API flat format
        if let Some(tools) = obj.get("tools") {
            body_obj.insert("tools".to_string(), translate_tools(tools));
        }

        // tool_choice
        if let Some(tc) = obj.get("tool_choice") {
            body_obj.insert("tool_choice".to_string(), translate_tool_choice(tc));
        }

        // System instruction via "instructions" field
        // Also map system/developer messages out of input into instructions
        let input = body_obj.get_mut("input").unwrap().as_array_mut().unwrap();
        let mut instructions: Vec<String> = Vec::new();
        input.retain(|msg| match msg.get("role").and_then(|r| r.as_str()) {
            Some("system" | "developer") => {
                if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                    instructions.push(text.to_string());
                }
                false
            }
            _ => true,
        });
        if !instructions.is_empty() {
            body_obj.insert("instructions".to_string(), json!(instructions.join("\n\n")));
        }

        // Strip x-* namespaces and provider-specific params from body
        body_obj.remove("thinking");

        let url = format!("{}/responses", self.base_url);

        Ok(ProviderRequest {
            url,
            headers: vec![
                ("Authorization".into(), format!("Bearer {}", self.api_key)),
                ("Content-Type".into(), "application/json".into()),
            ],
            body,
        })
    }

    fn transform_response(&self, model: &str, response: Value) -> Result<Value> {
        // Check for error (Responses API returns "error": null on success)
        if let Some(err) = response.get("error") {
            if !err.is_null() {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error");
                return Err(ShimError::ProviderError {
                    status: 400,
                    body: msg.to_string(),
                });
            }
        }

        let output = response
            .get("output")
            .and_then(|o| o.as_array())
            .ok_or_else(|| ShimError::ProviderError {
                status: 500,
                body: "no output in response".to_string(),
            })?;

        // Extract reasoning summary
        let mut reasoning_content: Option<String> = None;
        let mut text_content: Option<String> = None;
        let mut tool_calls: Vec<Value> = Vec::new();

        for item in output {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("reasoning") => {
                    if let Some(summary) = item.get("summary").and_then(|s| s.as_array()) {
                        let texts: Vec<&str> = summary
                            .iter()
                            .filter_map(|s| s.get("text").and_then(|t| t.as_str()))
                            .collect();
                        if !texts.is_empty() {
                            reasoning_content = Some(texts.join("\n"));
                        }
                    }
                }
                Some("message") => {
                    if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                        for part in content {
                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                text_content = Some(text.to_string());
                            }
                        }
                    }
                }
                Some("function_call") => {
                    tool_calls.push(json!({
                        "id": item.get("call_id").cloned().unwrap_or(json!("")),
                        "type": "function",
                        "function": {
                            "name": item.get("name").cloned().unwrap_or(json!("")),
                            "arguments": item.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}"),
                        }
                    }));
                }
                _ => {}
            }
        }

        let content = text_content.map(|t| json!(t)).unwrap_or(Value::Null);

        let mut message = json!({
            "role": "assistant",
            "content": content,
        });
        if !tool_calls.is_empty() {
            message["tool_calls"] = json!(tool_calls);
        }
        if let Some(reasoning) = reasoning_content {
            message["reasoning_content"] = json!(reasoning);
        }

        let status = response
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("completed");
        let finish_reason = match status {
            "completed" => "stop",
            "incomplete" => "length",
            _ => "stop",
        };

        let usage = response.get("usage").cloned().unwrap_or(json!({}));

        Ok(json!({
            "id": response.get("id").cloned().unwrap_or(json!("")),
            "object": "chat.completion",
            "model": model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish_reason,
            }],
            "usage": {
                "prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
                "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)),
                "total_tokens": usage.get("total_tokens").cloned().unwrap_or(json!(0)),
            }
        }))
    }

    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        let trimmed = chunk.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return Ok(None);
        }

        let parsed: Value = serde_json::from_str(trimmed)?;
        let event_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            // Reasoning summary deltas → reasoning_content
            "response.reasoning_summary_text.delta" => {
                let delta = parsed.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return Ok(None);
                }
                let chunk = json!({
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {"reasoning_content": delta},
                        "finish_reason": null,
                    }]
                });
                Ok(Some(serde_json::to_string(&chunk)?))
            }

            // Content text deltas
            "response.output_text.delta" => {
                let delta = parsed.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return Ok(None);
                }
                let chunk = json!({
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {"content": delta},
                        "finish_reason": null,
                    }]
                });
                Ok(Some(serde_json::to_string(&chunk)?))
            }

            // Function call output item added — emit tool_calls start chunk
            "response.output_item.added" => {
                let empty = json!({});
                let item = parsed.get("item").unwrap_or(&empty);
                if item.get("type").and_then(|t| t.as_str()) != Some("function_call") {
                    return Ok(None);
                }
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let index = parsed
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let chunk = json!({
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "id": call_id,
                                "type": "function",
                                "function": {"name": name, "arguments": ""},
                            }]
                        },
                        "finish_reason": null,
                    }]
                });
                Ok(Some(serde_json::to_string(&chunk)?))
            }

            // Function call argument deltas
            "response.function_call_arguments.delta" => {
                let delta = parsed.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return Ok(None);
                }
                let index = parsed
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let chunk = json!({
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": index,
                                "function": {"arguments": delta},
                            }]
                        },
                        "finish_reason": null,
                    }]
                });
                Ok(Some(serde_json::to_string(&chunk)?))
            }

            // Response completed — emit finish
            "response.completed" => {
                let resp = &parsed["response"];
                let status = resp
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("completed");
                let finish_reason = match status {
                    "completed" => "stop",
                    "incomplete" => "length",
                    _ => "stop",
                };
                let usage = resp.get("usage").cloned().unwrap_or(json!({}));
                let reasoning_tokens = usage
                    .pointer("/output_tokens_details/reasoning_tokens")
                    .cloned()
                    .unwrap_or(json!(0));
                let chunk = json!({
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": finish_reason,
                    }],
                    "usage": {
                        "prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
                        "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)),
                        "reasoning_tokens": reasoning_tokens,
                    }
                });
                Ok(Some(serde_json::to_string(&chunk)?))
            }

            // All other events: skip
            _ => Ok(None),
        }
    }
}

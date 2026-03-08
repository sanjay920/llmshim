use crate::error::{Result, ShimError};
use crate::provider::{Provider, ProviderRequest};
use crate::vision;
use serde_json::{json, Value};

pub struct Xai {
    pub api_key: String,
    pub base_url: String,
}

impl Xai {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: "https://api.x.ai/v1".to_string(),
        }
    }

    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }
}

/// Sanitize messages: strip cross-provider fields and translate images.
fn sanitize_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| {
            let mut out = msg.clone();
            if let Some(obj) = out.as_object_mut() {
                obj.remove("reasoning_content");
                obj.remove("annotations");
                obj.remove("refusal");
            }
            // Translate image content blocks to OpenAI Responses API format (same as xAI)
            if let Some(content) = out.get("content").cloned() {
                if content.is_array() {
                    out["content"] = vision::translate_content_blocks(&content, vision::to_openai);
                }
            }
            out
        })
        .collect()
}

/// Translate tool_choice from Anthropic format if needed.
fn translate_tool_choice(tc: &Value) -> Value {
    if let Some(tc_obj) = tc.as_object() {
        if let Some(tc_type) = tc_obj.get("type").and_then(|t| t.as_str()) {
            return match tc_type {
                "auto" => json!("auto"),
                "any" => json!("required"),
                "none" => json!("none"),
                "tool" => tc_obj
                    .get("name")
                    .map(|name| json!({"type": "function", "function": {"name": name}}))
                    .unwrap_or_else(|| tc.clone()),
                _ => tc.clone(),
            };
        }
    }
    tc.clone()
}

impl Provider for Xai {
    fn name(&self) -> &str {
        "xai"
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        let obj = request.as_object().ok_or(ShimError::MissingModel)?;

        let messages = obj
            .get("messages")
            .and_then(|m| m.as_array())
            .ok_or(ShimError::MissingModel)?;

        let clean_messages = sanitize_messages(messages);

        // Use Responses API (same as OpenAI)
        let mut body = json!({
            "model": model,
            "input": clean_messages,
        });
        let body_obj = body.as_object_mut().unwrap();

        // max_output_tokens
        if let Some(v) = obj.get("max_tokens").or(obj.get("max_completion_tokens")) {
            body_obj.insert("max_output_tokens".to_string(), v.clone());
        }

        // Stream flag
        if let Some(v) = obj.get("stream") {
            body_obj.insert("stream".to_string(), v.clone());
        }

        // Tools
        if let Some(tools) = obj.get("tools") {
            body_obj.insert("tools".to_string(), tools.clone());
        }

        // tool_choice
        if let Some(tc) = obj.get("tool_choice") {
            body_obj.insert("tool_choice".to_string(), translate_tool_choice(tc));
        }

        // Extract system/developer messages into instructions
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

        // Strip provider-specific params
        body_obj.remove("thinking");
        body_obj.remove("output_config");
        body_obj.remove("reasoning_effort");

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

        let mut text_content: Option<String> = None;
        let mut tool_calls: Vec<Value> = Vec::new();

        for item in output {
            match item.get("type").and_then(|t| t.as_str()) {
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
                // "reasoning" type — no visible summary for Grok, skip
                _ => {}
            }
        }

        let content = text_content.map(|t| json!(t)).unwrap_or(Value::Null);
        let mut message = json!({"role": "assistant", "content": content});
        if !tool_calls.is_empty() {
            message["tool_calls"] = json!(tool_calls);
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
        let reasoning_tokens = usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let mut result = json!({
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
        });

        // Surface reasoning token count so users know reasoning happened
        if reasoning_tokens > 0 {
            result["usage"]["reasoning_tokens"] = json!(reasoning_tokens);
        }

        Ok(result)
    }

    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        let trimmed = chunk.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return Ok(None);
        }

        let parsed: Value = serde_json::from_str(trimmed)?;
        let event_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
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

            // Response completed
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

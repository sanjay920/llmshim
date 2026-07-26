use crate::error::{Result, ShimError};
use crate::provider::{Provider, ProviderRequest};
use crate::vision;
use serde_json::{json, Value};

/// OpenRouter (https://openrouter.ai) — an OpenAI Chat Completions-compatible
/// aggregator. Unlike the other providers, which translate *away* from Chat
/// Completions to a native dialect, OpenRouter *is* Chat Completions, so this is
/// a near-passthrough: messages, tools, vision, and `response_format` are
/// already in the target shape and are forwarded unchanged. Model slugs
/// (`vendor/model`, e.g. `anthropic/claude-sonnet-4.5`) contain a slash and
/// arrive here intact because the router splits only on the first `/`.
pub struct OpenRouter {
    pub api_key: String,
    pub base_url: String,
}

impl OpenRouter {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: "https://openrouter.ai/api/v1".to_string(),
        }
    }

    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }
}

/// OpenRouter's reasoning-effort vocabulary is a superset of the unified one, so
/// the mapping is 1:1 (no per-model clamp — OpenRouter enforces the underlying
/// model's limits itself). `reasoning_mode: "pro"` bumps one tier, mirroring the
/// other providers; explicit `none` always wins.
fn normalize_openrouter_effort(effort: &str, pro: bool) -> &'static str {
    let base = match effort {
        "none" => "none",
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" => "max",
        _ => "medium",
    };
    if !pro {
        return base;
    }
    match base {
        "none" => "none",
        "minimal" => "low",
        "low" => "medium",
        "medium" => "high",
        "high" => "xhigh",
        _ => "max", // xhigh, max
    }
}

/// Sanitize messages for OpenRouter (OpenAI Chat Completions native). Strip
/// llmshim-normalized / foreign-provider fields so multi-model conversations
/// don't leak them, and normalize vision blocks to OpenAI form. Messages,
/// `tool_calls`, and `role: "tool"` all stay in Chat Completions shape.
fn sanitize_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| {
            let mut out = msg.clone();
            if let Some(obj) = out.as_object_mut() {
                obj.remove("reasoning_content"); // llmshim-normalized; OpenRouter uses reasoning_details
                obj.remove("reasoning_signature"); // opaque Anthropic token — never forward
                obj.remove("redacted_reasoning_content"); // opaque Anthropic token — never forward
                obj.remove("annotations");
                obj.remove("refusal");
            }
            if let Some(content) = out.get("content").cloned() {
                if content.is_array() {
                    // Normalize any image format to Chat Completions `image_url`,
                    // and any Responses-style `input_text` back to `text`.
                    let translated =
                        vision::translate_content_blocks(&content, vision::to_openai_chat);
                    out["content"] = normalize_text_blocks_to_chat(&translated);
                }
            }
            out
        })
        .collect()
}

/// Convert Responses-style `input_text` blocks back to Chat Completions `text`.
/// (llmshim's canonical input already uses `text`; this only matters when a
/// caller mixes in Responses-format blocks.)
fn normalize_text_blocks_to_chat(content: &Value) -> Value {
    match content {
        Value::Array(blocks) => Value::Array(
            blocks
                .iter()
                .map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) == Some("input_text") {
                        let mut out = b.clone();
                        out["type"] = json!("text");
                        out
                    } else {
                        b.clone()
                    }
                })
                .collect(),
        ),
        _ => content.clone(),
    }
}

impl Provider for OpenRouter {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        let obj = request.as_object().ok_or(ShimError::MissingModel)?;
        let messages = obj
            .get("messages")
            .and_then(|m| m.as_array())
            .ok_or(ShimError::MissingModel)?;

        let mut body = json!({
            "model": model,
            "messages": sanitize_messages(messages),
        });
        let body_obj = body.as_object_mut().unwrap();

        // Standard Chat Completions params — forwarded unchanged (OpenRouter is
        // the target format, so tools/response_format need no translation).
        for key in [
            "max_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
            "top_k",
            "frequency_penalty",
            "presence_penalty",
            "stop",
            "seed",
            "stream",
            "stream_options",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "response_format",
            "logprobs",
            "top_logprobs",
            "n",
        ] {
            if let Some(v) = obj.get(key) {
                body_obj.insert(key.to_string(), v.clone());
            }
        }

        // Unified reasoning -> OpenRouter `reasoning: {effort}`. A native
        // `x-openrouter.reasoning` object takes precedence and is copied below.
        let has_native_reasoning = obj
            .get("x-openrouter")
            .and_then(|x| x.get("reasoning"))
            .is_some();
        if !has_native_reasoning {
            if let Some(effort) = obj.get("reasoning_effort").and_then(|e| e.as_str()) {
                let pro = obj.get("reasoning_mode").and_then(|m| m.as_str()) == Some("pro");
                let effort = normalize_openrouter_effort(effort, pro);
                body_obj.insert("reasoning".to_string(), json!({ "effort": effort }));
            }
        }

        let mut headers = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.api_key),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];

        // x-openrouter namespace: OpenRouter-only controls. Body params
        // (provider, models, transforms, route, reasoning) are copied into the
        // body; the two attribution headers are copied to headers, not the body.
        if let Some(ext) = obj.get("x-openrouter").and_then(|e| e.as_object()) {
            for (k, v) in ext {
                match k.as_str() {
                    "http_referer" | "HTTP-Referer" => {
                        if let Some(s) = v.as_str() {
                            headers.push(("HTTP-Referer".to_string(), s.to_string()));
                        }
                    }
                    "x_title" | "X-Title" => {
                        if let Some(s) = v.as_str() {
                            headers.push(("X-Title".to_string(), s.to_string()));
                        }
                    }
                    _ => {
                        body_obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        // OpenRouter enables the `middle-out` transform by default, which
        // silently drops middle messages to fit the context window. For a
        // faithful passthrough we disable it; callers opt back in via
        // `x-openrouter.transforms`.
        if !body_obj.contains_key("transforms") {
            body_obj.insert("transforms".to_string(), json!([]));
        }

        let url = format!("{}/chat/completions", self.base_url);
        Ok(ProviderRequest { url, headers, body })
    }

    fn transform_response(&self, _model: &str, mut response: Value) -> Result<Value> {
        // Non-stream errors usually surface via HTTP status, but a body-level
        // `error` object can also appear — turn it into a ProviderError.
        if let Some(err) = response.get("error") {
            if !err.is_null() {
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
                let status = err.get("code").and_then(|c| c.as_u64()).unwrap_or(400) as u16;
                return Err(ShimError::ProviderError {
                    status,
                    body: message,
                });
            }
        }

        // The response is already Chat Completions-shaped. Normalize OpenRouter's
        // `message.reasoning` into llmshim's `reasoning_content` convention.
        if let Some(choices) = response.get_mut("choices").and_then(|c| c.as_array_mut()) {
            for choice in choices {
                if let Some(msg) = choice.get_mut("message").and_then(|m| m.as_object_mut()) {
                    if !msg.contains_key("reasoning_content") {
                        if let Some(r) = msg
                            .get("reasoning")
                            .and_then(|r| r.as_str())
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                        {
                            msg.insert("reasoning_content".to_string(), json!(r));
                        }
                    }
                }
            }
        }

        Ok(response)
    }

    fn transform_stream_chunk(&self, _model: &str, chunk: &str) -> Result<Option<String>> {
        // OpenRouter's SSE chunks are OpenAI-delta shaped. Parse, normalize the
        // reasoning delta, and forward. Unparseable payloads (e.g. stray
        // keepalive text) are skipped. `data: [DONE]` and `:`-comment keepalives
        // are already handled by the client's SSE reader.
        let mut parsed: Value = match serde_json::from_str(chunk) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };

        if let Some(choices) = parsed.get_mut("choices").and_then(|c| c.as_array_mut()) {
            for choice in choices {
                if let Some(delta) = choice.get_mut("delta").and_then(|d| d.as_object_mut()) {
                    if !delta.contains_key("reasoning_content") {
                        if let Some(r) = delta
                            .get("reasoning")
                            .and_then(|r| r.as_str())
                            .map(str::to_string)
                        {
                            delta.insert("reasoning_content".to_string(), json!(r));
                        }
                    }
                }
            }
        }

        Ok(Some(serde_json::to_string(&parsed)?))
    }
}

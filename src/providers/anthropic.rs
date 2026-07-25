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

    /// Newer Claude families (Opus 4.7/4.8, Sonnet 5) that reject the pre-4.6
    /// `thinking.type=enabled` path. Verified against the live API: these
    /// models return HTTP 400 for enabled-thinking and 200 for adaptive.
    fn is_new_adaptive_family(model: &str) -> bool {
        let m = model.to_lowercase();
        m.contains("opus-4-7")
            || m.contains("opus-4.7")
            || m.contains("opus-4-8")
            || m.contains("opus-4.8")
            || m.contains("opus-5")
            || m.contains("sonnet-5")
    }

    /// Models that must use the adaptive thinking path
    /// (`thinking:{type:"adaptive"}` + `output_config:{effort}`) rather than the
    /// pre-4.6 `thinking:{type:"enabled", budget_tokens}` path. Covers Claude 4.6
    /// and the newer Opus 4.7/4.8 and Sonnet 5 families.
    fn uses_adaptive_thinking(model: &str) -> bool {
        Self::is_claude_4_6(model) || Self::is_new_adaptive_family(model)
    }

    /// Models that support the 1M context window beta.
    /// Opus 4.x, Sonnet 4.x, and Sonnet 5.
    fn supports_1m_context(model: &str) -> bool {
        let m = model.to_lowercase();
        m.contains("opus-4") || m.contains("sonnet-4") || m.contains("sonnet-5")
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
            || m.contains("claude-sonnet-5")
            || Self::uses_adaptive_thinking(&m)
    }
}

// -- Request transformation helpers --

fn normalize_anthropic_content_blocks(content: &Value) -> Value {
    let mut translated = vision::translate_content_blocks(content, vision::to_anthropic);
    if let Some(blocks) = translated.as_array_mut() {
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("input_text") {
                block["type"] = json!("text");
            }
        }
    }
    translated
}

fn uses_extended_cache_ttl(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object
                .get("cache_control")
                .and_then(|cache_control| cache_control.get("ttl"))
                .and_then(Value::as_str)
                == Some("1h")
                || object.values().any(uses_extended_cache_ttl)
        }
        Value::Array(items) => items.iter().any(uses_extended_cache_ttl),
        _ => false,
    }
}

fn text_block(text: &str) -> Value {
    json!({
        "type": "text",
        "text": text,
    })
}

fn extract_system_message(messages: &[Value]) -> (Option<Value>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut system_blocks: Vec<Value> = Vec::new();
    let mut has_block_content = false;
    let mut rest: Vec<Value> = Vec::new();

    for msg in messages {
        match msg.get("role").and_then(|r| r.as_str()) {
            Some("system" | "developer") => {
                if let Some(content) = msg.get("content") {
                    match content {
                        Value::String(text) if !has_block_content => {
                            system_parts.push(text.to_string());
                        }
                        Value::String(text) => {
                            system_blocks.push(text_block(text));
                        }
                        Value::Array(blocks) => {
                            if !system_parts.is_empty() {
                                system_blocks
                                    .extend(system_parts.drain(..).map(|text| text_block(&text)));
                            }
                            has_block_content = true;
                            if let Value::Array(normalized) =
                                normalize_anthropic_content_blocks(&Value::Array(blocks.clone()))
                            {
                                system_blocks.extend(normalized);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => rest.push(msg.clone()),
        }
    }

    let system = if has_block_content {
        if system_blocks.is_empty() {
            None
        } else {
            Some(Value::Array(system_blocks))
        }
    } else if system_parts.is_empty() {
        None
    } else {
        Some(Value::String(system_parts.join("\n\n")))
    };
    (system, rest)
}

fn transform_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| {
            let mut out = msg.clone();

            // Capture normalized reasoning fields before sanitizing, so an
            // assistant turn's thinking block can be reconstructed losslessly
            // below (symmetric to the tool-call thought_signature round-trip).
            let role = out.get("role").and_then(|r| r.as_str()).map(str::to_string);
            let reasoning_content = out
                .get("reasoning_content")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let reasoning_signature = out
                .get("reasoning_signature")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let redacted_reasoning = out
                .get("redacted_reasoning_content")
                .and_then(|v| v.as_str())
                .map(str::to_string);

            // Sanitize cross-provider fields that Anthropic's API rejects.
            // This enables multi-model conversations (e.g., Cursor-style provider switching).
            if let Some(obj) = out.as_object_mut() {
                obj.remove("reasoning_content"); // our normalized thinking field
                obj.remove("reasoning_signature"); // reconstructed into a thinking block below
                obj.remove("redacted_reasoning_content"); // reconstructed into redacted_thinking below
                obj.remove("annotations"); // OpenAI returns this on every message
                obj.remove("refusal"); // OpenAI safety refusal field
                obj.remove("audio"); // OpenAI audio response field
                obj.remove("logprobs"); // OpenAI logprobs on message
            }

            // Translate image content blocks from OpenAI format to Anthropic format
            if let Some(content) = out.get("content").cloned() {
                if content.is_array() {
                    out["content"] = normalize_anthropic_content_blocks(&content);
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

                let mut tool_result = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                });
                if let Some(cache_control) = out.get("cache_control") {
                    tool_result["cache_control"] = cache_control.clone();
                }

                out = json!({
                    "role": "user",
                    "content": [tool_result]
                });
            }

            // Reconstruct thinking block(s) as the FIRST content block(s) of an
            // assistant turn so extended-thinking + tool-use continuations are
            // accepted (the API requires thinking before text/tool_use). Only when
            // we hold the opaque token — a thinking block without its signature is
            // rejected, so absent a signature we leave it stripped (no regression).
            if role.as_deref() == Some("assistant") {
                let mut thinking_blocks: Vec<Value> = Vec::new();
                if let (Some(text), Some(sig)) = (&reasoning_content, &reasoning_signature) {
                    thinking_blocks.push(json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": sig,
                    }));
                }
                if let Some(data) = &redacted_reasoning {
                    thinking_blocks.push(json!({
                        "type": "redacted_thinking",
                        "data": data,
                    }));
                }
                if !thinking_blocks.is_empty() {
                    match out.get("content").cloned() {
                        Some(Value::Array(arr)) => thinking_blocks.extend(arr),
                        Some(Value::String(s)) if !s.is_empty() => {
                            thinking_blocks.push(json!({"type": "text", "text": s}))
                        }
                        _ => {}
                    }
                    out["content"] = json!(thinking_blocks);
                }
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
            let mut out = json!({
                "name": func.get("name")?,
                "description": func.get("description").unwrap_or(&json!("")),
                "input_schema": func.get("parameters").unwrap_or(&json!({"type": "object", "properties": {}})),
            });
            if let Some(cache_control) = tool.get("cache_control") {
                out["cache_control"] = cache_control.clone();
            }
            Some(out)
        })
        .collect()
}

fn normalized_anthropic_usage(usage: &Value) -> Value {
    let input = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cache_creation = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let mut normalized = json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input + output + cache_read + cache_creation,
    });

    if cache_read > 0 {
        normalized["cache_read_input_tokens"] = json!(cache_read);
        normalized["prompt_tokens_details"] = json!({
            "cached_tokens": cache_read,
        });
    }
    if cache_creation > 0 {
        normalized["cache_creation_input_tokens"] = json!(cache_creation);
    }
    if let Some(cache_creation_detail) = usage.get("cache_creation") {
        normalized["cache_creation"] = cache_creation_detail.clone();
    }

    normalized
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
    // Opaque signature + redacted data so reasoning can round-trip losslessly
    // (see transform_messages reconstruction). Symmetric to tool thought_signature.
    let mut thinking_signature: Option<String> = None;
    let mut redacted_thinking: Option<String> = None;

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
                if let Some(s) = block.get("signature").and_then(|s| s.as_str()) {
                    thinking_signature = Some(s.to_string());
                }
            }
            Some("redacted_thinking") => {
                if let Some(d) = block.get("data").and_then(|d| d.as_str()) {
                    redacted_thinking = Some(d.to_string());
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
    let normalized_usage = normalized_anthropic_usage(&usage);

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
    // Surface the opaque signature + redacted data so the reasoning block can be
    // echoed back losslessly on a follow-up request (see transform_messages).
    if let Some(sig) = thinking_signature {
        message["reasoning_signature"] = json!(sig);
    }
    if let Some(data) = redacted_thinking {
        message["redacted_reasoning_content"] = json!(data);
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
        "usage": normalized_usage
    })
}

/// Normalize a unified reasoning effort (`none|low|medium|high|xhigh|max`,
/// legacy `minimal` accepted) and apply the mode:"pro" one-tier bump.
/// Unknown values fall back to "medium" (the pre-existing default).
fn normalize_unified_effort(effort: &str, pro: bool) -> &'static str {
    let base = match effort {
        "none" => "none",
        "minimal" | "low" => "low",
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
        // Explicit "none" wins even in pro mode.
        "none" => "none",
        "low" => "medium",
        "medium" => "high",
        "high" => "xhigh",
        _ => "max", // xhigh, max
    }
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
            body_obj.insert("system".to_string(), sys);
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
                // Skip control flags that are handled elsewhere (not API body params)
                if k == "disable_1m_context" || k == "extra_betas" {
                    continue;
                }
                body_obj.insert(k.clone(), v.clone());
            }
        }
        if let Some(cache_control) = obj.get("cache_control") {
            body_obj.insert("cache_control".to_string(), cache_control.clone());
        }

        // -- Thinking / reasoning support --
        let has_thinking = obj.contains_key("thinking")
            || obj
                .get("x-anthropic")
                .and_then(|x| x.get("thinking"))
                .is_some();

        // Handle unified reasoning controls (reasoning_effort + reasoning_mode)
        // -> Anthropic thinking translation. Explicit thinking config always wins.
        if let Some(effort) = obj.get("reasoning_effort").and_then(|e| e.as_str()) {
            if Self::supports_thinking(model) && !has_thinking {
                // Anthropic has no request-level standard/pro mode; map the
                // unified mode:"pro" to a one-tier effort bump (docs/src/guides/reasoning.md).
                let pro = obj
                    .get("reasoning_mode")
                    .and_then(|m| m.as_str())
                    .map(|m| m == "pro")
                    .unwrap_or(false);
                let effort = normalize_unified_effort(effort, pro);

                // Reasoning-summary visibility. Newer models (Sonnet 5, Opus
                // 4.7/4.8, ...) default `display` to "omitted" — a signed but
                // empty thinking block with no thinking_delta text. Default to
                // "summarized" so reasoning text is returned consistently across
                // model generations; a latency-sensitive caller opts back into
                // "omitted" via reasoning_summary. Verified live 2026-07-17.
                let display = match obj.get("reasoning_summary").and_then(|v| v.as_str()) {
                    Some("none") | Some("omitted") => "omitted",
                    _ => "summarized",
                };

                if effort == "none" {
                    if Self::uses_adaptive_thinking(model) {
                        // Adaptive models think by default even with no config;
                        // "disabled" is the only true zero-thinking request
                        // (verified live on sonnet-5, opus-4-8, sonnet-4-6).
                        body_obj.insert("thinking".to_string(), json!({"type": "disabled"}));
                    }
                    // Pre-4.6/Haiku: thinking is opt-in; omitting the key IS "none".
                } else if Self::uses_adaptive_thinking(model) {
                    body_obj.insert(
                        "thinking".to_string(),
                        json!({"type": "adaptive", "display": display}),
                    );
                    // Opus/Sonnet 4.6 reject "xhigh" (their tiers: low/medium/high/max);
                    // Opus 4.7/4.8 + Sonnet 5 accept the full low..max range (verified).
                    let anthropic_effort =
                        if effort == "xhigh" && !Self::is_new_adaptive_family(model) {
                            "max"
                        } else {
                            effort
                        };
                    body_obj.insert(
                        "output_config".to_string(),
                        json!({"effort": anthropic_effort}),
                    );
                } else {
                    // Pre-4.6: enabled thinking with a budget scaled to effort.
                    // Six monotonic tiers over the max_tokens budget.
                    let max_tokens = body_obj
                        .get("max_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(8192);
                    let budget = match effort {
                        "low" => max_tokens / 4,
                        "medium" => max_tokens / 2,
                        "high" => max_tokens * 3 / 4,
                        "xhigh" => max_tokens * 9 / 10,
                        _ => max_tokens.saturating_sub(1), // "max"
                    };
                    let budget = budget.max(1024); // Anthropic minimum
                    body_obj.insert(
                        "thinking".to_string(),
                        json!({
                            "type": "enabled",
                            "budget_tokens": budget,
                            "display": display
                        }),
                    );
                }
            }
        }

        // Pass through top-level thinking / output_config if user provided them
        // directly. This runs BEFORE the constraint check below so passthrough
        // thinking also gets the temperature/top_k strip (previously it ran
        // after, leaving temperature set -> upstream 400).
        if let Some(thinking) = obj.get("thinking") {
            if !body_obj.contains_key("thinking") {
                body_obj.insert("thinking".to_string(), thinking.clone());
            }
        }
        if let Some(output_config) = obj.get("output_config") {
            if !body_obj.contains_key("output_config") {
                body_obj.insert("output_config".to_string(), output_config.clone());
            }
        }

        // Thinking requires temperature=1: strip custom temperature/top_k
        // whenever thinking is active, however it was configured.
        if body_obj.contains_key("thinking") {
            let thinking_type = body_obj
                .get("thinking")
                .and_then(|t| t.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if thinking_type == "enabled" || thinking_type == "adaptive" {
                body_obj.remove("temperature");
                body_obj.remove("top_k");
            }
        }

        // Fast mode support: extract "speed" from the request and apply
        // Anthropic-specific transformations (body field + beta header).
        let speed = obj.get("speed").and_then(|s| s.as_str()).map(String::from);
        if let Some(ref s) = speed {
            body_obj.insert("speed".to_string(), json!(s));
        }

        let url = format!("{}/messages", self.base_url);

        // Build headers — include 1M context beta by default for supported models
        let mut headers = vec![
            ("x-api-key".into(), self.api_key.clone()),
            ("anthropic-version".into(), "2023-06-01".into()),
            ("content-type".into(), "application/json".into()),
        ];

        // Collect beta headers
        let mut betas: Vec<String> = Vec::new();

        // 1M context beta header — enabled by default, disable via x-anthropic
        let disable_1m = obj
            .get("x-anthropic")
            .and_then(|x| x.get("disable_1m_context"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !disable_1m && Self::supports_1m_context(model) {
            betas.push("context-1m-2025-08-07".to_string());
        }

        // Fast mode beta header
        if speed.as_deref() == Some("fast") {
            betas.push("fast-mode-2026-02-01".to_string());
        }
        if uses_extended_cache_ttl(request) {
            betas.push("extended-cache-ttl-2025-04-11".to_string());
        }

        // Caller-supplied beta tokens (e.g. Claude Code's `--betas`), passed as
        // a string array under `x-anthropic.extra_betas`. Appended to the
        // auto-managed set above; de-duplicated so an explicit request for an
        // already-enabled beta does not double it.
        if let Some(extra) = obj
            .get("x-anthropic")
            .and_then(|x| x.get("extra_betas"))
            .and_then(|v| v.as_array())
        {
            for beta in extra.iter().filter_map(|b| b.as_str()) {
                let beta = beta.trim();
                if !beta.is_empty() && !betas.iter().any(|existing| existing == beta) {
                    betas.push(beta.to_string());
                }
            }
        }

        if !betas.is_empty() {
            headers.push(("anthropic-beta".into(), betas.join(",")));
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
                        // Use the content_block index from the Anthropic event so
                        // parallel tool calls get separate indices in OpenAI format
                        let block_index = parsed.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                        let chunk = json!({
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": {
                                    "tool_calls": [{
                                        "index": block_index,
                                        "function": { "arguments": partial }
                                    }]
                                },
                                "finish_reason": null,
                            }]
                        });
                        Ok(Some(serde_json::to_string(&chunk)?))
                    }
                    // signature_delta: emit the opaque signature so a streaming
                    // consumer can reassemble a complete, round-trippable thinking
                    // block (fed back via reasoning_signature on the next request).
                    Some("signature_delta") => {
                        let signature = delta
                            .get("signature")
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        let chunk = json!({
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": { "reasoning_signature": signature },
                                "finish_reason": null,
                            }]
                        });
                        Ok(Some(serde_json::to_string(&chunk)?))
                    }
                    _ => Ok(None),
                }
            }
            "content_block_start" => {
                if let Some(cb) = parsed.get("content_block") {
                    if cb.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        // Use the content_block index from the Anthropic event
                        let block_index = parsed.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                        let chunk = json!({
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": {
                                    "tool_calls": [{
                                        "index": block_index,
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
                    // redacted_thinking arrives whole (no deltas); surface its
                    // opaque data so it can be echoed back on a later request.
                    if cb.get("type").and_then(|t| t.as_str()) == Some("redacted_thinking") {
                        if let Some(data) = cb.get("data").and_then(|d| d.as_str()) {
                            let chunk = json!({
                                "object": "chat.completion.chunk",
                                "model": model,
                                "choices": [{
                                    "index": 0,
                                    "delta": { "redacted_reasoning_content": data },
                                    "finish_reason": null,
                                }]
                            });
                            return Ok(Some(serde_json::to_string(&chunk)?));
                        }
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
                    let normalized_usage = normalized_anthropic_usage(&usage);
                    let chunk = json!({
                        "object": "chat.completion.chunk",
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": {},
                            "finish_reason": reason,
                        }],
                        "usage": normalized_usage
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

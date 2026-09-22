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

    fn is_fable(model: &str) -> bool {
        matches!(model, "claude-fable-5" | "claude-fable-5-1")
    }

    /// Models that support the 1M context window beta.
    /// Opus 4.x, Sonnet 4.x, and Sonnet 5.
    fn supports_1m_context(model: &str) -> bool {
        let m = model.to_lowercase();
        m.contains("opus-4") || m.contains("sonnet-4") || m.contains("sonnet-5")
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

fn text_block(text: &str) -> Value {
    json!({
        "type": "text",
        "text": text,
    })
}

fn extract_system_message(
    messages: &[Value],
    preserve_later_system: bool,
) -> (Option<Value>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut system_blocks: Vec<Value> = Vec::new();
    let mut has_block_content = false;
    let mut rest: Vec<Value> = Vec::new();

    for msg in messages {
        match msg.get("role").and_then(|r| r.as_str()) {
            Some("system" | "developer") => {
                // Fable 5.1 supports appended system turns. Hoisting those into
                // the initial prompt changes the prefix bound to prior thinking.
                if preserve_later_system && !rest.is_empty() {
                    let mut message = msg.clone();
                    message["role"] = json!("system");
                    rest.push(message);
                    continue;
                }
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

/// One Chat Completions message in, one Anthropic message out — a `role:
/// "tool"` result included, which becomes its own `user` message even when it
/// sits beside another.
///
/// Same-role adjacency is deliberately left alone. The Messages API accepts
/// it: its reference states that consecutive `user` or `assistant` turns in a
/// request are combined into a single turn server-side (platform.claude.com,
/// Messages API, `messages` parameter), and parallel tool results already
/// reach it here as back-to-back `user` messages. Merging locally would only
/// destroy message boundaries a caller may key on. Gemini is the one wire in
/// this crate that rejects adjacency, and it merges in its own adapter.
fn transform_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| {
            let mut out = msg.clone();

            let role = out.get("role").and_then(Value::as_str).map(str::to_owned);
            let reasoning_blocks = crate::reasoning::anthropic_blocks(&out);
            crate::reasoning::strip_fields(&mut out);

            // Sanitize cross-provider fields that Anthropic's API rejects.
            // This enables multi-model conversations (e.g., Cursor-style provider switching).
            if let Some(obj) = out.as_object_mut() {
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
                    let mut content_blocks: Vec<Value> =
                        out["content"].as_array().cloned().unwrap_or_default();

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

                        let mut block = json!({
                            "type": "tool_use",
                            "id": tc.get("id").cloned().unwrap_or(json!("")),
                            "name": func.get("name").cloned().unwrap_or(json!("")),
                            "input": input,
                        });
                        if let Some(cache) = tc.get("cache_control") {
                            block["cache_control"] = cache.clone();
                        }
                        content_blocks.push(block);
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
                if let Some(is_error) = out.get("is_error").filter(|v| v.is_boolean()) {
                    tool_result["is_error"] = is_error.clone();
                }
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
                let mut thinking_blocks = reasoning_blocks;
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
            if let Some(strict)=func.get("strict").or_else(||tool.get("strict")){out["strict"]=strict.clone();}
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

    crate::usage::normalize_cache(usage, &mut normalized);
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

fn transform_response_to_openai(model: &str, resp: &Value) -> Result<Value> {
    let content_blocks = resp
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in &content_blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(t.to_string());
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

    let stop_reason = match resp.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn" | "stop_sequence") => "stop",
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") if Anthropic::is_fable(model) || model == "claude-opus-5" => {
            "content_filter"
        }
        _ => {
            return Err(ShimError::ProviderError {
                status: 502,
                body: "Anthropic response has no supported terminal stop reason".into(),
                retry_after: None,
            })
        }
    };

    let usage = resp.get("usage").cloned().unwrap_or(json!({}));
    let normalized_usage = normalized_anthropic_usage(&usage);

    let mut message = json!({
        "role": "assistant",
        "content": content,
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    Ok(json!({
        "id": resp.get("id").cloned().unwrap_or(json!("")),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": stop_reason,
        }],
        "usage": normalized_usage
    }))
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

    fn request_admission_policy(&self) -> crate::provider::RequestAdmissionPolicy {
        crate::provider::RequestAdmissionPolicy::namespaced(
            "x-anthropic",
            &["model", "messages"],
            &[
                "system",
                "tools",
                "tool_choice",
                "thinking",
                "output_config",
            ],
            &["max_tokens"],
        )
    }

    fn replay_target(&self, model: &str) -> crate::reasoning::ReplayTarget {
        crate::reasoning::ReplayTarget::new(
            self.name(),
            model,
            crate::reasoning::WireFormat::AnthropicMessages,
        )
        .bind_account(&self.base_url, Some(&self.api_key))
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        let mut schema_budget = crate::schema::RequestBudget::new();
        let request = crate::schema::prepare_request(request, &mut schema_budget)?;
        let request = crate::cache::prepare_request(
            &request,
            crate::reasoning::WireFormat::AnthropicMessages,
        )?;
        let request = crate::reasoning::prepare_request(&request, &self.replay_target(model));
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

        let (system, user_messages) = extract_system_message(messages, model == "claude-fable-5-1");
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
        for key in &["temperature", "top_p", "top_k", "stream"] {
            if let Some(v) = obj.get(*key) {
                body_obj.insert(key.to_string(), v.clone());
            }
        }

        if let Some(stop) = obj.get("stop") {
            body_obj.insert("stop_sequences".into(), stop.clone());
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
        let reasoning_profile = super::anthropic_reasoning::Profile::for_model(model);
        let has_thinking = obj.contains_key("thinking")
            || obj
                .get("x-anthropic")
                .and_then(|x| x.get("thinking"))
                .is_some();

        // Handle unified reasoning controls (reasoning_effort + reasoning_mode)
        // -> Anthropic thinking translation. Explicit thinking config always wins.
        if let Some(effort) = obj.get("reasoning_effort").and_then(|e| e.as_str()) {
            if reasoning_profile.supported() && !has_thinking {
                // Anthropic has no request-level standard/pro mode; map the
                // unified mode:"pro" to a one-tier effort bump (docs/src/guides/reasoning.md).
                let pro = obj
                    .get("reasoning_mode")
                    .and_then(|m| m.as_str())
                    .map(|m| m == "pro")
                    .unwrap_or(false);
                let effort = normalize_unified_effort(effort, pro);
                let effort = if effort == "none" && Self::is_fable(model) {
                    "low"
                } else {
                    effort
                };

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
                    if reasoning_profile.adaptive() {
                        // Adaptive models think by default even with no config;
                        // "disabled" is the only true zero-thinking request
                        // (verified live on sonnet-5, opus-4-8, sonnet-4-6).
                        body_obj.insert("thinking".to_string(), json!({"type": "disabled"}));
                    }
                    // Pre-4.6/Haiku: thinking is opt-in; omitting the key IS "none".
                } else if reasoning_profile.adaptive() {
                    body_obj.insert(
                        "thinking".to_string(),
                        json!({"type": "adaptive", "display": display}),
                    );
                    let anthropic_effort = reasoning_profile.effort(effort);
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
                    let budget = reasoning_profile.budget(effort, max_tokens)?;
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

        // Fable always thinks, even without an explicit thinking object. Opus 5
        // also rejects sampling parameters regardless of its thinking setting.
        if Self::is_fable(model) || model == "claude-opus-5" {
            for key in ["temperature", "top_p", "top_k"] {
                body_obj.remove(key);
            }
        }
        if Self::is_fable(model) {
            if let Some(thinking) = body_obj.get("thinking") {
                if thinking["type"] != "adaptive" {
                    return Err(ShimError::ProviderError { status: 400, body:
                        "Claude Fable requires adaptive thinking; use reasoning_effort to control depth".into(), retry_after: None });
                }
            }
            if body_obj
                .get("messages")
                .and_then(Value::as_array)
                .and_then(|messages| messages.last())
                .is_some_and(|message| message["role"] == "assistant")
            {
                return Err(ShimError::ProviderError { status: 400, body:
                    "Claude Fable does not support assistant prefill; end the request with a user turn".into(), retry_after: None });
            }
        }
        if model == "claude-fable-5-1"
            && body_obj
                .get("tool_choice")
                .and_then(|choice| choice["type"].as_str())
                .is_some_and(|kind| matches!(kind, "any" | "tool"))
        {
            return Err(ShimError::ProviderError { status: 400, body:
                "Claude Fable 5.1 supports only auto or none tool choice; request the desired tool in the prompt".into(), retry_after: None });
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

        crate::schema::normalize_native_tools(
            crate::schema::Target::Anthropic,
            &mut body,
            &mut schema_budget,
        )?;
        crate::shim::native_format(
            &request,
            crate::reasoning::WireFormat::AnthropicMessages,
            &mut body,
            &mut schema_budget,
        )?;
        crate::cache::finish_request(
            &request,
            &mut body,
            crate::reasoning::WireFormat::AnthropicMessages,
        )?;
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
        if crate::cache::uses_extended_ttl(&body) {
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

        crate::toolcall::validate_native(&body, &self.replay_target(model))?;
        Ok(ProviderRequest { url, headers, body })
    }

    fn transform_response(&self, model: &str, response: Value) -> Result<Value> {
        let native = response.clone();
        let mut result = self.transform_response_native(model, response)?;
        crate::reasoning::capture_response(&self.replay_target(model), &native, &mut result);
        crate::toolcall::capture_response(&self.replay_target(model), &native, &mut result)?;
        Ok(result)
    }

    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        let result = self.transform_stream_chunk_native(model, chunk)?;
        let native: Value = match serde_json::from_str(chunk) {
            Ok(v) => v,
            Err(_) => return Ok(result),
        };
        crate::reasoning::capture_stream(&self.replay_target(model), &native, result)
    }
}

impl Anthropic {
    fn transform_response_native(&self, model: &str, response: Value) -> Result<Value> {
        // Check for API error
        if let Some(err) = response.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(ShimError::ProviderError {
                status: 400,
                body: msg.to_string(),
                retry_after: None,
            });
        }

        transform_response_to_openai(model, &response)
    }
}

impl Anthropic {
    fn transform_stream_chunk_native(&self, model: &str, chunk: &str) -> Result<Option<String>> {
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
                let mut chunk = json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": { "role": "assistant", "content": "" },
                        "finish_reason": null,
                    }]
                });
                if let Some(usage) = parsed.pointer("/message/usage") {
                    chunk["usage"] = normalized_anthropic_usage(usage);
                }
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
                    _ => Ok(None),
                }
            }
            "message_delta" => {
                let stop = parsed
                    .pointer("/delta/stop_reason")
                    .and_then(|r| r.as_str())
                    .map(|r| match r {
                        "end_turn" => "stop",
                        "max_tokens" => "length",
                        "tool_use" => "tool_calls",
                        "refusal" if Self::is_fable(model) || model == "claude-opus-5" => {
                            "content_filter"
                        }
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

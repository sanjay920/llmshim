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

fn strip_cache_control(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            obj.remove("cache_control");
            for nested in obj.values_mut() {
                strip_cache_control(nested);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_cache_control(item);
            }
        }
        _ => {}
    }
}

/// Sanitize messages for OpenAI Responses API: strip cross-provider fields,
/// translate images, and convert tool call/result messages to Responses API format.
///
/// The Responses API expects:
/// - Assistant messages with tool_calls → split into the assistant message +
///   separate `function_call` items
/// - `role: "tool"` messages → `function_call_output` items
///
/// Same-role adjacency passes through. `input` is a flat item list, and the
/// Responses reference documents roles and their precedence but no ordering or
/// alternation rule (developers.openai.com, Responses API, `input`); this
/// adapter already emits several `function_call_output` items in a row for
/// parallel tool calls. Two adjacent assistant messages stay two items. The
/// ChatGPT adapter goes through this same translator and inherits the stance.
fn sanitize_messages(messages: &[Value]) -> Vec<Value> {
    let mut result = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

        match role {
            "assistant" => {
                result.extend(crate::reasoning::responses_items(msg));
                // Emit the assistant message (content part).
                let mut out = msg.clone();
                crate::reasoning::strip_fields(&mut out);
                if let Some(obj) = out.as_object_mut() {
                    obj.remove("annotations");
                    obj.remove("refusal");
                    obj.remove("tool_calls"); // Handled separately below.
                }
                strip_cache_control(&mut out);
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
                        let mut item = json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": arguments,
                        });
                        if let Some(id) = tc.get("_llmshim_item_id") {
                            item["id"] = id.clone();
                        }
                        result.push(item);
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
                crate::reasoning::strip_fields(&mut out);
                if let Some(obj) = out.as_object_mut() {
                    obj.remove("annotations");
                    obj.remove("refusal");
                }
                strip_cache_control(&mut out);
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
                            if k != "type" && k != "function" && k != "cache_control" {
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

fn cached_tokens_from_usage(usage: &Value) -> Option<Value> {
    usage
        .pointer("/input_tokens_details/cached_tokens")
        .cloned()
        .or_else(|| {
            usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .cloned()
        })
}

fn normalized_openai_usage(usage: &Value) -> Value {
    let mut normalized = json!({
        "prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
        "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)),
        "total_tokens": usage.get("total_tokens").cloned().unwrap_or(json!(0)),
    });

    if let Some(cached_tokens) = cached_tokens_from_usage(usage) {
        normalized["prompt_tokens_details"] = json!({
            "cached_tokens": cached_tokens,
        });
    }
    if let Some(output_details) = usage.get("output_tokens_details") {
        normalized["completion_tokens_details"] = output_details.clone();
        if let Some(reasoning_tokens) = output_details.get("reasoning_tokens") {
            normalized["reasoning_tokens"] = reasoning_tokens.clone();
        }
    }

    crate::usage::normalize_cache(usage, &mut normalized);
    normalized
}

/// OpenAI "pro" tier models (e.g. `gpt-5.5-pro`, `gpt-5.4-pro`) only accept
/// `reasoning.effort` of `medium`, `high`, or `xhigh` — they reject
/// `minimal`/`low`/`none` with HTTP 400.
fn is_pro_model(model: &str) -> bool {
    model.to_lowercase().contains("-pro")
}

/// GPT-5.6 named variants (`gpt-5.6-sol`, `-terra`, `-luna`) reject
/// `reasoning.effort: "minimal"` with HTTP 400 but accept
/// `low`/`medium`/`high`/`xhigh`/`none`.
fn is_gpt_5_6(model: &str) -> bool {
    model.to_lowercase().starts_with("gpt-5.6")
}

/// GPT-5.4 family (`gpt-5.4`, `-mini`, `-nano`) rejects "minimal": its
/// API-reported enum is none/low/medium/high/xhigh (verified live). The
/// `-pro` variant is caught by `is_pro_model` first, which is stricter.
fn is_gpt_5_4(model: &str) -> bool {
    model.to_lowercase().starts_with("gpt-5.4")
}

/// Coerce a caller's effort to a value the target model actually accepts, so a
/// value the model would 400 on is clamped rather than failing the request.
/// GPT-5.6 and GPT-6 Astra support "max"; older families clamp to xhigh.
fn clamp_reasoning_effort<'a>(model: &str, effort: &'a str) -> &'a str {
    if model == "gpt-6-astra" {
        // https://developers.openai.com/api/docs/models/gpt-6-astra
        // Astra supports low/medium/high/xhigh/max and cannot disable reasoning.
        match effort {
            "minimal" | "none" => "low",
            other => other,
        }
    } else if is_pro_model(model) {
        // pro tier: only medium/high/xhigh (verified live)
        match effort {
            "minimal" | "low" | "none" => "medium",
            "max" => "xhigh",
            other => other,
        }
    } else if is_gpt_5_6(model) {
        // 5.6: full range incl. "max"; rejects only "minimal"
        match effort {
            "minimal" => "low",
            other => other,
        }
    } else if is_gpt_5_4(model) {
        // 5.4 family: rejects "minimal" and lacks "max"
        match effort {
            "minimal" => "low",
            "max" => "xhigh",
            other => other,
        }
    } else {
        // Older and unknown models: pass through, but cap "max" at xhigh.
        match effort {
            "max" => "xhigh",
            other => other,
        }
    }
}

/// One-tier effort bump, used to emulate mode:"pro" on models where
/// `reasoning.mode` isn't accepted natively. Explicit "none" wins.
fn bump_effort(effort: &str) -> &'static str {
    match effort {
        "none" => "none",
        "minimal" => "low",
        "low" => "medium",
        "medium" => "high",
        _ => "xhigh", // high, xhigh, max
    }
}

impl Provider for OpenAi {
    fn name(&self) -> &str {
        "openai"
    }

    fn replay_target(&self, model: &str) -> crate::reasoning::ReplayTarget {
        crate::reasoning::ReplayTarget::new(
            self.name(),
            model,
            crate::reasoning::WireFormat::OpenAiResponses,
        )
        .bind_account(&self.base_url, Some(&self.api_key))
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        self.transform_request_for_target(model, request, &self.replay_target(model))
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

impl OpenAi {
    pub(crate) fn transform_response_native(&self, model: &str, response: Value) -> Result<Value> {
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
        let mut text_content: Option<String> = None;
        let mut refusal = String::new();
        let mut tool_calls: Vec<Value> = Vec::new();

        for item in output {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("message") => {
                    if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                        for part in content {
                            if part["type"] == "refusal" {
                                if let Some(text) = part["refusal"].as_str() {
                                    refusal.push_str(text);
                                }
                            }
                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                text_content.get_or_insert_with(String::new).push_str(text);
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
        if !refusal.is_empty() {
            message["refusal"] = json!(refusal);
        }
        if !tool_calls.is_empty() {
            message["tool_calls"] = json!(tool_calls);
        }

        let finish_reason = match response.get("status").and_then(Value::as_str) {
            Some("completed" | "incomplete") if message["refusal"].is_string() => "content_filter",
            Some("completed")
                if message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty()) =>
            {
                "tool_calls"
            }
            Some("completed") => "stop",
            Some("incomplete") => "length",
            _ => {
                return Err(ShimError::ProviderError {
                    status: 502,
                    body: "OpenAI response has no supported terminal status".into(),
                })
            }
        };

        let usage = response.get("usage").cloned().unwrap_or(json!({}));
        let normalized_usage = normalized_openai_usage(&usage);

        Ok(json!({
            "id": response.get("id").cloned().unwrap_or(json!("")),
            "object": "chat.completion",
            "model": model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish_reason,
            }],
            "usage": normalized_usage
        }))
    }
}

impl OpenAi {
    pub(crate) fn transform_stream_chunk_native(
        &self,
        model: &str,
        chunk: &str,
    ) -> Result<Option<String>> {
        let trimmed = chunk.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return Ok(None);
        }

        let parsed: Value = serde_json::from_str(trimmed)?;
        let event_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            "response.refusal.delta" => Ok(Some(json!({"object":"chat.completion.chunk","model":model,"choices":[{"index":0,"delta":{"refusal":parsed["delta"]},"finish_reason":null}]}).to_string())),
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
                let mut usage_out = normalized_openai_usage(&usage);
                if usage_out.get("reasoning_tokens").is_none() {
                    usage_out["reasoning_tokens"] = reasoning_tokens;
                }
                let chunk = json!({
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": finish_reason,
                    }],
                    "usage": usage_out
                });
                Ok(Some(serde_json::to_string(&chunk)?))
            }

            // All other events: skip
            _ => Ok(None),
        }
    }
}

impl OpenAi {
    pub(crate) fn transform_request_for_target(
        &self,
        model: &str,
        request: &Value,
        target: &crate::reasoning::ReplayTarget,
    ) -> Result<ProviderRequest> {
        let request = crate::schema::prepare_request(request);
        let request = crate::cache::prepare_request(&request, target.wire)?;
        let request = crate::reasoning::prepare_request(&request, target);
        let request = crate::toolcall::prepare_request(&request, target)?;
        let obj = request.as_object().ok_or(ShimError::MissingModel)?;

        let messages = obj
            .get("messages")
            .and_then(|m| m.as_array())
            .ok_or(ShimError::MissingModel)?;

        let clean_messages = sanitize_messages(messages);

        // Build Responses API request — store defaults to false (OpenAI defaults to true)
        let store = obj.get("store").cloned().unwrap_or(json!(false));
        let mut body = json!({
            "model": model,
            "input": clean_messages,
            "store": store,
        });
        let body_obj = body.as_object_mut().unwrap();

        // max_output_tokens (Responses API name)
        if let Some(v) = obj.get("max_tokens").or(obj.get("max_completion_tokens")) {
            body_obj.insert("max_output_tokens".to_string(), v.clone());
        }

        // Reasoning config — only when explicitly requested
        let effort = obj
            .get("reasoning_effort")
            .and_then(|e| e.as_str())
            .or_else(|| {
                obj.get("output_config")
                    .and_then(|oc| oc.get("effort"))
                    .and_then(|e| e.as_str())
            });

        // Unified reasoning mode: "pro" is NATIVE on gpt-5.6 and -pro models
        // (`reasoning.mode`, verified live); every other model 400s on the
        // field, so emulate it there with a one-tier effort bump — the same
        // policy the non-OpenAI providers use.
        let pro_mode = matches!(
            obj.get("reasoning_mode").and_then(|m| m.as_str()),
            Some("pro")
        );
        let mode_is_native = is_gpt_5_6(model) || is_pro_model(model);

        let effort = match (effort, pro_mode, mode_is_native) {
            (Some("max"), true, false) if model == "gpt-6-astra" => Some("max"),
            (Some(e), true, false) => Some(bump_effort(e)),
            (None, true, false) => Some("high"), // pro alone ≈ medium, bumped
            (e, _, _) => e,
        };

        if let Some(effort) = effort {
            // Clamp efforts the target model would reject up to its nearest
            // accepted tier (per-family rules in clamp_reasoning_effort).
            let effort = clamp_reasoning_effort(model, effort);
            let mut reasoning = json!({
                "effort": effort,
                "summary": "auto",
            });
            if effort == "none" {
                reasoning["summary"] = json!(null);
            }
            if pro_mode && mode_is_native {
                reasoning["mode"] = json!("pro");
            }
            body_obj.insert("reasoning".to_string(), reasoning);
        } else if pro_mode && mode_is_native {
            // mode without effort: valid natively — let the model pick its
            // default effort under pro mode.
            body_obj.insert(
                "reasoning".to_string(),
                json!({"mode": "pro", "summary": "auto"}),
            );
        }

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

        for key in [
            "include",
            "prompt_cache_key",
            "prompt_cache_retention",
            "safety_identifier",
        ] {
            if let Some(v) = obj.get(key) {
                body_obj.insert(key.to_string(), v.clone());
            }
        }
        if let Some(ext) = obj.get("x-openai").and_then(|e| e.as_object()) {
            for (k, v) in ext {
                body_obj.insert(k.clone(), v.clone());
            }
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

        // Fast mode / priority processing: translate "speed": "fast" to
        // OpenAI's priority processing format.
        if let Some(speed) = obj.get("speed").and_then(|s| s.as_str()) {
            if speed == "fast" {
                body_obj.insert(
                    "priority".to_string(),
                    json!({"type": "default_with_boost"}),
                );
            }
        }

        // Strip provider-specific params from body
        body_obj.remove("thinking");

        let url = format!("{}/responses", self.base_url);

        crate::reasoning::enforce_stateless(&mut body)?;
        crate::toolcall::validate_native(&body, target)?;
        crate::schema::normalize_native_tools(crate::schema::Target::OpenAiResponses, &mut body);
        crate::shim::native_format(
            &request,
            crate::reasoning::WireFormat::OpenAiResponses,
            &mut body,
        );
        crate::cache::finish_request(
            &request,
            &mut body,
            crate::reasoning::WireFormat::OpenAiResponses,
        )?;
        Ok(ProviderRequest {
            url,
            headers: vec![
                ("Authorization".into(), format!("Bearer {}", self.api_key)),
                ("Content-Type".into(), "application/json".into()),
            ],
            body,
        })
    }
}

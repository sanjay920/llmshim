//! Shared request conversion with an explicit transport replay boundary.
use super::*;

impl Anthropic {
    pub(in crate::providers) fn request_for_target(
        &self,
        model: &str,
        request: &Value,
        target: &crate::reasoning::ReplayTarget,
    ) -> Result<ProviderRequest> {
        crate::reasoning::preflight_request(request)?;
        let mut schema_budget = crate::schema::RequestBudget::new();
        let request = crate::schema::prepare_request(request, &mut schema_budget)?;
        let request = crate::cache::prepare_request(
            &request,
            crate::reasoning::WireFormat::AnthropicMessages,
        )?;
        let request = crate::reasoning::prepare_request(&request, target)?;
        let request = crate::toolcall::prepare_request(&request, target)?;
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

        let (system, user_messages) =
            extract_system_message(messages, constraints::preserves_bound_system_turns(model));
        let anthropic_messages = transform_messages(&user_messages);

        let mut body = json!({
            "model": model,
            "messages": anthropic_messages,
        });

        let body_obj = body.as_object_mut().unwrap();

        // A native top-level `system` keeps the caller's own cache boundaries. Given beside
        // chat-style system messages it is ambiguous which the caller meant, so it is refused
        // rather than one of them silently dropped.
        match (system, obj.get("system")) {
            (Some(_), Some(_)) => {
                return Err(ShimError::ProviderError {
                    status: 400,
                    body: concat!(
                        "give the system prompt either as a top-level `system` ",
                        "or as system messages, not both"
                    )
                    .into(),
                    retry_after: None,
                });
            }
            (Some(from_messages), None) => {
                body_obj.insert("system".to_string(), from_messages);
            }
            (None, Some(native)) => {
                body_obj.insert("system".to_string(), native.clone());
            }
            (None, None) => {}
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
        if let Some(stop) = obj.get("stop_sequences") {
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

        if obj.get("parallel_tool_calls") == Some(&json!(false)) {
            let mut choice = body_obj
                .get("tool_choice")
                .cloned()
                .unwrap_or_else(|| json!({"type": "auto"}));
            choice["disable_parallel_tool_use"] = json!(true);
            body_obj.insert("tool_choice".into(), choice);
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
        let reasoning_profile = crate::providers::anthropic_reasoning::Profile::for_model(model);
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
                let effort = constraints::clamp_effort(model, effort);

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

        let last_role = body_obj
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.last())
            .and_then(|message| message["role"].as_str())
            .map(str::to_owned);
        constraints::apply(model, body_obj, false, last_role.as_deref())?;

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

        // Caller-supplied beta tokens, passed as
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

        crate::toolcall::validate_native(&body, target)?;
        Ok(ProviderRequest { url, headers, body })
    }
}

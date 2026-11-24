//! Shared request conversion with an explicit transport replay boundary.
use super::*;

impl Gemini {
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
            crate::reasoning::WireFormat::GoogleGenerateContent,
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

        crate::toolcall::validate_native(&body, target)?;
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
}

use super::types::{ChatRequest, ChatResponse, ResponseMessage, StreamEvent, Usage};
use crate::provider::{Provider, RequestAdmissionPolicy};
use crate::router::Router;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub(crate) struct AdmissionTarget {
    pub provider_name: String,
    pub model: String,
    pub policy: RequestAdmissionPolicy,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedRequest {
    pub payload: Value,
    pub target: AdmissionTarget,
}

/// Convert our ChatRequest into the OpenAI-format Value that lib.rs expects.
pub fn request_to_value(req: &ChatRequest) -> Value {
    let mut v = json!({
        "model": req.model,
        "messages": req.messages,
    });
    if let Some(cache) = &req.cache {
        v["x-cache"] = json!(cache);
    }

    // Apply provider-agnostic config
    if let Some(cfg) = &req.config {
        if let Some(mt) = cfg.max_tokens {
            v["max_tokens"] = json!(mt);
        }
        if let Some(t) = cfg.temperature {
            v["temperature"] = json!(t);
        }
        if let Some(tp) = cfg.top_p {
            v["top_p"] = json!(tp);
        }
        if let Some(tk) = cfg.top_k {
            v["top_k"] = json!(tk);
        }
        if let Some(stop) = &cfg.stop {
            v["stop"] = json!(stop);
        }
        if let Some(effort) = &cfg.reasoning_effort {
            v["reasoning_effort"] = json!(effort);
        }
        if let Some(mode) = &cfg.reasoning_mode {
            v["reasoning_mode"] = json!(mode);
        }
    }

    // Merge provider_config as top-level keys (passthrough to provider transform)
    if let Some(pc) = &req.provider_config {
        if let Some(obj) = pc.as_object() {
            for (k, val) in obj {
                v[k.clone()] = val.clone();
            }
        }
    }

    if let Some(shim) = &req.shim {
        v["x-shim"] = json!(shim);
    }
    if let Some(format) = &req.response_format {
        v["response_format"] = format.clone();
    }
    // Routing and canonical history always come from the typed proxy envelope.
    // Validation rejects conflicting passthrough fields, and these assignments
    // keep the conversion safe if a future internal caller skips validation.
    v["model"] = json!(req.model);
    v["messages"] = json!(req.messages);
    v
}

fn invalid_request_override(field: &str) -> crate::error::ShimError {
    crate::error::ShimError::ProviderError {
        status: 400,
        body: format!("{field} cannot replace a field admitted from the proxy request"),
        retry_after: None,
    }
}

fn validate_provider_config_envelope(req: &ChatRequest) -> crate::error::Result<()> {
    let Some(provider_config) = req.provider_config.as_ref() else {
        return Ok(());
    };
    let Some(provider_config) = provider_config.as_object() else {
        return Err(crate::error::ShimError::ProviderError {
            status: 400,
            body: "provider_config must be an object".into(),
            retry_after: None,
        });
    };

    for protected_field in ["model", "messages"] {
        if provider_config.contains_key(protected_field) {
            return Err(invalid_request_override(&format!(
                "provider_config.{protected_field}"
            )));
        }
    }

    Ok(())
}

pub(crate) fn active_native_namespace(target: &AdmissionTarget) -> Option<&str> {
    target.policy.native_namespace()
}

fn validate_active_native_overrides(
    request: &Value,
    target: &AdmissionTarget,
) -> crate::error::Result<()> {
    let Some(namespace) = target.policy.native_namespace() else {
        return Ok(());
    };
    let Some(native_config) = request.get(namespace).and_then(Value::as_object) else {
        return Ok(());
    };
    for protected_field in target.policy.protected_native_fields() {
        if native_config.contains_key(*protected_field) {
            return Err(invalid_request_override(&format!(
                "{namespace}.{protected_field}"
            )));
        }
    }
    Ok(())
}

fn admission_target(provider: &dyn Provider, resolved_model: String) -> AdmissionTarget {
    AdmissionTarget {
        provider_name: provider.name().to_string(),
        policy: provider.request_admission_policy(),
        model: resolved_model,
    }
}

fn prepare_resolved_request(
    request: Value,
    provider: &dyn Provider,
    resolved_model: String,
) -> crate::error::Result<PreparedRequest> {
    let target = admission_target(provider, resolved_model);
    validate_active_native_overrides(&request, &target)?;
    if let Some(messages) = request["messages"].as_array() {
        crate::toolcall::validate_history(messages)?;
    }
    Ok(PreparedRequest {
        payload: request,
        target,
    })
}

pub(crate) fn prepare_engine_request(
    router: &Router,
    request: Value,
) -> crate::error::Result<PreparedRequest> {
    let request = router.expand_route(&request)?.into_owned();
    let addressed_model = request["model"]
        .as_str()
        .ok_or(crate::error::ShimError::MissingModel)?;
    let (provider, resolved_model) = router.resolve(addressed_model)?;
    prepare_resolved_request(request, provider, resolved_model)
}

pub(crate) fn validate_resolvable_fallbacks(
    router: &Router,
    base_request: &Value,
    fallback_models: &[String],
) -> crate::error::Result<()> {
    for fallback_model in fallback_models {
        let mut fallback_request = base_request.clone();
        fallback_request["model"] = Value::String(fallback_model.clone());
        let fallback_request = match router.expand_route(&fallback_request) {
            Ok(expanded) => expanded.into_owned(),
            Err(_) => continue,
        };
        let (provider, resolved_model) = match router.resolve(fallback_model) {
            Ok(resolved) => resolved,
            Err(_) => continue,
        };
        prepare_resolved_request(fallback_request, provider, resolved_model)?;
    }
    Ok(())
}

/// Build the route-expanded engine request and immutable admission target.
pub(crate) fn prepare_request(
    router: &Router,
    req: &ChatRequest,
) -> crate::error::Result<PreparedRequest> {
    validate_provider_config_envelope(req)?;
    prepare_engine_request(router, request_to_value(req))
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use crate::reasoning::WireFormat;

    fn router() -> Router {
        Router::new().register(
            "openai",
            Box::new(crate::providers::openai::OpenAi::new("test".into())),
        )
    }

    fn request_with_provider_config(provider_config: Value) -> ChatRequest {
        serde_json::from_value(json!({
            "model": "openai/gpt-5.6-luna",
            "messages": [{"role": "user", "content": "canonical"}],
            "provider_config": provider_config,
        }))
        .unwrap()
    }

    #[test]
    fn canonical_model_and_messages_cannot_be_replaced_at_the_merge_root() {
        for provider_config in [
            json!({"model": "anthropic/claude-opus-5"}),
            json!({"messages": [{"role": "user", "content": "replacement"}]}),
        ] {
            let request = request_with_provider_config(provider_config);
            assert!(prepare_request(&router(), &request).is_err());
            let converted = request_to_value(&request);
            assert_eq!(converted["model"], "openai/gpt-5.6-luna");
            assert_eq!(converted["messages"][0]["content"], "canonical");
        }
    }

    #[test]
    fn native_namespaces_cannot_replace_the_admitted_model_or_prompt() {
        for protected_field in ["model", "input"] {
            let request = request_with_provider_config(json!({
                "x-openai": {(protected_field): "replacement"}
            }));
            let error = prepare_request(&router(), &request)
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("x-openai.{protected_field}")));
        }
    }

    #[test]
    fn active_override_rules_follow_each_provider_wire() {
        let cases = [
            (
                "openai",
                WireFormat::OpenAiResponses,
                "x-openai",
                &["input"] as &'static [&'static str],
            ),
            (
                "chatgpt",
                WireFormat::OpenAiResponses,
                "x-chatgpt",
                &["input"],
            ),
            (
                "anthropic",
                WireFormat::AnthropicMessages,
                "x-anthropic",
                &["messages"],
            ),
            (
                "gemini",
                WireFormat::GoogleGenerateContent,
                "x-gemini",
                &["contents"],
            ),
            (
                "openrouter",
                WireFormat::OpenAiChat,
                "x-openrouter",
                &["messages"],
            ),
            ("vllm", WireFormat::OpenAiChat, "x-vllm", &["messages"]),
            (
                "sglang",
                WireFormat::OpenAiResponses,
                "x-sglang",
                &["input"],
            ),
        ];
        for (provider_name, _wire, namespace, protected_fields) in cases {
            let target = AdmissionTarget {
                provider_name: provider_name.into(),
                model: "test".into(),
                policy: RequestAdmissionPolicy::namespaced(namespace, protected_fields, &[], &[]),
            };
            let request = json!({(namespace): {(protected_fields[0]): []}});
            assert!(validate_active_native_overrides(&request, &target).is_err());
        }

        let xai_target = AdmissionTarget {
            provider_name: "xai".into(),
            model: "grok-4.7".into(),
            policy: RequestAdmissionPolicy::default(),
        };
        assert!(validate_active_native_overrides(
            &json!({"x-xai":{"input":"ignored"}}),
            &xai_target
        )
        .is_ok());
    }

    #[test]
    fn inactive_namespaces_and_nested_data_fields_remain_available() {
        let request = request_with_provider_config(json!({
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object", "properties": {
                "model": {"type": "string"}, "messages": {"type": "string"},
                "input": {"type": "string"}, "contents": {"type": "string"}
            }}}}],
            "x-openai": {"reasoning": {"effort": "high"}, "max_output_tokens": 2048, "messages": "non-overriding native data"},
            "x-anthropic": {"input": "inactive data"},
            "x-unknown": {"model": "inactive data"},
            "x-openrouter": {"provider": {"sort": "throughput"}},
        }));
        let prepared = prepare_request(&router(), &request).unwrap();
        assert_eq!(prepared.payload["tools"][0]["function"]["name"], "lookup");
        assert_eq!(prepared.payload["x-openai"]["reasoning"]["effort"], "high");
        assert_eq!(
            prepared.payload["x-openrouter"]["provider"]["sort"],
            "throughput"
        );
    }

    #[test]
    fn named_route_defaults_are_expanded_before_override_validation() {
        let routed = router().route(
            "large",
            crate::config::Route {
                model: "openai/gpt-5.6-luna".into(),
                settings: std::collections::BTreeMap::from([
                    ("max_tokens".into(), json!(32_000)),
                    ("x-openai".into(), json!({"input": "replacement"})),
                ]),
            },
        );
        let request: ChatRequest = serde_json::from_value(json!({
            "model": "route/large",
            "messages": [{"role": "user", "content": "canonical"}]
        }))
        .unwrap();
        let error = prepare_request(&routed, &request).unwrap_err().to_string();
        assert!(error.contains("x-openai.input"));
    }

    #[test]
    fn named_route_and_alias_target_are_prepared_for_admission_and_dispatch() {
        let routed = router()
            .alias("fast", "openai/gpt-5.6-luna")
            .route(
                "large",
                crate::config::Route {
                    model: "fast".into(),
                    settings: std::collections::BTreeMap::from([
                        ("max_tokens".into(), json!(32_000)),
                        (
                            "tools".into(),
                            json!([{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}]),
                        ),
                    ]),
                },
            );
        let request: ChatRequest = serde_json::from_value(json!({
            "model": "route/large",
            "messages": [{"role": "user", "content": "canonical"}]
        }))
        .unwrap();
        let prepared = prepare_request(&routed, &request).unwrap();
        assert_eq!(prepared.target.provider_name, "openai");
        assert_eq!(prepared.target.model, "gpt-5.6-luna");
        assert_eq!(prepared.payload["model"], "fast");
        assert_eq!(prepared.payload["max_tokens"], 32_000);
        assert_eq!(prepared.payload["tools"][0]["function"]["name"], "lookup");
        assert!(crate::proxy::ratelimit::estimate_prepared_request_tokens(&prepared) >= 32_000);
    }

    #[test]
    fn openrouter_model_routing_controls_remain_supported() {
        let router = Router::new().register(
            "openrouter",
            Box::new(crate::providers::openrouter::OpenRouter::new("test".into())),
        );
        let request: ChatRequest = serde_json::from_value(json!({
            "model":"openrouter/anthropic/claude-sonnet-5",
            "messages":[{"role":"user","content":"hello"}],
            "provider_config":{"x-openrouter":{
                "models":["anthropic/claude-sonnet-5","openai/gpt-5.6-luna"],
                "provider":{"sort":"throughput"},
                "route":"fallback"
            }}
        }))
        .unwrap();
        let prepared = prepare_request(&router, &request).unwrap();
        assert_eq!(prepared.target.provider_name, "openrouter");
        assert_eq!(
            prepared.payload["x-openrouter"]["models"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            prepared.payload["x-openrouter"]["provider"]["sort"],
            "throughput"
        );
    }

    #[test]
    fn every_resolvable_fallback_target_is_validated_before_dispatch() {
        let router = Router::new()
            .register(
                "openai",
                Box::new(crate::providers::openai::OpenAi::new("test".into())),
            )
            .register(
                "anthropic",
                Box::new(crate::providers::anthropic::Anthropic::new("test".into())),
            );
        let openai_primary: ChatRequest = serde_json::from_value(json!({
            "model":"openai/gpt-5.6-luna",
            "messages":[{"role":"user","content":"canonical"}],
            "provider_config":{"x-anthropic":{
                "model":"claude-opus-5",
                "messages":[{"role":"user","content":"replacement"}]
            }}
        }))
        .unwrap();
        let prepared = prepare_request(&router, &openai_primary).unwrap();
        let error = validate_resolvable_fallbacks(
            &router,
            &prepared.payload,
            &["anthropic/claude-sonnet-5".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("x-anthropic.model"));

        let anthropic_primary: ChatRequest = serde_json::from_value(json!({
            "model":"anthropic/claude-sonnet-5",
            "messages":[{"role":"user","content":"canonical"}],
            "provider_config":{"x-openai":{"input":"replacement"}}
        }))
        .unwrap();
        let prepared = prepare_request(&router, &anthropic_primary).unwrap();
        let error = validate_resolvable_fallbacks(
            &router,
            &prepared.payload,
            &["openai/gpt-5.6-luna".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("x-openai.input"));

        assert!(validate_resolvable_fallbacks(
            &router,
            &prepared.payload,
            &["missing-provider/model".into()]
        )
        .is_ok());
    }

    #[test]
    fn fallback_routes_and_aliases_are_validated_after_expansion() {
        let router = Router::new()
            .register(
                "openai",
                Box::new(crate::providers::openai::OpenAi::new("test".into())),
            )
            .register(
                "anthropic",
                Box::new(crate::providers::anthropic::Anthropic::new("test".into())),
            )
            .alias("claude-route-target", "anthropic/claude-sonnet-5")
            .route(
                "dangerous-fallback",
                crate::config::Route {
                    model: "claude-route-target".into(),
                    settings: std::collections::BTreeMap::from([(
                        "x-anthropic".into(),
                        json!({"messages":[{"role":"user","content":"replacement"}]}),
                    )]),
                },
            );
        let request: ChatRequest = serde_json::from_value(json!({
            "model":"openai/gpt-5.6-luna",
            "messages":[{"role":"user","content":"canonical"}]
        }))
        .unwrap();
        let prepared = prepare_request(&router, &request).unwrap();
        let error = validate_resolvable_fallbacks(
            &router,
            &prepared.payload,
            &["route/dangerous-fallback".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("x-anthropic.messages"));
    }

    #[test]
    fn arbitrary_openai_compatible_names_own_their_namespace_and_wire_policy() {
        for (wire, prompt_field, output_field) in [
            (WireFormat::OpenAiChat, "messages", "max_tokens"),
            (WireFormat::OpenAiResponses, "input", "max_output_tokens"),
        ] {
            let router = Router::new().register(
                "local",
                Box::new(
                    crate::providers::openai_compat::OpenAiCompatible::new(
                        "local",
                        "http://127.0.0.1:9",
                        None,
                    )
                    .with_wire(wire),
                ),
            );
            for protected_field in ["model", prompt_field] {
                let request: ChatRequest = serde_json::from_value(json!({
                    "model":"local/declared",
                    "messages":[{"role":"user","content":"canonical"}],
                    "provider_config":{"x-local":{(protected_field):"replacement"}}
                }))
                .unwrap();
                let error = prepare_request(&router, &request).unwrap_err().to_string();
                assert!(error.contains(&format!("x-local.{protected_field}")));
            }

            let request: ChatRequest = serde_json::from_value(json!({
                "model":"local/declared",
                "messages":[{"role":"user","content":"canonical"}],
                "provider_config":{"x-local":{(output_field):7_000}}
            }))
            .unwrap();
            let prepared = prepare_request(&router, &request).unwrap();
            assert!(crate::proxy::ratelimit::estimate_prepared_request_tokens(&prepared) >= 7_000);
        }
    }

    #[test]
    fn arbitrary_openai_compatible_fallback_names_activate_their_namespace() {
        for (wire, protected_field) in [
            (WireFormat::OpenAiChat, "messages"),
            (WireFormat::OpenAiResponses, "input"),
        ] {
            let router = Router::new()
                .register(
                    "openai",
                    Box::new(crate::providers::openai::OpenAi::new("test".into())),
                )
                .register(
                    "local",
                    Box::new(
                        crate::providers::openai_compat::OpenAiCompatible::new(
                            "local",
                            "http://127.0.0.1:9",
                            None,
                        )
                        .with_wire(wire),
                    ),
                );
            let request: ChatRequest = serde_json::from_value(json!({
                "model":"openai/gpt-5.6-luna",
                "messages":[{"role":"user","content":"canonical"}],
                "provider_config":{"x-local":{(protected_field):"replacement"}}
            }))
            .unwrap();
            let prepared = prepare_request(&router, &request).unwrap();
            let error = validate_resolvable_fallbacks(
                &router,
                &prepared.payload,
                &["local/declared".into()],
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(&format!("x-local.{protected_field}")));
        }
    }
}

/// Release date as a Unix timestamp. The OpenAI list shape types `created` as
/// an integer, so an undated model reports `0` rather than a null.
fn released_at(id: &str) -> i64 {
    crate::catalog::lookup_id(id)
        .and_then(|info| info.release_date)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|at| at.and_utc().timestamp())
        .unwrap_or(0)
}

/// Build the shared `GET /v1/models` body: the OpenAI list envelope plus
/// llmshim's own array. Proxy and gateway serve the identical shape.
pub(crate) fn models_response(provider_keys: &[&str]) -> super::types::ModelsResponse {
    let available = crate::models::available_models(provider_keys);
    super::types::ModelsResponse {
        object: "list",
        data: available
            .iter()
            .map(|m| super::types::ModelObject {
                id: m.id.to_string(),
                object: "model",
                created: released_at(m.id),
                owned_by: m.provider.to_string(),
            })
            .collect(),
        models: available
            .into_iter()
            .map(|m| super::types::ModelEntry {
                id: m.id.to_string(),
                provider: m.provider.to_string(),
                name: m.name.to_string(),
            })
            .collect(),
    }
}

/// Convert the OpenAI-format Value response from lib.rs into our ChatResponse.
pub fn value_to_response(v: &Value, provider: &str, latency_ms: u64) -> ChatResponse {
    let choice = &v["choices"][0];
    let msg = &choice["message"];

    let content = msg.get("content").cloned().unwrap_or(Value::Null);
    let tool_calls = msg.get("tool_calls").cloned().filter(|v| !v.is_null());
    let reasoning_text = crate::reasoning::reasoning_text(msg);
    let reasoning = (!reasoning_text.is_empty()).then_some(reasoning_text);

    let usage = extract_usage(&v["usage"]);

    ChatResponse {
        finish_reason: choice["finish_reason"].as_str().map(str::to_owned),
        served_model: v["x-llmshim-served-model"].as_str().map(str::to_owned),
        id: v
            .get("id")
            .and_then(|id| id.as_str())
            .unwrap_or("")
            .to_string(),
        model: v
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string(),
        provider: provider.to_string(),
        message: ResponseMessage {
            refusal: msg["refusal"].as_str().map(str::to_owned),
            role: "assistant".to_string(),
            content,
            tool_calls,
            reasoning: msg.get("reasoning").cloned(),
        },
        reasoning,
        usage,
        latency_ms,
    }
}

/// Extract usage from an OpenAI-format usage object.
pub fn extract_usage(usage: &Value) -> Usage {
    let input = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning = usage
        .get("reasoning_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(input + output);

    Usage {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: reasoning,
        total_tokens: total,
        cache_read_tokens: usage["cache_read_tokens"].as_u64().unwrap_or(0),
        cache_write_tokens: usage["cache_write_tokens"].as_u64().unwrap_or(0),
        cost_usd: crate::cost::stamped(usage),
        cost_source: crate::cost::stamped_source(usage).map(str::to_owned),
    }
}

#[cfg(test)]
#[test]
fn cache_accounting_survives_proxy_projection() {
    let usage = extract_usage(
        &json!({"prompt_tokens": 10, "cache_read_tokens": 7, "cache_write_tokens": 3}),
    );
    let wire = serde_json::to_value(usage).unwrap();
    assert_eq!(wire["cache_read_tokens"], 7);
    assert_eq!(wire["cache_write_tokens"], 3);
    let empty = serde_json::to_value(extract_usage(&json!({}))).unwrap();
    assert_eq!(empty["cache_read_tokens"], 0);
    assert_eq!(empty["cache_write_tokens"], 0);
    let events =
        chunk_to_events(&json!({"choices":[],"usage":{"cache_read_tokens":9}}).to_string());
    assert!(matches!(
        &events[..],
        [StreamEvent::Usage(Usage {
            cache_read_tokens: 9,
            ..
        })]
    ));
}

#[cfg(test)]
#[test]
fn proxy_assistant_message_round_trips_reasoning_without_projection_loss() {
    use crate::provider::Provider;
    let provider = crate::providers::anthropic::Anthropic::new("test".into());
    let native = json!({"stop_reason":"end_turn","content":[{"type":"thinking","thinking":"thought","signature":"opaque+/="},{"type":"text","text":"answer"}]});
    let normalized = provider
        .transform_response("claude-sonnet-4-6", native)
        .unwrap();
    let response = value_to_response(&normalized, "anthropic", 0);
    let message = serde_json::to_value(response.message).unwrap();
    assert_eq!(
        message["reasoning"],
        normalized["choices"][0]["message"]["reasoning"]
    );
    let request: ChatRequest =
        serde_json::from_value(json!({"model":"claude-sonnet-4-6","messages":[message]})).unwrap();
    let outbound = provider
        .transform_request("claude-sonnet-4-6", &request_to_value(&request))
        .unwrap();
    assert_eq!(
        outbound.body["messages"][0]["content"][0]["signature"],
        "opaque+/="
    );
    let signature = json!({"data":"opaque","origin":{"provider":"gemini","model":"gemini-3.8-flash","family":"gemini","wire":"google-generate-content","received_at":"2026-09-16T00:00:00Z"}});
    let events=chunk_to_events(&json!({"choices":[{"delta":{"tool_calls":[{"id":"c1","function":{"name":"read","arguments":"{}"},"thought_signature":signature}]}}]}).to_string());
    assert_eq!(
        serde_json::to_value(&events[0]).unwrap()["thought_signature"],
        signature
    );
}

/// Parse a single OpenAI-format stream chunk into typed SSE events.
/// Returns one or more events per chunk.
pub fn chunk_to_events(chunk_json: &str) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    let parsed: Value = match serde_json::from_str(chunk_json) {
        Ok(v) => v,
        Err(_) => return events,
    };

    let choice = parsed["choices"]
        .as_array()
        .and_then(|choices| {
            choices
                .iter()
                .find(|c| c["index"].as_u64().unwrap_or(0) == 0)
        })
        .unwrap_or(&Value::Null);
    let delta = &choice["delta"];

    // Reasoning content
    if let Some(blocks) = delta.get("reasoning").and_then(Value::as_array) {
        if !blocks.is_empty() {
            events.push(StreamEvent::Reasoning {
                text: crate::reasoning::reasoning_text(delta),
                blocks: blocks.clone(),
            });
        }
    }

    // Text content
    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            events.push(StreamEvent::Content {
                text: content.to_string(),
            });
        }
    }

    if let Some(text) = delta["refusal"].as_str().filter(|text| !text.is_empty()) {
        events.push(StreamEvent::Content { text: text.into() });
    }

    // Tool calls
    if let Some(tool_calls) = delta.get("tool_calls").and_then(|tc| tc.as_array()) {
        for tc in tool_calls {
            if let (Some(id), Some(name)) = (
                tc.get("id").and_then(|i| i.as_str()),
                tc.pointer("/function/name").and_then(|n| n.as_str()),
            ) {
                let args = tc
                    .pointer("/function/arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("")
                    .to_string();
                events.push(StreamEvent::ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    arguments: args,
                    thought_signature: tc.get("thought_signature").cloned(),
                    wire_ids: tc.get("wire_ids").cloned(),
                });
            }
        }
    }

    // Usage-only chunks are valid (notably Chat Completions sends one after
    // its last content delta). Do not require a choice or a finish reason.
    if let Some(usage) = parsed.get("usage").filter(|v| v.is_object()) {
        events.push(StreamEvent::Usage(extract_usage(usage)));
    }

    // Finish reason → done event
    if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        events.push(StreamEvent::Done {
            finish_reason: Some(finish.into()),
            served_model: parsed["x-llmshim-served-model"].as_str().map(str::to_owned),
        });
    }

    events
}

#[cfg(test)]
#[test]
fn proxy_keeps_capability_options_and_refusal_projection() {
    let req: ChatRequest =
        serde_json::from_value(json!({"model":"local/test","messages":[],"x-shim":{"structured_output":"prompt"},"response_format":{"type":"json_schema","json_schema":{"schema":{"type":"integer"}}}})).unwrap();
    let normalized = request_to_value(&req);
    assert_eq!(normalized["x-shim"]["structured_output"], "prompt");
    assert_eq!(
        normalized["response_format"]["json_schema"]["schema"]["type"],
        "integer"
    );
    let mut r = json!({"choices":[{"message":{"role":"assistant","content":null}}]});
    r["choices"][0]["message"]["refusal"] = json!("Cannot comply");
    let projected = value_to_response(&r, "local", 0);
    assert_eq!(
        serde_json::to_value(projected).unwrap()["message"]["refusal"],
        "Cannot comply"
    );
}

use llmshim::models::{available_models, spec, ModelCapabilities, Support, MODELS};

#[test]
fn models_registry_has_all_providers() {
    let providers: Vec<&str> = MODELS.iter().map(|m| m.provider).collect();
    assert!(providers.contains(&"openai"));
    assert!(providers.contains(&"anthropic"));
    assert!(providers.contains(&"gemini"));
    assert!(providers.contains(&"xai"));
}

#[test]
fn models_registry_has_expected_count() {
    assert_eq!(MODELS.len(), 27);
}

#[test]
fn models_registry_includes_gpt_5_5() {
    assert!(MODELS.iter().any(|m| m.id == "openai/gpt-5.5"));
}

#[test]
fn models_ids_have_provider_prefix() {
    for m in MODELS {
        assert!(m.id.contains('/'), "Model {} missing provider prefix", m.id);
        assert!(
            m.id.starts_with(&format!("{}/", m.provider)),
            "Model {} prefix doesn't match provider {}",
            m.id,
            m.provider
        );
    }
}

#[test]
fn available_models_filters_by_provider() {
    let registered = vec!["anthropic", "openai"];
    let models = available_models(&registered);
    for m in &models {
        assert!(
            m.provider == "anthropic" || m.provider == "openai",
            "Unexpected provider: {}",
            m.provider
        );
    }
    // Should not include gemini or xai
    assert!(models.iter().all(|m| m.provider != "gemini"));
    assert!(models.iter().all(|m| m.provider != "xai"));
}

#[test]
fn available_models_empty_providers_returns_empty() {
    let models = available_models(&[]);
    assert!(models.is_empty());
}

#[test]
fn available_models_all_providers_returns_all() {
    let registered = vec!["openai", "anthropic", "gemini", "xai"];
    let models = available_models(&registered);
    assert_eq!(models.len(), MODELS.len());
}

#[test]
fn available_models_unknown_provider_ignored() {
    let registered = vec!["nonexistent"];
    let models = available_models(&registered);
    assert!(models.is_empty());
}

// --- spec metadata (issue #31) ---

#[test]
fn spec_looks_up_by_full_id_and_bare_name() {
    let by_id = spec("openai/gpt-5.6-sol").expect("lookup by full id");
    let by_name = spec("gpt-5.6-sol").expect("lookup by bare name");
    assert_eq!(by_id.id, by_name.id);
    assert_eq!(by_id.name, "gpt-5.6-sol");
}

#[test]
fn spec_returns_none_for_unregistered() {
    assert!(spec("openai/does-not-exist").is_none());
}

#[test]
fn undocumented_spec_fields_stay_unknown_not_guessed() {
    // The honesty rule: xAI publishes no max-output ceiling and doesn't state
    // per-model streaming / parallel-tool-call support, so those stay
    // None/Unknown rather than being guessed — even though other fields are
    // populated for the same model.
    let m = spec("xai/grok-4.5").unwrap();
    assert_eq!(m.max_output_tokens, None);
    assert_eq!(m.capabilities.streaming, Support::Unknown);
    assert_eq!(m.capabilities.parallel_tool_calls, Support::Unknown);
    // Verified fields are populated:
    assert_eq!(m.context_window_tokens, Some(500_000));
    assert_eq!(m.capabilities.tools, Support::Supported);
}

#[test]
fn verified_specs_are_populated() {
    let opus = spec("anthropic/claude-opus-4-8").unwrap();
    assert_eq!(opus.context_window_tokens, Some(1_000_000));
    assert_eq!(opus.max_output_tokens, Some(128_000));
    assert_eq!(opus.capabilities.images, Support::Supported);
    assert_eq!(opus.capabilities.prompt_cache, Support::Supported);

    let haiku = spec("anthropic/claude-haiku-4-5-20251001").unwrap();
    assert_eq!(haiku.context_window_tokens, Some(200_000));
    assert_eq!(haiku.max_output_tokens, Some(64_000));

    let mini = spec("openai/gpt-5.4-mini").unwrap();
    assert_eq!(mini.context_window_tokens, Some(400_000));
}

#[test]
fn documented_capability_exceptions_are_recorded() {
    // gpt-5.5-pro: streaming explicitly not supported.
    assert_eq!(
        spec("openai/gpt-5.5-pro").unwrap().capabilities.streaming,
        Support::Unsupported
    );
    // gpt-5.4-pro: structured outputs explicitly not supported.
    assert_eq!(
        spec("openai/gpt-5.4-pro")
            .unwrap()
            .capabilities
            .structured_output,
        Support::Unsupported
    );
}

#[test]
fn default_capabilities_are_all_unknown() {
    let caps = ModelCapabilities::default();
    assert_eq!(caps, ModelCapabilities::unknown());
    assert_eq!(caps.reasoning, Support::Unknown);
    assert_eq!(caps.structured_output, Support::Unknown);
}

#[test]
fn reasoning_support_is_populated_for_every_model() {
    // reasoning is derived from the provider clamp logic, so no model should be
    // left Unknown on this field.
    for m in MODELS {
        assert_ne!(
            m.capabilities.reasoning,
            Support::Unknown,
            "{} has unpopulated reasoning support",
            m.id
        );
    }
}

#[test]
fn reasoning_support_matches_provider_behavior() {
    // Effort-controlled models accept a reasoning control.
    for id in [
        "openai/gpt-5.6-sol",
        "anthropic/claude-opus-5",
        "anthropic/claude-opus-4-8",
        "xai/grok-4.6",
        "xai/grok-4.5",
    ] {
        assert_eq!(
            spec(id).unwrap().capabilities.reasoning,
            Support::Supported,
            "{id} should support a reasoning control"
        );
    }
    // grok-4.20-* are name-locked and 400 on any reasoning param.
    for id in [
        "xai/grok-4.20-beta-0309-reasoning",
        "xai/grok-4.20-beta-0309-non-reasoning",
        "xai/grok-4.20-multi-agent-beta-0309",
    ] {
        assert_eq!(
            spec(id).unwrap().capabilities.reasoning,
            Support::Unsupported,
            "{id} rejects reasoning controls"
        );
    }
}

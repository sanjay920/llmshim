use llmshim::models::{available_models, MODELS};

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
    assert_eq!(MODELS.len(), 12);
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

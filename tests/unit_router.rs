use llmshim::error::ShimError;
use llmshim::providers::anthropic::Anthropic;
use llmshim::providers::openai::OpenAi;
use llmshim::router::{parse_model, Router};
use std::collections::HashMap;

// ============================================================
// parse_model tests
// ============================================================

#[test]
fn parse_explicit_provider_openai() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model("openai/gpt-4o", &aliases).unwrap();
    assert_eq!(provider, "openai");
    assert_eq!(model, "gpt-4o");
}

#[test]
fn parse_explicit_provider_anthropic() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model("anthropic/claude-sonnet-4-20250514", &aliases).unwrap();
    assert_eq!(provider, "anthropic");
    assert_eq!(model, "claude-sonnet-4-20250514");
}

#[test]
fn parse_explicit_provider_custom() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model("groq/llama-3-70b", &aliases).unwrap();
    assert_eq!(provider, "groq");
    assert_eq!(model, "llama-3-70b");
}

#[test]
fn parse_infer_openai_gpt() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model("gpt-4o", &aliases).unwrap();
    assert_eq!(provider, "openai");
    assert_eq!(model, "gpt-4o");
}

#[test]
fn parse_infer_openai_o1() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model("o1-preview", &aliases).unwrap();
    assert_eq!(provider, "openai");
    assert_eq!(model, "o1-preview");
}

#[test]
fn parse_infer_openai_o3() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model("o3-mini", &aliases).unwrap();
    assert_eq!(provider, "openai");
    assert_eq!(model, "o3-mini");
}

#[test]
fn parse_infer_openai_o4() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model("o4-mini", &aliases).unwrap();
    assert_eq!(provider, "openai");
    assert_eq!(model, "o4-mini");
}

#[test]
fn parse_infer_anthropic_claude() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model("claude-sonnet-4-20250514", &aliases).unwrap();
    assert_eq!(provider, "anthropic");
    assert_eq!(model, "claude-sonnet-4-20250514");
}

#[test]
fn parse_unknown_model_errors() {
    let aliases = HashMap::new();
    let err = parse_model("llama-3-70b", &aliases).unwrap_err();
    assert!(matches!(err, ShimError::UnknownProvider(_)));
}

#[test]
fn parse_alias_resolves() {
    let mut aliases = HashMap::new();
    aliases.insert(
        "smart".to_string(),
        "anthropic/claude-sonnet-4-20250514".to_string(),
    );
    let (provider, model) = parse_model("smart", &aliases).unwrap();
    assert_eq!(provider, "anthropic");
    assert_eq!(model, "claude-sonnet-4-20250514");
}

#[test]
fn parse_alias_to_bare_model() {
    let mut aliases = HashMap::new();
    aliases.insert("default".to_string(), "gpt-4o".to_string());
    let (provider, model) = parse_model("default", &aliases).unwrap();
    assert_eq!(provider, "openai");
    assert_eq!(model, "gpt-4o");
}

#[test]
fn parse_alias_chain_does_not_recurse() {
    let mut aliases = HashMap::new();
    aliases.insert("a".to_string(), "b".to_string());
    aliases.insert("b".to_string(), "openai/gpt-4o".to_string());
    // "a" resolves to "b", but "b" is not re-resolved through aliases
    let err = parse_model("a", &aliases).unwrap_err();
    assert!(matches!(err, ShimError::UnknownProvider(_)));
}

#[test]
fn parse_empty_model_errors() {
    let aliases = HashMap::new();
    let err = parse_model("", &aliases).unwrap_err();
    assert!(matches!(err, ShimError::UnknownProvider(_)));
}

#[test]
fn parse_model_with_multiple_slashes() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model("azure/deployments/gpt-4/chat", &aliases).unwrap();
    assert_eq!(provider, "azure");
    assert_eq!(model, "deployments/gpt-4/chat");
}

// ============================================================
// Router tests
// ============================================================

#[test]
fn router_register_and_get() {
    let router = Router::new().register("openai", Box::new(OpenAi::new("test-key".into())));
    let provider = router.get("openai").unwrap();
    assert_eq!(provider.name(), "openai");
}

#[test]
fn router_get_unknown_errors() {
    let router = Router::new();
    assert!(matches!(
        router.get("openai"),
        Err(ShimError::UnknownProvider(_))
    ));
}

#[test]
fn router_alias_resolve() {
    let router = Router::new()
        .register("anthropic", Box::new(Anthropic::new("test-key".into())))
        .alias("smart", "anthropic/claude-sonnet-4-20250514");

    let (provider, model) = router.resolve("smart").unwrap();
    assert_eq!(provider.name(), "anthropic");
    assert_eq!(model, "claude-sonnet-4-20250514");
}

#[test]
fn router_resolve_explicit() {
    let router = Router::new().register("openai", Box::new(OpenAi::new("test-key".into())));
    let (provider, model) = router.resolve("openai/gpt-4o").unwrap();
    assert_eq!(provider.name(), "openai");
    assert_eq!(model, "gpt-4o");
}

#[test]
fn router_resolve_inferred() {
    let router = Router::new().register("openai", Box::new(OpenAi::new("test-key".into())));
    let (provider, model) = router.resolve("gpt-4o").unwrap();
    assert_eq!(provider.name(), "openai");
    assert_eq!(model, "gpt-4o");
}

#[test]
fn router_resolve_unregistered_provider_errors() {
    let router = Router::new();
    assert!(matches!(
        router.resolve("openai/gpt-4o"),
        Err(ShimError::UnknownProvider(_))
    ));
}

#[test]
fn router_multiple_aliases() {
    let router = Router::new()
        .register("openai", Box::new(OpenAi::new("k".into())))
        .register("anthropic", Box::new(Anthropic::new("k".into())))
        .alias("fast", "openai/gpt-4o-mini")
        .alias("smart", "anthropic/claude-sonnet-4-20250514")
        .alias("default", "openai/gpt-4o");

    let (p1, m1) = router.resolve("fast").unwrap();
    assert_eq!(p1.name(), "openai");
    assert_eq!(m1, "gpt-4o-mini");

    let (p2, m2) = router.resolve("smart").unwrap();
    assert_eq!(p2.name(), "anthropic");
    assert_eq!(m2, "claude-sonnet-4-20250514");

    let (p3, m3) = router.resolve("default").unwrap();
    assert_eq!(p3.name(), "openai");
    assert_eq!(m3, "gpt-4o");
}

// ============================================================
// Auto-detection — gemini and grok
// ============================================================

#[test]
fn parse_infer_gemini() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model("gemini-3-flash-preview", &aliases).unwrap();
    assert_eq!(provider, "gemini");
    assert_eq!(model, "gemini-3-flash-preview");
}

#[test]
fn parse_infer_grok() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model("grok-4-1-fast-reasoning", &aliases).unwrap();
    assert_eq!(provider, "xai");
    assert_eq!(model, "grok-4-1-fast-reasoning");
}

#[test]
fn parse_infer_case_insensitive() {
    let aliases = HashMap::new();
    let (p1, _) = parse_model("GPT-5.4", &aliases).unwrap();
    assert_eq!(p1, "openai");
    let (p2, _) = parse_model("Claude-Sonnet-4-6", &aliases).unwrap();
    assert_eq!(p2, "anthropic");
    let (p3, _) = parse_model("GEMINI-3-flash", &aliases).unwrap();
    assert_eq!(p3, "gemini");
    let (p4, _) = parse_model("Grok-4", &aliases).unwrap();
    assert_eq!(p4, "xai");
}

// ============================================================
// provider_keys
// ============================================================

#[test]
fn router_provider_keys() {
    let router = Router::new()
        .register("openai", Box::new(OpenAi::new("k".into())))
        .register("anthropic", Box::new(Anthropic::new("k".into())));
    let keys = router.provider_keys();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&"openai"));
    assert!(keys.contains(&"anthropic"));
}

#[test]
fn router_provider_keys_empty() {
    let router = Router::new();
    assert!(router.provider_keys().is_empty());
}

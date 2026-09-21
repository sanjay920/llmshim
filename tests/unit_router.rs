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
    let (provider, model) = parse_model("grok-4.3", &aliases).unwrap();
    assert_eq!(provider, "xai");
    assert_eq!(model, "grok-4.3");
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

#[test]
fn parse_openrouter_preserves_slug_with_slash() {
    // openrouter/<vendor>/<model>: split_once('/') keeps the vendor/model slug
    // (with its internal slash) intact as the model name.
    let aliases = HashMap::new();
    let (provider, model) =
        parse_model("openrouter/anthropic/claude-sonnet-4.5", &aliases).unwrap();
    assert_eq!(provider, "openrouter");
    assert_eq!(model, "anthropic/claude-sonnet-4.5");
}

#[test]
fn parse_openrouter_slug_with_variant_suffix() {
    let aliases = HashMap::new();
    let (provider, model) = parse_model(
        "openrouter/meta-llama/llama-3.1-70b-instruct:nitro",
        &aliases,
    )
    .unwrap();
    assert_eq!(provider, "openrouter");
    assert_eq!(model, "meta-llama/llama-3.1-70b-instruct:nitro");
}

#[test]
fn parse_vllm_and_sglang_preserve_hf_slug() {
    let aliases = HashMap::new();
    let (p, m) = parse_model("vllm/meta-llama/Llama-3.1-8B-Instruct", &aliases).unwrap();
    assert_eq!(p, "vllm");
    assert_eq!(m, "meta-llama/Llama-3.1-8B-Instruct");
    let (p, m) = parse_model("sglang/Qwen/Qwen3.6-35B-A3B-FP8", &aliases).unwrap();
    assert_eq!(p, "sglang");
    assert_eq!(m, "Qwen/Qwen3.6-35B-A3B-FP8");
}

#[test]
fn router_resolves_catalog_spellings_without_rerouting_reseller_models() {
    let router = Router::new()
        .register("anthropic", Box::new(Anthropic::new("test".into())))
        .register(
            "gemini",
            Box::new(llmshim::providers::gemini::Gemini::new("test".into())),
        )
        .register(
            "openrouter",
            Box::new(llmshim::providers::openrouter::OpenRouter::new(
                "test".into(),
            )),
        );
    let (p, name) = router.resolve("anthropic/claude-haiku-4.5").unwrap();
    assert_eq!(p.name(), "anthropic");
    assert_eq!(name, "claude-haiku-4-5");
    let (p, name) = router.resolve("google/gemini-3.8-flash").unwrap();
    assert_eq!(p.name(), "gemini");
    assert_eq!(name, "gemini-3.8-flash");
    let (p, name) = router
        .resolve("openrouter/anthropic/claude-haiku-4.5")
        .unwrap();
    assert_eq!(p.name(), "openrouter");
    assert_eq!(name, "anthropic/claude-haiku-4.5");
}

// ============================================================
// Named routes
// ============================================================

fn route(model: &str, settings: &[(&str, serde_json::Value)]) -> llmshim::config::Route {
    llmshim::config::Route {
        model: model.into(),
        settings: settings
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
    }
}

#[test]
fn a_named_route_resolves_to_its_model_and_stays_opaque_to_llmshim() {
    // "compaction" is the harness's word. llmshim only knows it maps to a
    // model — there is no role vocabulary anywhere below this line.
    let router = Router::new()
        .register("anthropic", Box::new(Anthropic::new("k".into())))
        .route(
            "compaction",
            route(
                "anthropic/claude-haiku-4-5-20251001",
                &[("reasoning_effort", serde_json::json!("low"))],
            ),
        );

    let (provider, model) = router.resolve("route/compaction").unwrap();
    assert_eq!(provider.name(), "anthropic");
    assert_eq!(model, "claude-haiku-4-5-20251001");
    assert_eq!(router.route_names(), vec!["compaction"]);
}

#[test]
fn an_unknown_route_is_an_error_not_a_silent_default() {
    let router = Router::new().register("anthropic", Box::new(Anthropic::new("k".into())));
    let err = match router.resolve("route/nope") {
        Ok(_) => panic!("an unknown route must not resolve"),
        Err(e) => e,
    };
    match err {
        ShimError::ProviderError {
            status, ref body, ..
        } => {
            assert_eq!(status, 400);
            assert!(body.contains("unknown named route"), "{body}");
        }
        other => panic!("expected a 400, got {other}"),
    }
}

#[test]
fn routes_do_not_chain() {
    let router = Router::new().route("a", route("route/b", &[]));
    assert!(router.resolve("route/a").is_err());
    assert!(router
        .expand_route(&serde_json::json!({"model": "route/a"}))
        .is_err());
}

#[test]
fn route_settings_are_defaults_and_the_request_wins() {
    let router = Router::new()
        .register("anthropic", Box::new(Anthropic::new("k".into())))
        .route(
            "cheap",
            route(
                "anthropic/claude-haiku-4-5-20251001",
                &[
                    ("reasoning_effort", serde_json::json!("low")),
                    ("max_tokens", serde_json::json!(4096)),
                ],
            ),
        );

    let request = serde_json::json!({
        "model": "route/cheap",
        "messages": [{"role": "user", "content": "hi"}],
        // The caller overrides one of the route's settings for this call.
        "reasoning_effort": "high",
    });
    let expanded = router.expand_route(&request).unwrap();
    assert_eq!(expanded["model"], "anthropic/claude-haiku-4-5-20251001");
    assert_eq!(expanded["reasoning_effort"], "high", "request overrides");
    assert_eq!(expanded["max_tokens"], 4096, "route fills the rest");
    assert_eq!(expanded["messages"], request["messages"]);

    // A request that names no route is untouched and not cloned.
    let plain = serde_json::json!({"model": "anthropic/claude-sonnet-5"});
    assert!(matches!(
        router.expand_route(&plain).unwrap(),
        std::borrow::Cow::Borrowed(_)
    ));
}

#[test]
fn routes_are_configuration_and_parse_from_the_config_file() {
    let config: llmshim::config::Config = toml::from_str(
        r#"
        [routes.compaction]
        model = "anthropic/claude-haiku-4-5-20251001"
        reasoning_effort = "low"
        max_tokens = 4096
        "#,
    )
    .unwrap();
    let route = &config.routes["compaction"];
    assert_eq!(route.model, "anthropic/claude-haiku-4-5-20251001");
    assert_eq!(route.settings["reasoning_effort"], "low");
    assert_eq!(route.settings["max_tokens"], 4096);
    assert!(
        !route.settings.contains_key("model"),
        "the target is not a setting"
    );
}

// ============================================================
// from_env / from_env_without_catalog_refresh
// ============================================================

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

fn catalog_offline() -> bool {
    std::env::var("LLMSHIM_CATALOG_OFFLINE").is_ok_and(|v| v == "1")
}

/// The refresh's one observable is the task it spawns. On a current-thread
/// runtime that task cannot run until this test yields, and the test never
/// does, so the fetch never starts even without `LLMSHIM_CATALOG_OFFLINE`.
///
/// Under CI's `LLMSHIM_CATALOG_OFFLINE=1` neither constructor may spawn, so
/// only the first assertion discriminates there; the `from_env` half is live
/// when a developer runs the tests without the variable.
#[tokio::test(flavor = "current_thread")]
async fn offline_constructor_spawns_no_refresh_and_from_env_still_does() {
    let before = alive_tasks();
    let router = Router::from_env_without_catalog_refresh();
    assert_eq!(
        alive_tasks(),
        before,
        "constructing the router spawned a task"
    );

    let expected = usize::from(!catalog_offline());
    let scheduled = router.refresh_catalog_in_background();
    assert_eq!(scheduled.is_some(), !catalog_offline());
    assert_eq!(alive_tasks(), before + expected);
    if let Some(task) = scheduled {
        task.abort();
    }

    let before = alive_tasks();
    let _ = Router::from_env();
    assert_eq!(
        alive_tasks(),
        before + expected,
        "from_env no longer refreshes"
    );
}

/// No refresh still means a catalog: the vendored snapshot resolves without
/// any network having happened.
#[test]
fn offline_constructor_resolves_a_bundled_model() {
    let router = Router::from_env_without_catalog_refresh()
        .register("anthropic", Box::new(Anthropic::new("test".into())));
    let (provider, model) = router.resolve("anthropic/claude-sonnet-5").unwrap();
    assert_eq!(provider.name(), "anthropic");
    assert_eq!(model, "claude-sonnet-5");
    assert!(llmshim::catalog::resolve("anthropic/claude-sonnet-5").is_some());
}

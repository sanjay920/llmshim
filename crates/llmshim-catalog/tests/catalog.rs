use chrono::Utc;
use llmshim_catalog::{
    Catalog, CatalogSource, ModelCapabilities, ModelFamily, ModelInfo, ReasoningOption, Support,
};
use serde_json::json;

fn feed(value: serde_json::Value) -> String {
    json!({"test": {"models": {"m": value}}}).to_string()
}

#[test]
fn grok_4_7_is_available_offline_and_4_6_remains_explicit() {
    let c = Catalog::vendored();
    for id in ["xai/grok-4.7", "openrouter/x-ai/grok-4.7"] {
        let m = c.resolve(id).unwrap();
        assert_eq!(m.family, Some(ModelFamily::Grok));
        assert_eq!(m.context_window_tokens, Some(500_000));
        assert_eq!(m.reasoning_options.len(), 1);
        assert_eq!(m.field_sources["family"], CatalogSource::Builtin);
        assert!(
            m.cost_for_input_tokens(200_001).unwrap().input.unwrap()
                > m.cost.unwrap().input.unwrap()
        );
    }
    assert_eq!(c.resolve("grok-4.7").unwrap().id, "xai/grok-4.7");
    assert!(llmshim_catalog::builtin::spec("grok-4.6").is_some());
    assert!(!llmshim_catalog::builtin::MODELS
        .iter()
        .any(|m| m.id == "xai/grok-4.6"));
    assert_eq!(
        llmshim_catalog::builtin::spec("grok-4.7")
            .unwrap()
            .max_output_tokens,
        None
    );
}

#[test]
fn context_price_tiers_select_by_full_input_and_inherit_missing_rates() {
    let mut c = Catalog::empty();
    c.merge_models_dev(
        &feed(
            json!({"cost":{"input":2,"output":6,"cache_read":0.5,"tiers":[
                {"tier":{"type":"context","size":400000},"output":18},
                {"tier":{"type":"context","size":200000},"input":4,"output":12,"cache_read":1}
            ]}}),
        ),
        None,
    )
    .unwrap();
    let m = c.resolve("test/m").unwrap();
    assert_eq!(m.cost_for_input_tokens(200_000), m.cost);
    let long = m.cost_for_input_tokens(200_001).unwrap();
    assert_eq!(
        (long.input, long.output, long.cache_read),
        (Some(4.0), Some(12.0), Some(1.0))
    );
    let longer = m.cost_for_input_tokens(400_001).unwrap();
    assert_eq!((longer.input, longer.output), (Some(4.0), Some(18.0)));
    let restored: ModelInfo = serde_json::from_value(serde_json::to_value(m).unwrap()).unwrap();
    assert_eq!(restored.cost_for_input_tokens(400_001), Some(longer));
}

#[test]
fn tier_policy_preserves_local_prices_and_ignores_provider_billing() {
    let mut c = Catalog::vendored();
    c.merge_local_toml("[models.\"xai/grok-4.7\".cost]\ninput=0.25")
        .unwrap();
    c.merge_provider_models(
        "xai",
        &json!({"data":[{"id":"grok-4.7","cost":{"input":999},"context_cost_tiers":[]}]}),
        Utc::now(),
    )
    .unwrap();
    let m = c.resolve("xai/grok-4.7").unwrap();
    let long = m.cost_for_input_tokens(300_000).unwrap();
    assert_eq!(long.input, Some(0.25));
    assert_eq!(long.output, Some(12.0));
    c.merge_local_toml("[models.\"xai/grok-4.7\"]\ncontext_cost_tiers=[]")
        .unwrap();
    c.merge_builtins();
    assert_eq!(
        c.resolve("xai/grok-4.7")
            .unwrap()
            .cost_for_input_tokens(300_000)
            .unwrap()
            .output,
        Some(6.0)
    );
    assert!(c
        .merge_local_toml("[models.\"xai/grok-4.7\"]\ncontext_cost_tiers=\"bad\"")
        .is_err());
}

#[test]
fn launch_metadata_wins_over_stale_community_data_in_either_order() {
    for builtins_first in [true, false] {
        let mut c = Catalog::empty();
        if builtins_first {
            c.merge_builtins();
        }
        c.merge_models_dev(&json!({"xai":{"models":{"grok-4.7":{"family":"gpt","cost":{"input":99},"context_cost_tiers":[]}}}}).to_string(),None).unwrap();
        if !builtins_first {
            c.merge_builtins();
        }
        let m = c.resolve("xai/grok-4.7").unwrap();
        assert_eq!(m.family, Some(ModelFamily::Grok));
        assert_eq!(m.cost_for_input_tokens(300_000).unwrap().input, Some(4.0));
    }
}

#[test]
fn absent_fields_are_unknown_and_zero_is_a_real_price() {
    let mut catalog = Catalog::empty();
    catalog
        .merge_models_dev(
            &feed(json!({"cost":{"input":0, "output":-1},"reasoning":false})),
            None,
        )
        .unwrap();
    let m = catalog.resolve("test/m").unwrap();
    assert_eq!(m.source, CatalogSource::ModelsDev);
    assert_eq!(m.capabilities.tools, Support::Unknown);
    assert_eq!(m.capabilities.reasoning, Support::Unsupported);
    assert_eq!(m.family, None);
    assert_eq!(m.cost.unwrap().input, Some(0.0));
    assert_eq!(m.cost.unwrap().output, None);
    assert_eq!(m.context_window_tokens, None);
    assert_eq!(m.knowledge_cutoff, None);
}

#[test]
fn verified_assertions_win_in_both_merge_orders_unknowns_fill() {
    for builtin_first in [true, false] {
        let mut catalog = Catalog::empty();
        let mut verified = ModelInfo::new("test", "m");
        verified.context_window_tokens = Some(42);
        verified.capabilities = ModelCapabilities::unknown().with_tools(Support::Unsupported);
        if builtin_first {
            catalog.merge_model(verified.clone());
        }
        let data = feed(
            json!({"limit":{"context":99,"output":7},"tool_call":true,"streaming":true,"cost":{"input":3}}),
        );
        catalog.merge_models_dev(&data, None).unwrap();
        if !builtin_first {
            catalog.merge_model(verified);
        }
        let m = catalog.resolve("test/m").unwrap();
        assert_eq!(m.context_window_tokens, Some(42));
        assert_eq!(m.max_output_tokens, Some(7));
        assert_eq!(m.capabilities.tools, Support::Unsupported);
        assert_eq!(m.capabilities.streaming, Support::Supported);
        assert_eq!(m.cost.unwrap().input, Some(3.0));
        assert_eq!(
            m.field_sources["context_window_tokens"],
            CatalogSource::Builtin
        );
        assert_eq!(
            m.field_sources["max_output_tokens"],
            CatalogSource::ModelsDev
        );
    }
}

#[test]
fn five_layer_precedence_applies_per_field() {
    let mut c = Catalog::empty();
    c.merge_models_dev(
        &feed(json!({"cost":{"input":1,"output":2},"limit":{"context":10}})),
        None,
    )
    .unwrap();
    c.merge_models_dev(
        &feed(json!({"cost":{"input":3},"limit":{"context":20}})),
        Some(Utc::now()),
    )
    .unwrap();
    let mut b = ModelInfo::new("test", "m");
    b.context_window_tokens = Some(30);
    b.capabilities.tools = Support::Supported;
    c.merge_model(b);
    c.merge_provider_models("test", &json!({"data":[{"id":"m","tool_call":false,"limit":{"context":40},"cost":{"output":999}}]}), Utc::now()).unwrap();
    c.merge_local_toml(
        "[models.\"test/m\"]\ncontext_window_tokens=50\n[models.\"test/m\".cost]\ninput=4",
    )
    .unwrap();
    c.merge_models_dev(
        &feed(json!({"cost":{"input":99,"output":8},"limit":{"context":99},"tool_call":true})),
        None,
    )
    .unwrap();
    let m = c.resolve("test/m").unwrap();
    assert_eq!(m.context_window_tokens, Some(50));
    assert_eq!(m.capabilities.tools, Support::Unsupported);
    assert_eq!(m.cost.unwrap().input, Some(4.0));
    assert_eq!(m.cost.unwrap().output, Some(8.0));
    assert_eq!(m.source, CatalogSource::Local);
    assert_eq!(
        m.field_sources["capabilities.tools"],
        CatalogSource::ProviderApi
    );
}

#[test]
fn local_model_and_alias_keep_provenance() {
    let mut c = Catalog::empty();
    c.merge_local_toml(
        r#"
[models."vllm/custom"]
label = "Private endpoint"
family = "qwen"
context_window_tokens = 12345
reasoning_options = [{type = "budget_tokens", min = 128, max = 2048}]
[models."vllm/custom".capabilities]
tools = "unsupported"
reasoning = "supported"
[aliases]
local = "vllm/custom"
"alias-chain" = "local"
"cycle-a" = "cycle-b"
"cycle-b" = "cycle-a"
"#,
    )
    .unwrap();
    let m = c.resolve("alias-chain").unwrap();
    assert_eq!(m.family, Some(ModelFamily::Qwen));
    assert_eq!(m.capabilities.tools, Support::Unsupported);
    assert!(matches!(
        m.reasoning_options[0],
        ReasoningOption::BudgetTokens {
            min: Some(128),
            max: Some(2048)
        }
    ));
    assert_eq!(m.source, CatalogSource::Local);
    assert!(c.resolve("cycle-a").is_none());
}

#[test]
fn bad_local_layer_is_atomic() {
    let mut c = Catalog::empty();
    assert!(c
        .merge_local_toml(
            "[models.\"a/good\"]\nfamily=\"qwen\"\n[models.\"z/bad\"]\nfamily=\"not-verified\""
        )
        .is_err());
    assert_eq!(c.models().count(), 0);
}

#[test]
fn provider_listing_never_introduces_prices_or_guessed_capabilities() {
    let mut c = Catalog::empty();
    c.merge_provider_models("vllm", &json!({"data":[{"id":"arbitrary","cost":{"input":100,"output":200},"knowledge":"2026-01-01","open_weights":false}]}), Utc::now()).unwrap();
    let m = c.resolve("vllm/arbitrary").unwrap();
    assert!(m.cost.is_none());
    assert!(m.open_weights.is_none());
    assert!(m.knowledge_cutoff.is_none());
    assert_eq!(m.capabilities, ModelCapabilities::unknown());
    assert!(m.family.is_none());
}

#[test]
fn known_spellings_resolve_without_rewriting_wire_identity() {
    let mut c = Catalog::vendored();
    let haiku = c.resolve("anthropic/claude-haiku-4.5").unwrap();
    assert!(haiku.name.starts_with("claude-haiku-4-5"));
    assert_eq!(
        c.resolve("google/gemini-3.8-flash").unwrap().id,
        "gemini/gemini-3.8-flash"
    );
    for region in ["us", "eu", "global"] {
        let provider = format!("region-{region}");
        let name = format!("{region}.anthropic.claude-haiku-4-5");
        c.merge_model(ModelInfo::new(&provider, &name));
        assert_eq!(
            c.resolve(&format!("{provider}/anthropic.claude-haiku-4-5"))
                .unwrap()
                .name,
            name
        );
    }
    c.merge_model(ModelInfo::new(
        "multi-region",
        "us.anthropic.claude-example",
    ));
    c.merge_model(ModelInfo::new(
        "multi-region",
        "eu.anthropic.claude-example",
    ));
    assert!(c.resolve("multi-region/anthropic.claude-example").is_none());
    assert_eq!(
        c.resolve("multi-region/eu.anthropic.claude-example")
            .unwrap()
            .name,
        "eu.anthropic.claude-example"
    );
}

#[test]
fn bare_aliases_do_not_choose_a_reseller_arbitrarily() {
    let mut c = Catalog::empty();
    c.merge_model(ModelInfo::new("first", "ambiguous"));
    c.merge_model(ModelInfo::new("second", "ambiguous"));
    assert!(c.resolve("ambiguous").is_none());
    c.alias("ambiguous", "first/ambiguous");
    assert_eq!(c.resolve("ambiguous").unwrap().provider, "first");
}

#[test]
fn catalog_options_keep_effort_and_budget_distinct_and_preserve_media() {
    let mut c = Catalog::empty();
    c.merge_models_dev(&feed(json!({"family":"claude-sonnet","reasoning_options":[{"type":"effort","values":["low","high","max"]},{"type":"budget_tokens","min":1024}],"modalities":{"input":["text","image","pdf"],"output":["text"]},"knowledge":"2025-08-31","release_date":"2026-02"})), None).unwrap();
    let m = c.resolve("test/m").unwrap();
    assert_eq!(m.family, Some(ModelFamily::Claude));
    assert!(matches!(
        m.reasoning_options[0],
        ReasoningOption::Effort { .. }
    ));
    assert!(matches!(
        m.reasoning_options[1],
        ReasoningOption::BudgetTokens {
            min: Some(1024),
            max: None
        }
    ));
    assert_eq!(m.modalities.input, ["text", "image", "pdf"]);
    assert_eq!(m.knowledge_cutoff.unwrap().to_string(), "2025-08-31");
    assert_eq!(m.release_date, None);
}

#[test]
fn vendored_artifact_has_broad_coverage_without_changing_discovery() {
    let c = Catalog::vendored();
    let providers: std::collections::BTreeSet<_> = c.models().map(|m| &m.provider).collect();
    assert!(providers.len() >= 200);
    assert!(c.models().count() >= 7000);
    assert_eq!(llmshim_catalog::builtin::MODELS.len(), 15);
    assert_eq!(llmshim_catalog::builtin::CHATGPT_MODELS.len(), 4);
    assert_eq!(c.resolve("gpt-6-astra").unwrap().provider, "openai");
    assert!(llmshim_catalog::builtin::all().all(|m| m.family.is_some()));
}

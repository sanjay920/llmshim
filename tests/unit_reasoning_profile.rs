use llmshim::{
    catalog::{
        EffortLevel::{High, Low, Max, Medium},
        ModelCapabilities, ReasoningOption, Support,
    },
    providers::anthropic_reasoning::Profile,
};
#[test]
fn catalog_options_choose_effort_budget_and_unsupported_without_model_names() {
    let p = Profile::from_options(
        ModelCapabilities::unknown(),
        &[ReasoningOption::BudgetTokens {
            min: Some(2048),
            max: Some(4096),
        }],
    );
    assert!(!p.adaptive());
    assert_eq!(p.budget("low", 4096).unwrap(), 2048);
    assert_eq!(p.budget("max", 16000).unwrap(), 4096);
    assert!(p.budget("high", 2048).is_err());
    let options = [
        ReasoningOption::Effort {
            values: vec![Low, High],
            default: None,
        },
        ReasoningOption::BudgetTokens {
            min: Some(1024),
            max: None,
        },
    ];
    let p = Profile::from_options(ModelCapabilities::unknown(), &options);
    assert!(p.adaptive());
    assert_eq!(p.effort("medium"), "high");
    assert_eq!(p.effort("max"), "high");
    assert!(!Profile::from_options(
        ModelCapabilities::unknown().with_reasoning(Support::Unsupported),
        &options
    )
    .supported());
}
#[test]
fn verified_reasoning_table_wins_over_community_and_unknown_models_are_not_guessed() {
    let catalog = llmshim::catalog::Catalog::vendored();
    let model = catalog.resolve("anthropic/claude-sonnet-4-6").unwrap();
    assert_eq!(
        model.reasoning_options,
        vec![ReasoningOption::Effort {
            values: vec![Low, Medium, High, Max],
            default: Some(High)
        }]
    );
    assert!(Profile::for_model("claude-sonnet-4-6-20250514").adaptive());
    assert!(!Profile::for_model("claude-opus-99-not-a-real-model").supported());
}

#[test]
fn haiku_5_5_uses_its_documented_effort_levels() {
    use llmshim::{provider::Provider, providers::anthropic::Anthropic};
    use serde_json::json;

    assert!(Profile::for_model("claude-haiku-5-5").adaptive());
    let provider = Anthropic::new("test-key".into());
    for effort in ["low", "medium", "high", "xhigh", "max"] {
        let request = json!({
            "messages": [{"role":"user", "content":"hello"}],
            "reasoning_effort": effort,
        });
        let body = provider
            .transform_request("claude-haiku-5-5", &request)
            .unwrap()
            .body;
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], effort);
    }
    let request = json!({"messages": [{"role":"user", "content":"hello"}]});
    let body = provider
        .transform_request("claude-haiku-5-5", &request)
        .unwrap()
        .body;
    assert!(body.get("thinking").is_none());
    assert!(body.get("output_config").is_none());
}

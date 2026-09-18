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
            values: vec![Low, Medium, High, Max]
        }]
    );
    assert!(Profile::for_model("claude-sonnet-4-6-20250514").adaptive());
    assert!(!Profile::for_model("claude-opus-99-not-a-real-model").supported());
}

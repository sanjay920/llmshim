use llmshim_catalog::{Catalog, CatalogSource, ModelFamily};
use serde_json::json;

const SUBSCRIPTION: &str = "chatgpt/gpt-6.1-sol";

#[test]
fn sol_subscription_default_preserves_provider_scope_and_unknown_billing_facts() {
    let catalog = Catalog::vendored();
    let subscription = catalog.resolve(SUBSCRIPTION).unwrap();
    assert_eq!(subscription.context_window_tokens, Some(500_000));
    assert_eq!(subscription.id, SUBSCRIPTION);
    assert_eq!(subscription.provider, "chatgpt");
    assert_eq!(subscription.name, "gpt-6.1-sol");
    assert_eq!(subscription.family, Some(ModelFamily::Gpt));
    assert_eq!(
        subscription.field_sources["context_window_tokens"],
        CatalogSource::Builtin
    );
    assert_eq!(subscription.max_output_tokens, None);
    assert_eq!(subscription.cost, None);

    let api = catalog.resolve("openai/gpt-6.1-sol").unwrap();
    assert_eq!(api.context_window_tokens, Some(1_050_000));
    assert_eq!(api.max_output_tokens, Some(128_000));
    assert!(api.cost.is_some());

    for id in [
        "chatgpt/gpt-6-astra",
        "chatgpt/gpt-6-sol",
        "chatgpt/gpt-6-luna",
    ] {
        let other = catalog.resolve(id).unwrap();
        assert_eq!(other.context_window_tokens, None, "{id}");
        assert_eq!(other.max_output_tokens, None, "{id}");
        assert_eq!(other.cost, None, "{id}");
    }
    assert!(catalog.resolve("chatgpt/gpt-6.1-sol-preview").is_none());
    assert!(catalog.resolve("another-provider/gpt-6.1-sol").is_none());
}

#[test]
fn subscription_default_and_local_capacity_keep_the_existing_merge_precedence() {
    let mut catalog = Catalog::vendored();
    let older_feed = json!({"chatgpt":{"models":{"gpt-6.1-sol":{
        "limit":{"context":272_000}
    }}}});
    catalog
        .merge_models_dev(&older_feed.to_string(), None)
        .unwrap();
    let subscription = catalog.resolve(SUBSCRIPTION).unwrap();
    assert_eq!(subscription.context_window_tokens, Some(500_000));
    assert_eq!(
        subscription.field_sources["context_window_tokens"],
        CatalogSource::Builtin
    );

    catalog
        .merge_local_toml(
            r#"[models."chatgpt/gpt-6.1-sol"]
context_window_tokens = 320000
"#,
        )
        .unwrap();
    catalog
        .merge_models_dev(&older_feed.to_string(), None)
        .unwrap();
    let subscription = catalog.resolve(SUBSCRIPTION).unwrap();
    assert_eq!(subscription.context_window_tokens, Some(320_000));
    assert_eq!(
        subscription.field_sources["context_window_tokens"],
        CatalogSource::Local
    );
    assert_eq!(subscription.max_output_tokens, None);
    assert_eq!(subscription.cost, None);
    assert_eq!(
        catalog
            .resolve("openai/gpt-6.1-sol")
            .unwrap()
            .context_window_tokens,
        Some(1_050_000)
    );
}

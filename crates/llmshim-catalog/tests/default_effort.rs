use llmshim_catalog::{Catalog, CatalogOptions, CatalogSource, ReasoningOption};
use serde_json::{json, Value};
use std::time::Duration;

fn options(catalog: &Catalog, id: &str) -> Value {
    serde_json::to_value(&catalog.resolve(id).unwrap().reasoning_options).unwrap()
}

#[test]
fn documented_defaults_survive_the_final_catalog_merge() {
    let catalog = Catalog::vendored();
    for (id, expected) in [
        ("anthropic/claude-haiku-5-5", "medium"),
        ("anthropic/claude-opus-5-5", "medium"),
        ("anthropic/claude-sonnet-5-5", "high"),
        ("anthropic/claude-opus-4-6", "high"),
        ("anthropic/claude-sonnet-4-6", "high"),
        ("anthropic/claude-opus-4-7", "high"),
        ("anthropic/claude-opus-4-8", "high"),
        ("anthropic/claude-opus-5", "high"),
        ("anthropic/claude-sonnet-5", "high"),
        ("anthropic/claude-fable-5", "high"),
        ("anthropic/claude-fable-5-1", "high"),
        ("gemini/gemini-3.8-flash", "medium"),
        ("gemini/gemini-3.6-flash", "medium"),
        ("gemini/gemini-3.5-flash-lite", "minimal"),
        ("gemini/gemini-3.1-flash-lite", "minimal"),
        ("gemini/gemini-3.1-flash-lite-image", "minimal"),
        ("gemini/gemini-3-flash-preview", "high"),
        ("openai/gpt-6-astra", "medium"),
        ("openai/gpt-6-sol", "medium"),
        ("openai/gpt-6-luna", "medium"),
        ("openai/gpt-5.6", "medium"),
        ("openai/gpt-5.6-sol", "medium"),
        ("xai/grok-4.5", "high"),
        ("xai/grok-4.6", "high"),
        ("xai/grok-4.7", "high"),
        ("cerebras/qwen-3.8-27b", "high"),
    ] {
        assert_eq!(options(&catalog, id)[0]["default"], expected, "{id}");
    }
    assert_eq!(
        options(&catalog, "anthropic/claude-haiku-5-5")[0]["values"],
        json!(["low", "medium", "high", "xhigh", "max"])
    );
    assert_eq!(
        options(&catalog, "cerebras/qwen-3.8-27b")[0]["values"],
        json!(["none", "low", "medium", "high"])
    );
    for id in ["openrouter/x-ai/grok-4.7", "openai/gpt-5.6-luna"] {
        assert!(options(&catalog, id)[0].get("default").is_none(), "{id}");
    }
    for model in catalog.models() {
        for option in serde_json::to_value(&model.reasoning_options)
            .unwrap()
            .as_array()
            .unwrap()
        {
            if let Some(default) = option.get("default") {
                assert!(
                    option["values"].as_array().unwrap().contains(default),
                    "{}",
                    model.id
                );
            }
        }
    }
}

#[test]
fn an_invalid_default_never_discards_valid_effort_choices() {
    for (default, expected) in [
        (Some(json!("medium")), Some(json!("medium"))),
        (Some(json!("none")), Some(json!("none"))),
        (None, None),
        (Some(Value::Null), None),
        (Some(json!("future")), None),
        (Some(json!("high")), None),
        (Some(json!(7)), None),
        (Some(json!({"level": "medium"})), None),
    ] {
        let mut wire = json!({"type":"effort", "values":["none", "low", "medium"]});
        if let Some(default) = default {
            wire["default"] = default;
        }
        let option: ReasoningOption = serde_json::from_value(wire.clone()).unwrap();
        let encoded = serde_json::to_value(&option).unwrap();
        assert_eq!(encoded["values"], wire["values"]);
        assert_eq!(encoded.get("default"), expected.as_ref(), "{wire}");
        assert_eq!(
            serde_json::from_value::<ReasoningOption>(encoded).unwrap(),
            option
        );
    }
}

#[test]
fn stronger_options_replace_the_default_atomically() {
    let mut catalog = Catalog::empty();
    catalog
        .merge_models_dev(
            &json!({"test":{"models":{"m":{"reasoning_options":[{
                "type":"effort", "values":["low", "medium"], "default":"medium"
            }]}}}})
            .to_string(),
            None,
        )
        .unwrap();
    assert_eq!(options(&catalog, "test/m")[0]["default"], "medium");
    catalog
        .merge_provider_models(
            "test",
            &json!({"data":[{
                "id":"m", "reasoning_options":[{"type":"effort", "values":["low"]}]
            }]}),
            chrono::Utc::now(),
        )
        .unwrap();
    assert_eq!(
        options(&catalog, "test/m"),
        json!([{"type":"effort", "values":["low"]}])
    );
    catalog
        .merge_local_toml(
            r#"
[models."test/m"]
reasoning_options = [{type="effort", values=["high"], default="high"}]
"#,
        )
        .unwrap();
    catalog
        .merge_provider_models(
            "test",
            &json!({"data":[{
                "id":"m", "reasoning_options":[{"type":"effort", "values":["low"]}]
            }]}),
            chrono::Utc::now(),
        )
        .unwrap();
    assert_eq!(options(&catalog, "test/m")[0]["default"], "high");
    assert_eq!(
        catalog.resolve("test/m").unwrap().field_sources["reasoning_options"],
        CatalogSource::Local
    );
}

#[test]
fn an_old_cache_keeps_choices_and_local_options_replace_builtin_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("catalog.json");
    let local = dir.path().join("models.toml");
    std::fs::write(
        &cache,
        json!({"test":{"models":{"old":{"reasoning_options":[{
            "type":"effort", "values":["low", "high"]
        }]}}}})
        .to_string(),
    )
    .unwrap();
    let settings = CatalogOptions {
        cache_file: Some(cache),
        global_overrides: Some(local.clone()),
        project_overrides: None,
        offline: false,
        ttl: Duration::from_secs(86400),
        url: "http://127.0.0.1:9".into(),
    };
    let catalog = Catalog::load(&settings).unwrap();
    assert_eq!(
        options(&catalog, "anthropic/claude-opus-5-5")[0]["default"],
        "medium"
    );
    assert_eq!(
        options(&catalog, "test/old"),
        json!([{"type":"effort", "values":["low", "high"]}])
    );
    std::fs::write(
        &local,
        r#"
[models."anthropic/claude-opus-5-5"]
reasoning_options = [{type="effort", values=["low"]}]
"#,
    )
    .unwrap();
    let restarted = Catalog::load(&settings).unwrap();
    assert_eq!(
        options(&restarted, "anthropic/claude-opus-5-5"),
        json!([{"type":"effort", "values":["low"]}])
    );
    assert_eq!(
        options(&restarted, "test/old"),
        options(&catalog, "test/old")
    );
}

#[test]
fn programmatic_models_cannot_publish_unlisted_defaults() {
    use llmshim_catalog::{EffortLevel, ModelInfo};

    let mut model = ModelInfo::new("test", "m");
    model.reasoning_options = vec![ReasoningOption::Effort {
        values: vec![EffortLevel::Low],
        default: Some(EffortLevel::High),
    }];
    let mut catalog = Catalog::empty();
    catalog.merge_model(model);
    assert_eq!(
        options(&catalog, "test/m"),
        json!([{"type":"effort", "values":["low"]}])
    );
}

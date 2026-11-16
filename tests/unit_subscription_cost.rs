//! This binary owns the process-wide catalog so local price fixtures stay isolated.

use llmshim::cost;
use serde_json::{json, Value};

#[test]
fn subscription_cost_uses_qualified_rates_and_preserves_provider_accounting() {
    let directory = tempfile::tempdir().unwrap();
    let overrides = directory.path().join("models.toml");
    std::fs::write(
        &overrides,
        "[models.\"chatgpt/gpt-6-sol\"]\ncost = { input = 3, output = 7 }\n",
    )
    .unwrap();
    std::env::set_var("XDG_CONFIG_HOME", directory.path());
    std::env::set_var("LLMSHIM_CATALOG_PROJECT", &overrides);
    std::env::set_var("LLMSHIM_CATALOG_OFFLINE", "1");

    let usage = json!({"uncached_input_tokens":5,"completion_tokens":1});
    assert_eq!(cost::cost_usd("chatgpt", "gpt-6.1-sol", &usage), None);
    assert!(cost::for_target("chatgpt", "gpt-6.1-sol").is_none());
    assert!(!cost::is_priceable("chatgpt", "gpt-6.1-sol"));
    let mut unknown = json!({"usage":usage.clone()});
    cost::stamp("chatgpt", "gpt-6.1-sol", &mut unknown);
    assert_eq!(unknown["usage"]["cost_usd"], Value::Null);
    assert_eq!(unknown["usage"]["cost_source"], "catalog");

    assert_eq!(
        cost::cost_usd("openai", "gpt-6.1-sol", &usage),
        Some(0.00002)
    );
    let qualified = cost::for_target("chatgpt", "gpt-6-sol").unwrap();
    assert_eq!(qualified.input, Some(3.0));
    assert_eq!(qualified.output, Some(7.0));
    assert!(cost::is_priceable("chatgpt", "gpt-6-sol"));
    let priced = cost::cost_usd("chatgpt", "gpt-6-sol", &usage).unwrap();
    assert!((priced - 0.000022).abs() < 1e-12);

    for model in ["gpt-6.1-sol", "gpt-6-sol"] {
        for reported in [0.0, 0.25] {
            let mut billed = json!({"usage":usage.clone()});
            billed["usage"]["cost"] = json!(reported);
            cost::stamp("chatgpt", model, &mut billed);
            assert_eq!(billed["usage"]["cost_usd"], reported);
            assert_eq!(billed["usage"]["cost_source"], "provider");
        }
    }
    let mut floor = json!({"usage":usage});
    floor["usage"]["provider_cost_floor_usd"] = json!(0.000001);
    cost::stamp("chatgpt", "gpt-6.1-sol", &mut floor);
    assert_eq!(floor["usage"]["cost_usd"], 0.000001);
    assert_eq!(floor["usage"]["cost_source"], "provider_floor");
}

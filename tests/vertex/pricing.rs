use super::*;
fn unknown(provider: &str, model: &str) {
    let mut response = json!({"usage":{"prompt_tokens":100,"completion_tokens":20}});
    llmshim::cost::stamp(provider, model, &mut response);
    assert!(response["usage"]["cost_usd"].is_null());
    assert_eq!(response["usage"]["cost_source"], "catalog");
    assert!(!llmshim::cost::is_priceable(provider, model));
    assert!(llmshim::cost::for_target(provider, model).is_none());
}
#[test]
fn vertex_gemini_does_not_borrow_direct_api_prices() {
    assert!(llmshim::cost::for_target("gemini", "gemini-3.8-flash").is_some());
    unknown("vertex-gemini", "gemini-3.8-flash");
}
#[test]
fn vertex_anthropic_does_not_borrow_direct_api_prices() {
    assert!(llmshim::cost::for_target("anthropic", "claude-sonnet-5-5").is_some());
    unknown("vertex-anthropic", "claude-sonnet-5-5");
    unknown("vertex-anthropic", "claude-sonnet-5-5@20261001");
}

//! Claude Haiku 5.5 constraints, from Anthropic's migration guide (2026-11).
use llmshim::{error::ShimError, provider::Provider, providers::anthropic::Anthropic};
use serde_json::json;

fn provider() -> Anthropic {
    Anthropic::new("test-key".into())
}

#[test]
fn sampling_parameters_are_omitted() {
    let body = provider()
        .transform_request(
            "claude-haiku-5-5",
            &json!({"messages": [{"role": "user", "content": "hi"}],
                "temperature": 0.2, "top_p": 0.9, "top_k": 5}),
        )
        .unwrap()
        .body;
    for key in ["temperature", "top_p", "top_k"] {
        assert!(body.get(key).is_none(), "{key} reached Haiku 5.5");
    }
}

#[test]
fn assistant_prefill_and_manual_thinking_are_local_400s() {
    let prefill = provider()
        .transform_request(
            "claude-haiku-5-5",
            &json!({"messages": [{"role": "user", "content": "hi"},
                {"role": "assistant", "content": "{"}]}),
        )
        .err()
        .unwrap();
    assert!(matches!(
        prefill,
        ShimError::ProviderError { status: 400, .. }
    ));
    let manual = provider()
        .transform_request(
            "claude-haiku-5-5",
            &json!({"messages": [{"role": "user", "content": "hi"}],
                "thinking": {"type": "enabled", "budget_tokens": 2048}}),
        )
        .err()
        .unwrap();
    assert!(matches!(
        manual,
        ShimError::ProviderError { status: 400, .. }
    ));
}

#[test]
fn thinking_can_be_disabled_and_forced_tools_are_allowed() {
    let none = provider()
        .transform_request(
            "claude-haiku-5-5",
            &json!({"messages": [{"role": "user", "content": "hi"}], "reasoning_effort": "none"}),
        )
        .unwrap()
        .body;
    assert_eq!(none["thinking"]["type"], "disabled");
    let forced = provider()
        .transform_request(
            "claude-haiku-5-5",
            &json!({"messages": [{"role": "user", "content": "hi"}], "tool_choice": "required",
                "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}]}),
        )
        .unwrap()
        .body;
    assert_eq!(forced["tool_choice"]["type"], "any");
}

#[test]
fn spec_and_price_are_published() {
    let spec = llmshim::models::spec("anthropic/claude-haiku-5-5").unwrap();
    assert_eq!(spec.context_window_tokens, Some(1_000_000));
    assert_eq!(spec.max_output_tokens, Some(128_000));
}

#[test]
fn refusal_is_a_content_filter_finish_reason() {
    let result = provider()
        .transform_response(
            "claude-haiku-5-5",
            json!({"content": [{"type": "text", "text": "Declined"}],
                "stop_reason": "refusal", "usage": {"input_tokens": 5, "output_tokens": 1}}),
        )
        .unwrap();
    assert_eq!(result["choices"][0]["finish_reason"], "content_filter");
    let chunk = provider()
        .transform_stream_chunk(
            "claude-haiku-5-5",
            &json!({"type": "message_delta", "delta": {"stop_reason": "refusal"}}).to_string(),
        )
        .unwrap()
        .unwrap();
    let chunk: serde_json::Value = serde_json::from_str(&chunk).unwrap();
    assert_eq!(chunk["choices"][0]["finish_reason"], "content_filter");
}

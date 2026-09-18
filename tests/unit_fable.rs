use llmshim::{error::ShimError, provider::Provider, providers::anthropic::Anthropic};
use serde_json::{json, Value};

fn provider() -> Anthropic {
    Anthropic::new("test-key".into())
}
const FABLES: [&str; 2] = ["claude-fable-5", "claude-fable-5-1"];

#[test]
fn fable_uses_always_on_adaptive_thinking_and_all_effort_levels() {
    for model in FABLES {
        for (effort, expected) in [
            ("none", "low"),
            ("minimal", "low"),
            ("low", "low"),
            ("medium", "medium"),
            ("high", "high"),
            ("xhigh", "xhigh"),
            ("max", "max"),
        ] {
            let req = provider()
                .transform_request(
                    model,
                    &json!({"messages": [{"role": "user", "content": "Hi"}],
                "reasoning_effort": effort, "max_tokens": 128}),
                )
                .unwrap();
            assert_eq!(req.body["thinking"]["type"], "adaptive");
            assert_eq!(req.body["output_config"]["effort"], expected);
            assert!(req.body["thinking"].get("budget_tokens").is_none());
            assert!(req
                .headers
                .iter()
                .all(|(name, value)| name != "anthropic-beta" || !value.contains("context-1m")));
        }
    }
}

#[test]
fn fable_and_opus5_strip_sampling_even_without_explicit_thinking() {
    for model in ["claude-fable-5", "claude-fable-5-1", "claude-opus-5"] {
        for extra in [
            json!({}),
            json!({"reasoning_effort": "none"}),
            json!({"thinking": {"type": "adaptive"}}),
        ] {
            let mut input = json!({"messages": [{"role": "user", "content": "Hi"}], "temperature": 0.4, "top_p": 0.5, "top_k": 10,
                "x-anthropic": {"temperature": 0.7, "top_p": 0.8, "top_k": 20}});
            input
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let req = provider().transform_request(model, &input).unwrap();
            for key in ["temperature", "top_p", "top_k"] {
                assert!(req.body.get(key).is_none(), "{model} {key}");
            }
        }
    }
}

#[test]
fn fable_rejects_explicit_disabled_or_manual_thinking() {
    for model in FABLES {
        for thinking in [
            json!({"type": "disabled"}),
            json!({"type": "enabled", "budget_tokens": 1024}),
        ] {
            for namespace in [false, true] {
                let mut input = json!({"messages": [{"role": "user", "content": "Hi"}]});
                if namespace {
                    input["x-anthropic"] = json!({"thinking": thinking});
                } else {
                    input["thinking"] = thinking.clone();
                }
                let error = provider().transform_request(model, &input).err().unwrap();
                assert!(matches!(
                    error,
                    ShimError::ProviderError { status: 400, .. }
                ));
                assert!(error.to_string().contains("adaptive thinking"));
            }
        }
    }
}

#[test]
fn fable51_rejects_forced_tools_without_silently_weakening_the_request() {
    for choice in [
        json!("required"),
        json!({"type": "function", "function": {"name": "lookup"}}),
        json!({"type": "any"}),
        json!({"type": "tool", "name": "lookup"}),
    ] {
        let input =
            json!({"messages": [{"role": "user", "content": "Use lookup"}], "tool_choice": choice});
        assert!(provider()
            .transform_request("claude-fable-5", &input)
            .is_ok());
        let error = provider()
            .transform_request("claude-fable-5-1", &input)
            .err()
            .unwrap();
        assert!(matches!(
            error,
            ShimError::ProviderError { status: 400, .. }
        ));
    }
    let error = provider()
        .transform_request(
            "claude-fable-5-1",
            &json!({"messages": [],
        "tool_choice": "auto", "x-anthropic": {"tool_choice": {"type": "any"}}}),
        )
        .err()
        .unwrap();
    assert!(matches!(
        error,
        ShimError::ProviderError { status: 400, .. }
    ));
    for choice in ["auto", "none"] {
        let req = provider()
            .transform_request(
                "claude-fable-5-1",
                &json!({"messages": [], "tool_choice": choice}),
            )
            .unwrap();
        assert_eq!(req.body["tool_choice"]["type"], choice);
    }
}

#[test]
fn fable_rejects_assistant_prefill_but_accepts_tool_results() {
    for model in FABLES {
        let error = provider()
            .transform_request(
                model,
                &json!({"messages": [{"role": "user", "content": "Hi"},
            {"role": "assistant", "content": "The answer is"}]}),
            )
            .err()
            .unwrap();
        assert!(matches!(
            error,
            ShimError::ProviderError { status: 400, .. }
        ));
        let req = provider().transform_request(model, &json!({"messages": [{"role":"user","content":"Weather?"},{"role":"assistant","tool_calls":[{"id":"call","function":{"name":"weather","arguments":"{}"}}]},{"role": "tool", "tool_call_id": "call", "content": "Sunny"}]})).unwrap();
        assert_eq!(req.body["messages"][2]["role"], "user");
        assert_eq!(req.body["messages"][2]["content"][0]["type"], "tool_result");
    }
}

#[test]
fn fable51_keeps_appended_system_turns_out_of_the_bound_initial_prompt() {
    let messages = json!([
        {"role": "system", "content": "Initial instruction"},
        {"role": "user", "content": "First question"},
        {"role": "assistant", "content": "Answer", "reasoning_content": "", "reasoning_signature": "opaque-signed-block", "reasoning_origin": provider().replay_target("claude-fable-5-1").origin()},
        {"role": "developer", "content": "Instruction for this turn"},
        {"role": "user", "content": "Second question"}
    ]);
    let result = provider()
        .transform_request("claude-fable-5-1", &json!({"messages": messages}))
        .unwrap();
    assert_eq!(result.body["system"], "Initial instruction");
    assert_eq!(
        result.body["messages"][2],
        json!({"role": "system", "content": "Instruction for this turn"})
    );
    assert_eq!(
        result.body["messages"][1]["content"][0],
        json!({"type": "thinking", "thinking": "", "signature": "opaque-signed-block"})
    );
    let older = provider()
        .transform_request("claude-opus-5", &json!({"messages": messages}))
        .unwrap();
    assert_eq!(
        older.body["system"],
        "Initial instruction\n\nInstruction for this turn"
    );
}

#[test]
fn refusal_has_a_content_filter_finish_reason_instead_of_success_or_502() {
    for model in ["claude-fable-5", "claude-fable-5-1", "claude-opus-5"] {
        let result = provider()
            .transform_response(
                model,
                json!({"content": [{"type": "text", "text": "Declined"}],
            "stop_reason": "refusal", "usage": {"input_tokens": 5, "output_tokens": 1}}),
            )
            .unwrap();
        assert_eq!(result["choices"][0]["finish_reason"], "content_filter");
        let stream = provider()
            .transform_stream_chunk(
                model,
                &json!({"type": "message_delta", "delta": {"stop_reason": "refusal"}}).to_string(),
            )
            .unwrap()
            .unwrap();
        let stream: Value = serde_json::from_str(&stream).unwrap();
        assert_eq!(stream["choices"][0]["finish_reason"], "content_filter");
    }
}

#[test]
fn fable_specs_include_both_versions_with_verified_limits() {
    for model in FABLES {
        let id = format!("anthropic/{model}");
        let entry = llmshim::models::spec(&id).unwrap();
        assert_eq!(entry.context_window_tokens, Some(1_000_000));
        assert_eq!(entry.max_output_tokens, Some(128_000));
    }
}

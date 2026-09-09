use llmshim::provider::Provider;
use llmshim::providers::{anthropic::Anthropic, gemini::Gemini, openai::OpenAi, xai::Xai};
use serde_json::{json, Value};

fn check(provider: &dyn Provider, template: Value, pointer: &str, bad: &[Value]) {
    for value in bad {
        let mut response = template.clone();
        *response.pointer_mut(pointer).unwrap() = value.clone();
        let error = provider
            .transform_response("test-model", response)
            .unwrap_err();
        assert!(!error.to_string().contains("private"));
    }
    let mut response = template;
    let (parent, key) = pointer.rsplit_once('/').unwrap();
    response
        .pointer_mut(parent)
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove(key);
    assert!(provider.transform_response("test-model", response).is_err());
}

#[test]
fn openai_nonterminal_or_missing_status_cannot_become_stop() {
    check_responses_api(&OpenAi::new("unused".into()));
}

#[test]
fn xai_nonterminal_or_missing_status_cannot_become_stop() {
    check_responses_api(&Xai::new("unused".into()));
}

fn check_responses_api(provider: &dyn Provider) {
    let response = json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"private partial text"}]}]});
    check(
        provider,
        response.clone(),
        "/status",
        &[
            json!("failed"),
            json!("cancelled"),
            json!("queued"),
            json!("in_progress"),
            json!("private-unknown"),
            Value::Null,
            json!(42),
        ],
    );
    for (status, expected) in [("completed", "stop"), ("incomplete", "length")] {
        let mut value = response.clone();
        value["status"] = json!(status);
        assert_eq!(
            provider.transform_response("test-model", value).unwrap()["choices"][0]
                ["finish_reason"],
            expected
        );
    }
}

#[test]
fn gemini_unknown_or_missing_reason_cannot_become_stop() {
    let provider = Gemini::new("unused".into());
    let response = json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"private partial text"}]}}]});
    check(
        &provider,
        response.clone(),
        "/candidates/0/finishReason",
        &[
            json!("RECITATION"),
            json!("MALFORMED_FUNCTION_CALL"),
            json!("OTHER"),
            json!("private-unknown"),
            Value::Null,
            json!(42),
        ],
    );
    for (reason, expected) in [
        ("STOP", "stop"),
        ("MAX_TOKENS", "length"),
        ("SAFETY", "content_filter"),
    ] {
        let mut value = response.clone();
        value["candidates"][0]["finishReason"] = json!(reason);
        assert_eq!(
            provider.transform_response("test-model", value).unwrap()["choices"][0]
                ["finish_reason"],
            expected
        );
    }
}

#[test]
fn anthropic_missing_or_unknown_reason_cannot_become_stop() {
    let provider = Anthropic::new("unused".into());
    let response =
        json!({"stop_reason":"end_turn","content":[{"type":"text","text":"private partial text"}]});
    check(
        &provider,
        response.clone(),
        "/stop_reason",
        &[
            json!("pause_turn"),
            json!("private-unknown"),
            Value::Null,
            json!(42),
        ],
    );
    for (reason, expected) in [
        ("end_turn", "stop"),
        ("stop_sequence", "stop"),
        ("max_tokens", "length"),
        ("tool_use", "tool_calls"),
    ] {
        let mut value = response.clone();
        value["stop_reason"] = json!(reason);
        assert_eq!(
            provider.transform_response("test-model", value).unwrap()["choices"][0]
                ["finish_reason"],
            expected
        );
    }
}

use super::*;
use serde_json::json;

#[test]
fn reasoning_controls_map_or_refuse_each_name() {
    let request = json!({"messages":[{"role":"user","content":"Hi"}],
        "max_tokens":4096,"reasoning_effort":"high","reasoning_mode":"pro",
        "reasoning_summary":"omitted"});
    let prepared = provider()
        .transform_request("anthropic.claude-sonnet-4-6", &request)
        .unwrap();
    assert_eq!(
        prepared.body["additionalModelRequestFields"]["thinking"]["type"],
        "adaptive"
    );
    for field in ["reasoning_effort", "reasoning_mode", "reasoning_summary"] {
        let mut request = json!({"messages":[{"role":"user","content":"Hi"}]});
        request[field] = json!("high");
        let error = provider()
            .transform_request("amazon.nova-pro-v1:0", &request)
            .err()
            .unwrap();
        assert!(error.to_string().contains(field));
    }
}

#[test]
fn tool_history_without_enabled_definitions_is_refused_locally() {
    let request: Value =
        serde_json::from_str(include_str!("../fixtures/bedrock/chat.json")).unwrap();
    for choice in [json!("none"), Value::Null] {
        let mut request = request.clone();
        request["tool_choice"] = choice;
        request["messages"] = json!([
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_1",
             "type":"function","function":{"name":"weather","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"sunny"}]);
        if request["tool_choice"].is_null() {
            request.as_object_mut().unwrap().remove("tools");
        }
        let error = provider()
            .transform_request("model", &request)
            .err()
            .unwrap();
        assert!(error.to_string().contains("toolConfig"));
    }
}

#[test]
fn null_controls_are_absent_and_malformed_reasoning_is_protocol_error() {
    let prepared = provider()
        .transform_request(
            "model",
            &json!({
        "messages":[{"role":"user","content":"Hi"}],"n":null,"temperature":null,
        "top_p":null,"max_tokens":null}),
        )
        .unwrap();
    assert!(prepared.body.get("inferenceConfig").is_none());
    let error = provider()
        .transform_response(
            "model",
            json!({"stopReason":"end_turn",
        "output":{"message":{"content":[{"reasoningContent":{}}]}}}),
        )
        .unwrap_err();
    assert_eq!(
        llmshim::providers::bedrock::BedrockError::from_shim(&error)
            .unwrap()
            .kind,
        llmshim::providers::bedrock::BedrockErrorKind::Protocol
    );
}

#[test]
fn lower_case_timeout_and_expired_tokens_are_typed() {
    use llmshim::providers::bedrock::{BedrockError, BedrockErrorKind};
    assert_eq!(
        BedrockError::service(408, "modelTimeoutException").kind,
        BedrockErrorKind::Timeout
    );
    assert_eq!(
        BedrockError::service(403, "ExpiredTokenException").kind,
        BedrockErrorKind::Credentials
    );
    assert_eq!(
        BedrockError::service(403, "UnrecognizedClientException").kind,
        BedrockErrorKind::Credentials
    );
}

#[test]
fn dated_and_geographic_anthropic_ids_use_budget_profile() {
    for model in [
        "anthropic.claude-3-7-sonnet-20250219-v1:0",
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
    ] {
        let prepared = provider()
            .transform_request(
                model,
                &json!({
            "messages":[{"role":"user","content":"Hi"}],
            "reasoning_effort":"high","reasoning_mode":"pro","max_tokens":4096}),
            )
            .unwrap();
        assert_eq!(
            prepared.body["additionalModelRequestFields"]["thinking"]["type"],
            "enabled"
        );
        assert_eq!(
            prepared.body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            3686
        );
    }
}

#[test]
fn native_thinking_wins_and_obeys_sampling_constraints() {
    for effort in [Value::Null, json!("high")] {
        let prepared = provider()
            .transform_request(
                "anthropic.claude-sonnet-4-6",
                &json!({
            "messages":[{"role":"user","content":"Hi"}], "reasoning_effort":effort,
            "temperature":0.2,"top_p":0.5,"x-bedrock":{"additionalModelRequestFields":{
            "thinking":{"type":"enabled","budget_tokens":1024}}}}),
            )
            .unwrap();
        assert_eq!(
            prepared.body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            1024
        );
        assert!(prepared.body["inferenceConfig"]
            .get("temperature")
            .is_none());
        assert!(prepared.body["inferenceConfig"].get("topP").is_none());
    }
}

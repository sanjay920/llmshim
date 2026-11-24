#![cfg(feature = "bedrock")]
//! Offline protocol contracts: no cloud endpoint or ambient credential is used.
use llmshim::{
    provider::Provider,
    providers::bedrock::{AwsProfile, Bedrock},
};
use serde_json::Value;

fn provider() -> Bedrock {
    Bedrock::new(AwsProfile::new("fixture", "us-east-1"))
}

#[test]
fn converse_recorded_request_and_response() {
    let request: Value = serde_json::from_str(include_str!("fixtures/bedrock/chat.json")).unwrap();
    let prepared = provider().transform_request("model:0", &request).unwrap();
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/bedrock/converse-request.json")).unwrap();
    assert_eq!(prepared.body, expected);
    assert_eq!(
        prepared.url,
        "https://bedrock-runtime.us-east-1.amazonaws.com/model/model%3A0/converse"
    );
    let response: Value =
        serde_json::from_str(include_str!("fixtures/bedrock/converse-response.json")).unwrap();
    let result = provider().transform_response("model:0", response).unwrap();
    assert_eq!(result["choices"][0]["message"]["content"], "Hello.");
    assert_eq!(result["usage"]["prompt_tokens"], 15);
    assert_eq!(result["usage"]["uncached_input_tokens"], 10);
    assert_eq!(result["usage"]["cache_read_tokens"], 3);
    assert_eq!(result["usage"]["cache_write_tokens"], 2);
    assert_eq!(result["usage"]["completion_tokens"], 4);
}

#[path = "bedrock/credentials.rs"]
mod credentials;
#[path = "bedrock/stream.rs"]
mod stream;

#[test]
fn service_errors_use_codes_and_status_never_message_text() {
    use llmshim::providers::bedrock::{BedrockError, BedrockErrorKind};
    let transport = provider().transport().unwrap();
    for (status, body, expected) in [
        (
            429,
            r#"{"message":"AccessDeniedException","__type":"ThrottlingException"}"#,
            BedrockErrorKind::Throttled,
        ),
        (
            403,
            r#"{"message":"ThrottlingException"}"#,
            BedrockErrorKind::AccessDenied,
        ),
        (
            400,
            r#"{"code":"ValidationException","message":"retry this request"}"#,
            BedrockErrorKind::Validation,
        ),
        (
            424,
            r#"{"__type":"ModelErrorException","originalStatusCode":500}"#,
            BedrockErrorKind::Model,
        ),
    ] {
        let error = transport.error(
            status,
            &reqwest::header::HeaderMap::new(),
            body.into(),
            None,
        );
        let classified = BedrockError::from_shim(&error).unwrap();
        assert_eq!(classified.kind, expected);
        assert_eq!(classified.status, status);
        let llmshim::error::ShimError::ProviderError { body, .. } = &error else {
            panic!()
        };
        let envelope: Value = serde_json::from_str(body).unwrap();
        assert_eq!(envelope["error"]["code"], classified.code);
        assert!(envelope["error"]["message"].is_string());
    }
}

#[test]
fn signed_reasoning_cannot_cross_profile_bindings() {
    use serde_json::json;
    let provider = provider();
    let model = "anthropic.claude-sonnet-4-6";
    let normalized = provider
        .transform_response(
            model,
            json!({"stopReason":"end_turn",
        "output":{"message":{"role":"assistant","content":[
            {"reasoningContent":{"reasoningText":{"text":"private reasoning","signature":"sig"}}},
            {"text":"answer"}]}},"usage":{"inputTokens":1,"outputTokens":1}}),
        )
        .unwrap();
    let request = json!({"messages":[{"role":"user","content":"question"},
        normalized["choices"][0]["message"],{"role":"user","content":"continue"}]});
    assert!(provider
        .transform_request(model, &request)
        .unwrap()
        .body
        .to_string()
        .contains("private reasoning"));
    let other = Bedrock::new(AwsProfile::new("other", "us-east-1"));
    assert!(!other
        .transform_request(model, &request)
        .unwrap()
        .body
        .to_string()
        .contains("private reasoning"));
}

#[test]
fn modeled_malformed_output_is_a_model_failure_not_a_protocol_guess() {
    use llmshim::providers::bedrock::{BedrockError, BedrockErrorKind};
    for stop in ["malformed_model_output", "malformed_tool_use"] {
        let native = serde_json::json!({"stopReason":stop,"output":{"message":{
            "role":"assistant","content":[{"text":"partial"}]}},
            "usage":{"inputTokens":1,"outputTokens":1}});
        let error = provider().transform_response("model", native).unwrap_err();
        assert_eq!(
            BedrockError::from_shim(&error).unwrap().kind,
            BedrockErrorKind::Model
        );
    }
}

#[path = "bedrock/review.rs"]
mod review;

#[path = "bedrock/credential_review.rs"]
mod credential_review;

#[path = "bedrock/round2.rs"]
mod round2;

#[path = "bedrock/bounds.rs"]
mod bounds;

#[path = "bedrock/unsigned.rs"]
mod unsigned;

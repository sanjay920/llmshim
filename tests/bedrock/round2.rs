use super::*;
use serde_json::json;

fn history(results: Value) -> Value {
    let mut messages = vec![
        json!({"role":"user","content":"weather"}),
        json!({"role":"assistant","content":null,"tool_calls":[
            {"id":"a","type":"function","function":{"name":"weather","arguments":"{}"}},
            {"id":"b","type":"function","function":{"name":"weather","arguments":"{}"}}]}),
    ];
    messages.extend(results.as_array().unwrap().iter().cloned());
    json!({"messages":messages,"tools":[{"type":"function","function":{"name":"weather",
        "parameters":{"type":"object","properties":{}}}}]})
}
#[test]
fn parallel_tool_results_share_one_converse_turn_in_order() {
    let request = history(json!([
        {"role":"tool","tool_call_id":"a","content":"sunny"},
        {"role":"tool","tool_call_id":"b","content":"warm"}]));
    let body = provider()
        .transform_request("model", &request)
        .unwrap()
        .body;
    assert_eq!(body["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        body["messages"][2],
        json!({"role":"user","content":[
        {"toolResult":{"toolUseId":"a","content":[{"text":"sunny"}]}},
        {"toolResult":{"toolUseId":"b","content":[{"text":"warm"}]}}]})
    );
}
#[test]
fn tool_results_then_user_text_share_one_converse_turn() {
    let request = history(json!([
        {"role":"tool","tool_call_id":"a","content":"sunny"},
        {"role":"tool","tool_call_id":"b","content":"warm"},
        {"role":"user","content":"Summarize."}]));
    let body = provider()
        .transform_request("model", &request)
        .unwrap()
        .body;
    assert_eq!(body["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        body["messages"][2]["content"][2],
        json!({"text":"Summarize."})
    );
}
#[test]
fn tool_text_parts_are_content_blocks_and_nontext_is_refused() {
    let mut request = history(json!([
        {"role":"tool","tool_call_id":"a","content":[{"type":"text","text":"sunny"}]},
        {"role":"tool","tool_call_id":"b","content":"warm"}]));
    let body = provider()
        .transform_request("model", &request)
        .unwrap()
        .body;
    assert_eq!(
        body["messages"][2]["content"][0]["toolResult"]["content"],
        json!([{"text":"sunny"}])
    );
    request["messages"][2]["content"] = json!([{"type":"image_url","image_url":{"url":"x"}}]);
    let error = provider()
        .transform_request("model", &request)
        .err()
        .unwrap();
    assert!(error.to_string().contains("image_url"));
    request["messages"][2]["content"] = Value::Null;
    assert!(provider().transform_request("model", &request).is_err());
}
#[test]
fn tls_default_provider_is_unambiguous() {
    let _config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(tokio_rustls::rustls::RootCertStore::empty())
        .with_no_client_auth();
}
#[test]
fn summary_or_mode_alone_never_enables_thinking() {
    for field in ["reasoning_summary", "reasoning_mode"] {
        let mut request = json!({"messages":[{"role":"user","content":"Hi"}]});
        request[field] = json!(if field == "reasoning_summary" {
            "none"
        } else {
            "standard"
        });
        let body = provider()
            .transform_request("us.anthropic.claude-sonnet-4-5-20250929-v1:0", &request)
            .unwrap()
            .body;
        assert!(body
            .pointer("/additionalModelRequestFields/thinking")
            .is_none());
    }
}
#[test]
fn adaptive_only_clamps_none_and_refuses_forced_tools() {
    let mut request = json!({"messages":[{"role":"user","content":"Hi"}],
        "reasoning_effort":"none","temperature":0.2,"top_p":0.3,
        "x-bedrock":{"additionalModelRequestFields":{"top_k":20}}});
    let model = "global.anthropic.claude-opus-5-5-v1:0";
    let body = provider().transform_request(model, &request).unwrap().body;
    assert_eq!(
        body["additionalModelRequestFields"]["thinking"]["type"],
        "adaptive"
    );
    assert_eq!(
        body["additionalModelRequestFields"]["output_config"]["effort"],
        "low"
    );
    assert!(body["inferenceConfig"].get("temperature").is_none());
    assert!(body["inferenceConfig"].get("topP").is_none());
    assert!(body["additionalModelRequestFields"].get("top_k").is_none());
    request["tools"] = json!([{"type":"function","function":{"name":"weather",
        "parameters":{"type":"object","properties":{}}}}]);
    request["tool_choice"] = json!("required");
    assert!(provider().transform_request(model, &request).is_err());
}
#[test]
fn signature_errors_never_expose_echoed_credentials() {
    use llmshim::providers::bedrock::{BedrockError, BedrockErrorKind};
    for code in ["InvalidSignatureException", "SignatureDoesNotMatch"] {
        let error = provider().transport().unwrap().error(
            403,
            &Default::default(),
            json!({"__type":code,"message":"x-amz-security-token:SYNTHETIC_SECRET",
                "nested":{"canonical":"SYNTHETIC_SECRET"}})
            .to_string(),
            None,
        );
        assert_eq!(
            BedrockError::from_shim(&error).unwrap().kind,
            BedrockErrorKind::Credentials
        );
        assert!(!error.to_string().contains("SYNTHETIC_SECRET"));
    }
}
#[test]
fn arn_is_encoded_by_transform_not_by_the_caller() {
    let request = provider()
        .transform_request(
            "arn:aws:bedrock:us-east-1:example:inference-profile/test",
            &json!({"messages":[{"role":"user","content":"Hi"}]}),
        )
        .unwrap();
    assert_eq!(
        request.url,
        concat!(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/",
            "arn%3Aaws%3Abedrock%3Aus-east-1%3Aexample%3Ainference-profile%2Ftest/converse"
        )
    );
}

#[test]
fn invalid_regions_and_unsupported_partitions_are_refused_before_dispatch() {
    for region in [
        "us-east-1/elsewhere",
        "us-east-1.example",
        "US-EAST-1",
        "",
        "us-iso-east-1",
        "us-isob-east-1",
        "eu-isoe-west-1",
        "us-isof-east-1",
        "eusc-de-east-1",
    ] {
        let provider = Bedrock::new(AwsProfile::new("fixture", region));
        assert!(
            provider
                .transform_request("model", &json!({"messages":[]}))
                .is_err(),
            "{region}"
        );
    }
    for (region, suffix) in [
        ("cn-north-1", "amazonaws.com.cn"),
        ("us-gov-west-1", "amazonaws.com"),
        ("us-east-1", "amazonaws.com"),
    ] {
        let provider = Bedrock::new(AwsProfile::new("fixture", region));
        let request = provider
            .transform_request("model", &json!({"messages":[]}))
            .unwrap();
        assert_eq!(
            request.url,
            format!("https://bedrock-runtime.{region}.{suffix}/model/model/converse")
        );
    }
}
#[test]
fn empty_assistant_text_is_omitted_and_missing_tool_schema_defaults_to_object() {
    let request = history(json!([
        {"role":"tool","tool_call_id":"a","content":"sunny"},
        {"role":"tool","tool_call_id":"b","content":"warm"}]));
    let mut request = request;
    request["messages"][1]["content"] = json!("");
    request["tools"][0]["function"]
        .as_object_mut()
        .unwrap()
        .remove("parameters");
    let body = provider()
        .transform_request("model", &request)
        .unwrap()
        .body;
    assert!(body["messages"][1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .all(|b| b.get("text").is_none()));
    assert_eq!(
        body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"],
        "object"
    );
    request["messages"][0]["content"] = json!([
        {"type":"text","text":"weather","cache_control":{"type":"ephemeral"}}]);
    assert!(provider().transform_request("model", &request).is_err());
}
#[test]
fn models_that_always_think_strip_sampling_without_an_effort_setting() {
    for model in [
        "anthropic.claude-fable-5-1-v1:0",
        "anthropic.claude-opus-5-v1:0",
    ] {
        let body = provider()
            .transform_request(
                model,
                &json!({
            "messages":[{"role":"user","content":"Hi"}],"temperature":0.2,"top_p":0.5,
            "x-bedrock":{"additionalModelRequestFields":{"top_k":20}}}),
            )
            .unwrap()
            .body;
        assert!(body["inferenceConfig"].get("temperature").is_none());
        assert!(body["inferenceConfig"].get("topP").is_none());
        assert!(body["additionalModelRequestFields"].get("top_k").is_none());
    }
}

#[test]
fn native_sampling_cannot_bypass_shared_constraints() {
    for model in [
        "anthropic.claude-fable-5-1-v1:0",
        "anthropic.claude-opus-5-v1:0",
    ] {
        let body = provider()
            .transform_request(
                model,
                &json!({
            "messages":[{"role":"user","content":"Hi"}],"x-bedrock":{
            "additionalModelRequestFields":{"temperature":0.2,"top_p":0.3,"top_k":10}}}),
            )
            .unwrap()
            .body;
        for field in ["temperature", "top_p", "top_k"] {
            assert!(
                body["additionalModelRequestFields"].get(field).is_none(),
                "{field}"
            );
        }
    }
}

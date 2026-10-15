use super::*;
fn thinking(provider: &Vertex, model: &str) -> Value {
    provider
        .transform_response(
            model,
            json!({"id":"fixture","type":"message",
        "role":"assistant","model":model,"content":[
            {"type":"thinking","thinking":"private reasoning","signature":"fixture-signature"},
            {"type":"text","text":"Answer"}],"stop_reason":"end_turn",
        "usage":{"input_tokens":1,"output_tokens":2}}),
        )
        .unwrap()
}
fn history(response: &Value) -> Value {
    json!({"messages":[{"role":"user","content":"Question"},
        response["choices"][0]["message"],{"role":"user","content":"Continue"}]})
}
#[test]
fn signed_vertex_thinking_replays_only_to_compatible_identity_and_model() {
    let model = "claude-sonnet-5-5@20261001";
    let provider = Vertex::anthropic(identity("global"));
    let response = thinking(&provider, model);
    assert_eq!(response["model"], model);
    let request = history(&response);
    assert!(provider
        .transform_request(model, &request)
        .unwrap()
        .body
        .to_string()
        .contains("fixture-signature"));
    for (project, region, binding, target_model) in [
        ("fixture-project", "global", "other", model),
        ("other-project", "global", "fixture", model),
        ("fixture-project", "us-central1", "fixture", model),
        (
            "fixture-project",
            "global",
            "fixture",
            "claude-sonnet-5@20260101",
        ),
    ] {
        let other = Vertex::anthropic(GoogleIdentity::token_source(
            project,
            region,
            binding,
            Arc::new(Fixed),
        ));
        let body = other
            .transform_request(target_model, &request)
            .unwrap()
            .body;
        assert!(
            !body.to_string().contains("fixture-signature"),
            "{project}/{region}/{binding}"
        );
    }
    let direct = llmshim::providers::anthropic::Anthropic::new(String::new());
    assert!(!direct
        .transform_request("claude-sonnet-5-5", &request)
        .unwrap()
        .body
        .to_string()
        .contains("fixture-signature"));
    let earlier = thinking(&provider, "claude-sonnet-5@20260101");
    assert!(provider
        .transform_request(model, &history(&earlier))
        .unwrap()
        .body
        .to_string()
        .contains("fixture-signature"));
}
#[test]
fn vertex_response_model_keeps_its_version_for_unary_and_manual_stream() {
    let provider = Vertex::anthropic(identity("global"));
    let model = "claude-sonnet-4-5@20250929";
    assert_eq!(thinking(&provider, model)["model"], model);
    let event = json!({"type":"message_start","message":{"id":"fixture","type":"message",
        "role":"assistant","model":model,"content":[],"usage":{"input_tokens":1,
        "output_tokens":0}}})
    .to_string();
    let manual: Value = serde_json::from_str(
        &provider
            .transform_stream_chunk(model, &event)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let normalized: Value = serde_json::from_str(
        &provider
            .stream_normalizer(model)
            .push(&event)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(manual["model"], model);
    assert_eq!(normalized["model"], model);
}
#[test]
fn vertex_tool_signature_survives_matching_target_and_is_removed_for_foreign_target() {
    let provider = Vertex::gemini(identity("global"));
    let model = "gemini-2.5-pro";
    let native = serde_json::from_str(include_str!("../fixtures/vertex/gemini.json")).unwrap();
    let response = provider.transform_response(model, native).unwrap();
    let message = &response["choices"][0]["message"];
    let request = json!({"messages":[{"role":"user","content":"Weather"},message,
        {"role":"tool","tool_call_id":message["tool_calls"][0]["id"],"content":"sunny"},
        {"role":"user","content":"Explain"}],"tools":[{"type":"function","function":{
        "name":"weather","parameters":{"type":"object","properties":{}}}}]});
    assert!(provider
        .transform_request(model, &request)
        .unwrap()
        .body
        .to_string()
        .contains("Zml4dHVyZQ=="));
    let other = Vertex::gemini(GoogleIdentity::token_source(
        "fixture-project",
        "global",
        "other",
        Arc::new(Fixed),
    ));
    assert!(!other
        .transform_request(model, &request)
        .unwrap()
        .body
        .to_string()
        .contains("Zml4dHVyZQ=="));
}

#[cfg(feature = "signature-introspection")]
#[test]
fn vertex_signature_observation_compares_canonical_models_without_changing_origin() {
    use base64::Engine;
    let field = |tag, data: Vec<u8>| [vec![tag, data.len() as u8], data].concat();
    let signature = base64::engine::general_purpose::STANDARD.encode(field(
        18,
        field(10, field(50, b"claude-sonnet-4-6".to_vec())),
    ));
    let model = "claude-sonnet-4-6@20260101";
    let provider = Vertex::anthropic(identity("global"));
    let response = provider
        .transform_response(
            model,
            json!({"type":"message","role":"assistant",
        "content":[{"type":"thinking","thinking":"brief","signature":signature},
        {"type":"text","text":"Answer"}],"stop_reason":"end_turn",
        "usage":{"input_tokens":1,"output_tokens":2}}),
        )
        .unwrap();
    assert!(response.get("x-llmshim-served-model").is_none());
    assert_eq!(
        response["choices"][0]["message"]["reasoning"][0]["origin"]["model"],
        model
    );
}

#[test]
fn versioned_anthropic_refusals_match_in_unary_manual_and_normalized_streams() {
    for model in ["claude-opus-5@20260101", "claude-fable-5-1@20260101"] {
        let provider = Vertex::anthropic(identity("global"));
        let response = provider
            .transform_response(
                model,
                json!({"type":"message",
            "role":"assistant","content":[],"stop_reason":"refusal",
            "usage":{"input_tokens":1,"output_tokens":0}}),
            )
            .unwrap();
        assert_eq!(response["choices"][0]["finish_reason"], "content_filter");
        let event = json!({"type":"message_delta","delta":{"stop_reason":"refusal"},
            "usage":{"output_tokens":0}})
        .to_string();
        let manual: Value = serde_json::from_str(
            &provider
                .transform_stream_chunk(model, &event)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(manual["choices"][0]["finish_reason"], "content_filter");
        let mut normalizer = provider.stream_normalizer(model);
        let normalized: Value =
            serde_json::from_str(&normalizer.push(&event).unwrap().unwrap()).unwrap();
        assert_eq!(normalized["choices"][0]["finish_reason"], "content_filter");
        assert_eq!(normalized["model"], model);
    }
}

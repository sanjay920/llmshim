use llmshim::{provider::Provider, providers::openai::OpenAi, providers::xai::Xai};
use serde_json::json;

#[test]
fn named_tool_selection_is_flat_for_all_accepted_input_dialects() {
    let p = Xai::new("test-key".into());
    for choice in [
        json!({"type":"function","function":{"name":"lookup"}}),
        json!({"type":"function","name":"lookup"}),
        json!({"type":"tool","name":"lookup"}),
    ] {
        let req = json!({"messages":[{"role":"user","content":"lookup"}],"tool_choice":choice});
        assert_eq!(
            p.transform_request("grok-4.7", &req).unwrap().body["tool_choice"],
            json!({"type":"function","name":"lookup"})
        );
    }
}

#[test]
fn ciphertext_survives_4_7_round_trip_without_mutating_history() {
    let p = Xai::new("test-key".into());
    let payload = json!({"type":"reasoning","id":"rs_synthetic","encrypted_content":"synthetic-ciphertext+/=","summary":[]});
    let result = p.transform_response("grok-4.7",json!({"id":"resp_synthetic","status":"completed","output":[payload,{"type":"message","role":"assistant","content":[{"type":"output_text","text":"pong"}]}],"usage":{"input_tokens":1,"output_tokens":2}})).unwrap();
    let assistant = &result["choices"][0]["message"];
    assert_eq!(assistant["reasoning"][0]["origin"]["family"], "grok");
    assert_eq!(assistant["reasoning"][0]["origin"]["model"], "grok-4.7");
    let req = json!({"messages":[{"role":"user","content":"ping"},assistant,{"role":"user","content":"continue"}],"reasoning_effort":"none"});
    let saved = req.clone();
    let wire = p.transform_request("grok-4.7", &req).unwrap();
    assert_eq!(wire.body["reasoning"]["effort"], "low");
    assert_eq!(wire.body["store"], false);
    assert!(wire.body["input"].as_array().unwrap().contains(&payload));
    let foreign = OpenAi::new("other-key".into())
        .transform_request("gpt-6-astra", &req)
        .unwrap();
    assert!(!foreign.body["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["type"] == "reasoning"));
    assert_eq!(req, saved);
}

#[test]
fn grok_4_7_effort_mapping_preserves_valid_tiers() {
    let p = Xai::new("test-key".into());
    for (effort, expected) in [
        ("none", "low"),
        ("low", "low"),
        ("medium", "medium"),
        ("high", "high"),
        ("xhigh", "xhigh"),
        ("max", "xhigh"),
    ] {
        let req = json!({"messages":[{"role":"user","content":"ping"}],"reasoning_effort":effort});
        assert_eq!(
            p.transform_request("grok-4.7", &req).unwrap().body["reasoning"]["effort"],
            expected
        );
    }
}

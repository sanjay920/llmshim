//! Prefix-bound Anthropic thinking: which model may read whose blocks.
//!
//! Opus 5.5 and Sonnet 5.5 both bind thinking to the conversation prefix, but
//! their documented readers differ. The provider records the producing model on
//! every block; this file pins the replay direction both ways.
use llmshim::{provider::Provider, providers::anthropic::Anthropic};
use serde_json::{json, Value};

fn request(message: Value) -> Value {
    json!({"messages":[{"role":"user","content":"question"},message,{"role":"user","content":"continue"}]})
}

fn anthropic_native() -> Value {
    json!({"id":"a1","stop_reason":"end_turn","content":[
    {"type":"thinking","thinking":"first\nthought","signature":"first-sig+/="},
    {"type":"redacted_thinking","data":"opaque+/=="},
    {"type":"thinking","thinking":"second","signature":"second-sig"},
    {"type":"text","text":"answer"}
],"usage":{}})
}

#[test]
fn opus_5_5_thinking_replays_only_to_documented_compatible_models() {
    let provider = Anthropic::new("key".into());
    let response = provider
        .transform_response("claude-opus-5-5", anthropic_native())
        .unwrap();
    let assistant_message = response["choices"][0]["message"].clone();
    assert_eq!(
        assistant_message["reasoning"][0]["origin"]["model"],
        "claude-opus-5-5"
    );

    let fable_request = provider
        .transform_request("claude-fable-5-1", &request(assistant_message.clone()))
        .unwrap();
    assert_eq!(
        fable_request.body["messages"][1]["content"],
        anthropic_native()["content"]
    );

    let older_request = provider
        .transform_request("claude-opus-5", &request(assistant_message.clone()))
        .unwrap();
    assert_eq!(
        older_request.body["messages"][1]["content"],
        json!("answer")
    );
    assert_eq!(
        assistant_message["reasoning"][0]["origin"]["model"],
        "claude-opus-5-5"
    );

    let fable_response = provider
        .transform_response("claude-fable-5-1", anthropic_native())
        .unwrap();
    let opus_request = provider
        .transform_request(
            "claude-opus-5-5",
            &request(fable_response["choices"][0]["message"].clone()),
        )
        .unwrap();
    assert_eq!(opus_request.body["messages"][1]["content"], json!("answer"));
}

#[test]
fn sonnet_5_5_thinking_replays_only_to_sonnet_5_5() {
    let provider = Anthropic::new("key".into());
    let response = provider
        .transform_response("claude-sonnet-5-5", anthropic_native())
        .unwrap();
    let assistant_message = response["choices"][0]["message"].clone();
    assert_eq!(
        assistant_message["reasoning"][0]["origin"]["model"],
        "claude-sonnet-5-5"
    );

    // Sonnet 5.5 blocks are readable only by Sonnet 5.5.
    let sonnet_request = provider
        .transform_request("claude-sonnet-5-5", &request(assistant_message.clone()))
        .unwrap();
    assert_eq!(
        sonnet_request.body["messages"][1]["content"],
        anthropic_native()["content"]
    );
    for other in ["claude-sonnet-5", "claude-opus-5-5", "claude-fable-5-1"] {
        let dropped = provider
            .transform_request(other, &request(assistant_message.clone()))
            .unwrap();
        assert_eq!(
            dropped.body["messages"][1]["content"],
            json!("answer"),
            "{other} read Sonnet 5.5 thinking"
        );
    }

    // Sonnet 5.5 reads earlier Sonnet/Opus/Haiku thinking, not Opus 5/5.5 or Fable.
    for source in ["claude-sonnet-5", "claude-opus-4-8", "claude-haiku-4-5"] {
        let upstream = provider
            .transform_response(source, anthropic_native())
            .unwrap();
        let replayed = provider
            .transform_request(
                "claude-sonnet-5-5",
                &request(upstream["choices"][0]["message"].clone()),
            )
            .unwrap();
        assert_eq!(
            replayed.body["messages"][1]["content"],
            anthropic_native()["content"],
            "Sonnet 5.5 refused thinking from {source}"
        );
    }
    for source in ["claude-opus-5", "claude-opus-5-5", "claude-fable-5-1"] {
        let upstream = provider
            .transform_response(source, anthropic_native())
            .unwrap();
        let dropped = provider
            .transform_request(
                "claude-sonnet-5-5",
                &request(upstream["choices"][0]["message"].clone()),
            )
            .unwrap();
        assert_eq!(
            dropped.body["messages"][1]["content"],
            json!("answer"),
            "Sonnet 5.5 read thinking from {source}"
        );
    }
}

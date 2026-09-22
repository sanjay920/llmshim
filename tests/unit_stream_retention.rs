use llmshim::{
    reasoning::{ReasoningAccumulator, ReplayTarget, WireFormat},
    streaming::{StreamNormalizer, StreamRetentionLimits},
};
use serde_json::json;

fn limits(normalizer_bytes: usize, normalizer_entries: usize) -> StreamRetentionLimits {
    StreamRetentionLimits::new(normalizer_bytes, normalizer_entries, 4 * 1024 * 1024, 4_096)
        .unwrap()
}

fn responses_stream(normalizer_bytes: usize, normalizer_entries: usize) -> StreamNormalizer {
    StreamNormalizer::with_retention_limits(
        ReplayTarget::new("openai", "gpt-5.4", WireFormat::OpenAiResponses),
        limits(normalizer_bytes, normalizer_entries),
    )
    .unwrap()
}

#[test]
fn tool_argument_fragments_fail_at_the_retained_byte_budget() {
    let mut stream = responses_stream(768, 64);
    stream
        .push(&json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"synthetic","name":"read","arguments":""}}).to_string())
        .unwrap();

    let mut failure = None;
    for _ in 0..16 {
        match stream.push(
            &json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}).to_string(),
        ) {
            Ok(_) => {}
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    let failure = failure.expect("assembled arguments must reach the tiny retained-byte limit");
    assert_eq!(
        failure.to_string(),
        "stream error: upstream stream retained state exceeds limit"
    );
    assert!(stream.push("[DONE]").is_err());
}

#[test]
fn every_tool_string_fragment_uses_the_fixed_retention_failure() {
    for field in ["name", "wire_id", "arguments"] {
        let mut stream = StreamNormalizer::with_retention_limits(
            ReplayTarget::new("custom", "model", WireFormat::OpenAiChat),
            limits(768, 128),
        )
        .unwrap();
        let mut failure = None;
        for _ in 0..32 {
            let call = match field {
                "name" => json!({"index":0,"function":{"name":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}),
                "wire_id" => {
                    json!({"index":0,"id":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx","function":{}})
                }
                "arguments" => {
                    json!({"index":0,"function":{"arguments":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}})
                }
                _ => unreachable!(),
            };
            let event =
                json!({"choices":[{"index":0,"delta":{"tool_calls":[call]},"finish_reason":null}]});
            match stream.push(&event.to_string()) {
                Ok(_) => {}
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        assert_eq!(
            failure.unwrap().to_string(),
            "stream error: upstream stream retained state exceeds limit",
            "{field} fragments must fail through the retained-state boundary"
        );
        assert!(stream.push("[DONE]").is_err());
    }
}

#[test]
fn many_empty_choice_identities_hit_the_entry_budget() {
    let mut stream = StreamNormalizer::with_retention_limits(
        ReplayTarget::new(
            "gemini",
            "gemini-2.5-pro",
            WireFormat::GoogleGenerateContent,
        ),
        limits(16 * 1024, 6),
    )
    .unwrap();

    for index in 0..3 {
        stream
            .push(
                &json!({"candidates":[{"index":index,"content":{"parts":[]},"finishReason":"STOP"}]}).to_string(),
            )
            .unwrap();
    }
    let error = stream
        .push(
            &json!({"candidates":[{"index":3,"content":{"parts":[]},"finishReason":"STOP"}]})
                .to_string(),
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "stream error: upstream stream retained state exceeds limit"
    );
}

#[test]
fn completed_tool_parts_release_their_retained_entries() {
    let mut stream = StreamNormalizer::with_retention_limits(
        ReplayTarget::new(
            "gemini",
            "gemini-2.5-pro",
            WireFormat::GoogleGenerateContent,
        ),
        limits(32 * 1024, 7),
    )
    .unwrap();

    for index in 0..2 {
        let output = stream
            .push(
                &json!({"candidates":[{"index":index,"content":{"parts":[{"functionCall":{"name":"read","args":{}}}]},"finishReason":"STOP"}]}).to_string(),
            )
            .unwrap()
            .unwrap();
        assert!(output.contains("tool_calls"));
    }
}

#[test]
fn forwarded_text_is_not_charged_as_retained_state() {
    let mut stream = responses_stream(512, 8);
    for _ in 0..128 {
        let output = stream
            .push(
                &json!({"type":"response.output_text.delta","output_index":0,"delta":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}).to_string(),
            )
            .unwrap()
            .unwrap();
        assert!(output.contains("xxxxxxxx"));
    }
}

#[test]
fn reasoning_replacement_releases_the_previous_snapshot() {
    let limits = limits(2_200, 64);
    let mut accumulator = ReasoningAccumulator::with_retention_limits(limits).unwrap();
    accumulator
        .push(&json!({"reasoning":[{"index":0,"text":"x".repeat(400)}]}))
        .unwrap();
    accumulator
        .push(&json!({"reasoning":[{"index":0,"replace":true,"text":"short"}]}))
        .unwrap();
    accumulator
        .push(&json!({"reasoning":[{"index":1,"text":"y".repeat(400)}]}))
        .unwrap();
    let blocks = accumulator.blocks();
    assert_eq!(blocks[0]["text"], "short");
    assert_eq!(blocks[1]["text"], "y".repeat(400));
}

#[test]
fn duplicate_reasoning_snapshot_does_not_consume_another_entry() {
    let limits = limits(8 * 1024, 8);
    let mut accumulator = ReasoningAccumulator::with_retention_limits(limits).unwrap();
    for _ in 0..32 {
        accumulator
            .push(&json!({"reasoning":[{"index":0,"replace":true,"text":"same"}]}))
            .unwrap();
    }
    assert_eq!(accumulator.blocks().len(), 1);
}

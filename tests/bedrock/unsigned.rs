use super::*;
use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
use serde_json::json;

#[test]
fn unsigned_unary_reasoning_preserves_text_without_a_signature() {
    let response = json!({"stopReason":"end_turn","output":{"message":{
        "role":"assistant","content":[
            {"reasoningContent":{"reasoningText":{"text":"Consider the question."}}},
            {"text":"Answer."}]}},"usage":{"inputTokens":1,"outputTokens":2}});
    let normalized = provider().transform_response("model", response).unwrap();
    let message = &normalized["choices"][0]["message"];
    assert_eq!(message["content"], "Answer.");
    let blocks = message["reasoning"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["text"], "Consider the question.");
    assert!(blocks[0].get("signature").is_none());
    assert!(blocks[0]["payload"].get("signature").is_none());
}

#[test]
fn unsigned_stream_reasoning_preserves_text_without_a_signature() {
    let mut decoder = llmshim::providers::bedrock::EventStreamDecoder::new("model");
    let mut normalizer = provider().stream_normalizer("model");
    let mut reasoning = llmshim::reasoning::ReasoningAccumulator::default();
    for (kind, payload) in [
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,
            "delta":{"reasoningContent":{"text":"Consider "}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,
            "delta":{"reasoningContent":{"text":"the question."}}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
        ("messageStop", json!({"stopReason":"end_turn"})),
        (
            "metadata",
            json!({"usage":{"inputTokens":1,"outputTokens":2}}),
        ),
    ] {
        let frame = Message::new(bytes::Bytes::from(payload.to_string()))
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String("event".into()),
            ))
            .add_header(Header::new(":event-type", HeaderValue::String(kind.into())));
        let mut wire = Vec::new();
        aws_smithy_eventstream::frame::write_message_to(&frame, &mut wire).unwrap();
        for event in decoder.push(&wire).unwrap() {
            if let Some(chunk) = normalizer.push(&event).unwrap() {
                let chunk: Value = serde_json::from_str(&chunk).unwrap();
                reasoning.push(&chunk["choices"][0]["delta"]).unwrap();
            }
        }
    }
    decoder.finish().unwrap();
    normalizer.finish().unwrap();
    let blocks = reasoning.blocks();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["text"], "Consider the question.");
    assert!(blocks[0].get("signature").is_none());
}

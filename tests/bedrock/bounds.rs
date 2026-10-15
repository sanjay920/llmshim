use super::*;
use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
use llmshim::providers::bedrock::{BedrockErrorKind, EventStreamDecoder};
use serde_json::json;

fn frame(kind: &'static str, payload: Value) -> Vec<u8> {
    let message = Message::new(bytes::Bytes::from(payload.to_string()))
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("event".into()),
        ))
        .add_header(Header::new(":event-type", HeaderValue::String(kind.into())));
    let mut bytes = Vec::new();
    aws_smithy_eventstream::frame::write_message_to(&message, &mut bytes).unwrap();
    bytes
}
fn start() -> EventStreamDecoder {
    let mut decoder = EventStreamDecoder::new("model");
    decoder
        .push(&frame("messageStart", json!({"role":"assistant"})))
        .unwrap();
    decoder
}
#[test]
fn declared_lengths_are_checked_with_valid_prelude_crc_before_payload() {
    for length in [15u32, 32 * 1024 * 1024 + 1] {
        let mut bytes = Vec::from(length.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_be_bytes());
        bytes.push(0);
        assert_eq!(
            EventStreamDecoder::new("model")
                .push(&bytes)
                .unwrap_err()
                .kind,
            BedrockErrorKind::Protocol
        );
    }
    for length in [16u32, 32 * 1024 * 1024] {
        let mut bytes = Vec::from(length.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_be_bytes());
        bytes.push(0);
        let mut decoder = EventStreamDecoder::new("model");
        decoder.push(&bytes).unwrap();
        assert!(
            decoder.finish().is_err(),
            "an incomplete in-bounds frame is truncated"
        );
    }
}
#[test]
fn block_count_accepts_limit_then_refuses_one_more() {
    let mut decoder = start();
    for index in 0..4096 {
        decoder
            .push(&frame(
                "contentBlockDelta",
                json!({"contentBlockIndex":index,
            "delta":{"text":"x"}}),
            ))
            .unwrap();
    }
    assert_eq!(
        decoder
            .push(&frame(
                "contentBlockDelta",
                json!({"contentBlockIndex":4096,
        "delta":{"text":"x"}})
            ))
            .unwrap_err()
            .kind,
        BedrockErrorKind::Protocol
    );
}
#[test]
fn redacted_bytes_accept_limit_then_refuse_one_more_byte() {
    use base64::Engine;
    let mut decoder = start();
    let data = base64::engine::general_purpose::STANDARD.encode(vec![0; 512 * 1024]);
    let chunk = frame(
        "contentBlockDelta",
        json!({"contentBlockIndex":0,
        "delta":{"reasoningContent":{"redactedContent":data}}}),
    );
    for _ in 0..8 {
        decoder.push(&chunk).unwrap();
    }
    let extra = frame(
        "contentBlockDelta",
        json!({"contentBlockIndex":0,
        "delta":{"reasoningContent":{"redactedContent":"AA=="}}}),
    );
    assert_eq!(
        decoder.push(&extra).unwrap_err().kind,
        BedrockErrorKind::Protocol
    );
}
#[test]
fn empty_error_frame_preserves_its_code() {
    let message = Message::new(bytes::Bytes::new())
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("error".into()),
        ))
        .add_header(Header::new(
            ":error-code",
            HeaderValue::String("SignatureDoesNotMatch".into()),
        ));
    let mut bytes = Vec::new();
    aws_smithy_eventstream::frame::write_message_to(&message, &mut bytes).unwrap();
    let error = EventStreamDecoder::new("model").push(&bytes).unwrap_err();
    assert_eq!(error.code, "SignatureDoesNotMatch");
    assert_eq!(error.status, 403);
    assert_eq!(error.kind, BedrockErrorKind::Credentials);
}

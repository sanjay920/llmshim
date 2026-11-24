use super::*;
use llmshim::providers::bedrock::{BedrockErrorKind, EventStreamDecoder};

fn frames() -> Vec<u8> {
    include_str!("../fixtures/bedrock/stream.hex")
        .split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect()
}

#[test]
fn recorded_stream_text_tool_fragments_and_metadata() {
    let wire = frames();
    for width in [1, 3, 64, wire.len()] {
        let mut decoder = EventStreamDecoder::new("model:0");
        let mut normalizer = provider().stream_normalizer("model:0");
        let mut chunks = Vec::new();
        for bytes in wire.chunks(width) {
            for data in decoder.push(bytes).unwrap() {
                if let Some(chunk) = normalizer.push(&data).unwrap() {
                    chunks.push(serde_json::from_str::<Value>(&chunk).unwrap());
                }
            }
        }
        decoder.finish().unwrap();
        if let Some(chunk) = normalizer.finish().unwrap() {
            chunks.push(serde_json::from_str(&chunk).unwrap());
        }
        assert!(chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["content"] == "Hello."));
        let call = chunks
            .iter()
            .find_map(|c| {
                c["choices"][0]["delta"]["tool_calls"]
                    .as_array()
                    .and_then(|calls| calls.first())
            })
            .unwrap();
        assert_eq!(call["function"]["name"], "weather");
        assert_eq!(call["function"]["arguments"], r#"{"city":"Paris"}"#);
        let usage = &chunks.last().unwrap()["usage"];
        assert_eq!(usage["cache_read_tokens"], 3);
        assert_eq!(usage["cache_write_tokens"], 2);
        assert_eq!(usage["uncached_input_tokens"], 10);
        assert_eq!(usage["completion_tokens"], 4);
    }
}

#[test]
fn crc_corruption_and_truncated_frames_fail_closed() {
    let wire = frames();
    for index in [8, 12, wire.len() - 1] {
        let mut bad = wire.clone();
        bad[index] ^= 1;
        let error = EventStreamDecoder::new("model").push(&bad).unwrap_err();
        assert_eq!(error.kind, BedrockErrorKind::Protocol);
    }
    let mut decoder = EventStreamDecoder::new("model");
    decoder.push(&wire[..wire.len() - 1]).unwrap();
    assert_eq!(
        decoder.finish().unwrap_err().kind,
        BedrockErrorKind::Protocol
    );
}

#[test]
fn exception_after_successful_stream_start_is_typed_failure() {
    let wire: Vec<u8> = include_str!("../fixtures/bedrock/exception.hex")
        .split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect();
    let mut decoder = EventStreamDecoder::new("model");
    let mut failed = false;
    for byte in wire {
        if let Err(error) = decoder.push(&[byte]) {
            assert_eq!(error.kind, BedrockErrorKind::Throttled);
            assert_eq!(error.code, "throttlingException");
            failed = true;
            break;
        }
    }
    assert!(failed, "HTTP 200 must not hide a stream exception");
}

#[test]
fn redacted_reasoning_fragments_preserve_bytes_and_context_limit_is_length() {
    let wire: Vec<u8> = include_str!("../fixtures/bedrock/redacted.hex")
        .split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect();
    let mut decoder = EventStreamDecoder::new("model");
    let mut normalizer = provider().stream_normalizer("model");
    let mut reasoning = llmshim::reasoning::ReasoningAccumulator::default();
    let mut terminal = Value::Null;
    for event in decoder.push(&wire).unwrap() {
        if let Some(chunk) = normalizer.push(&event).unwrap() {
            let chunk: Value = serde_json::from_str(&chunk).unwrap();
            reasoning.push(&chunk["choices"][0]["delta"]).unwrap();
            if chunk["choices"][0]["finish_reason"].is_string() {
                terminal = chunk;
            }
        }
    }
    decoder.finish().unwrap();
    assert_eq!(reasoning.blocks()[0]["data"], "YWI=");
    assert_eq!(terminal["choices"][0]["finish_reason"], "length");
}

#[test]
fn every_split_offset_preserves_all_recorded_events() {
    let wire = frames();
    let expected = EventStreamDecoder::new("model")
        .with_request_id("fixture")
        .push(&wire)
        .unwrap();
    for offset in 0..=wire.len() {
        let mut decoder = EventStreamDecoder::new("model").with_request_id("fixture");
        let mut actual = decoder.push(&wire[..offset]).unwrap();
        actual.extend(decoder.push(&wire[offset..]).unwrap());
        decoder.finish().unwrap();
        assert_eq!(actual, expected, "split at {offset}");
    }
}

#[test]
fn stream_exception_keeps_payload_and_timeout_status() {
    use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
    for (code, status) in [
        ("modelTimeoutException", 408),
        ("internalServerException", 500),
    ] {
        let message = Message::new(bytes::Bytes::from_static(
            b"{\"message\":\"fixture explanation\"}",
        ))
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String("exception".into()),
        ))
        .add_header(Header::new(
            ":exception-type",
            HeaderValue::String(code.into()),
        ));
        let mut wire = Vec::new();
        aws_smithy_eventstream::frame::write_message_to(&message, &mut wire).unwrap();
        let error = EventStreamDecoder::new("model").push(&wire).unwrap_err();
        assert_eq!(error.status, status);
        assert_eq!(error.native.unwrap()["message"], "fixture explanation");
    }
}

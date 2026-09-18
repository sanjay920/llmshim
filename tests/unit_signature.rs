use base64::Engine;
use llmshim::{
    provider::Provider,
    providers::{
        anthropic::Anthropic,
        anthropic_signature::{inspect, serving_model_from_signature, Integrity},
    },
    reasoning::{ReplayTarget, WireFormat},
};
use serde_json::{json, Value};
fn varint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while n > 127 {
        out.push((n as u8 & 127) | 128);
        n >>= 7;
    }
    out.push(n as u8);
    out
}
fn field(n: u64, data: &[u8]) -> Vec<u8> {
    [varint(n << 3 | 2), varint(data.len() as u64), data.to_vec()].concat()
}
fn signature(model: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(field(2, &field(1, &field(6, model))))
}
fn response(sig: &str) -> Value {
    Anthropic::new("test".into()).transform_response("claude-sonnet-4-6",json!({"stop_reason":"end_turn","content":[{"type":"thinking","thinking":"brief","signature":sig},{"type":"text","text":"answer"}]})).unwrap()
}

#[test]
fn synthetic_signature_formats_and_malformed_inputs_are_option_only() {
    let good = signature(b"claude-haiku-4-5-20251001");
    assert_eq!(
        serving_model_from_signature(&good),
        if cfg!(feature = "signature-introspection") {
            Some("claude-haiku-4-5-20251001".into())
        } else {
            None
        }
    );
    let mut invalid = vec![
        "not-base64".into(),
        signature(b"bad model"),
        signature(b""),
        signature(b"-bad"),
        signature(&[255]),
        signature(&[b'a'; 129]),
    ];
    for bytes in [
        vec![0],
        vec![0x12, 127],
        vec![255; 11],
        field(2, &field(1, &field(5, b"hash"))),
        [
            field(2, &field(1, &field(6, b"one"))),
            field(2, b"duplicate"),
        ]
        .concat(),
    ] {
        invalid.push(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    for sig in invalid {
        assert_eq!(serving_model_from_signature(&sig), None);
    }
    let bytes = [
        vec![8, 4],
        field(
            2,
            &field(
                1,
                &[
                    vec![8, 1],
                    field(6, b"model:version/path"),
                    vec![0x2d, 0, 0, 0, 0],
                ]
                .concat(),
            ),
        ),
    ]
    .concat();
    let got =
        serving_model_from_signature(&base64::engine::general_purpose::STANDARD.encode(bytes));
    assert_eq!(got.is_some(), cfg!(feature = "signature-introspection"));
}

#[test]
fn observations_do_not_change_stored_provenance_or_authorize_replay() {
    let r = response(&signature(b"claude-opus-4-6-20260101"));
    assert_eq!(
        r["choices"][0]["message"]["reasoning"][0]["origin"]["model"],
        "claude-sonnet-4-6"
    );
    assert_eq!(
        r["choices"][0]["message"]["reasoning"][0]["signature"],
        signature(b"claude-opus-4-6-20260101")
    );
    assert_eq!(
        r.get("x-llmshim-served-model").is_some(),
        cfg!(feature = "signature-introspection")
    );
    let before = r.clone();
    let integrity = inspect(&r, "claude-sonnet-4-6");
    assert_eq!(r, before);
    assert_eq!(
        matches!(integrity, Integrity::Mismatch { .. }),
        cfg!(feature = "signature-introspection")
    );
    let log = llmshim::log::LogEntry::from_response(
        "anthropic",
        "claude-sonnet-4-6",
        &r,
        std::time::Duration::ZERO,
    );
    assert_eq!(log.gateway_integrity, integrity);
    let matching = response(&signature(b"claude-sonnet-4.6-20260101"));
    assert_eq!(
        inspect(&matching, "anthropic/claude-sonnet-4-6"),
        if cfg!(feature = "signature-introspection") {
            Integrity::Ok
        } else {
            Integrity::Unknown
        }
    );
    assert!(matching.get("x-llmshim-served-model").is_none());
    let unknown = response("bad");
    assert_eq!(inspect(&unknown, "claude-sonnet-4-6"), Integrity::Unknown);
}

#[test]
fn streamed_signature_is_inspected_once_after_all_fragments() {
    let signature = signature(b"claude-opus-4-6");
    let cut = signature.len() / 2;
    let mut stream = llmshim::streaming::StreamNormalizer::new(ReplayTarget::new(
        "anthropic",
        "claude-sonnet-4-6",
        WireFormat::AnthropicMessages,
    ));
    for event in [
        json!({"type":"message_start","message":{"id":"a","usage":{}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"brief"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":&signature[..cut]}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":&signature[cut..]}}),
    ] {
        let chunk = stream.push(&event.to_string()).unwrap();
        assert!(!chunk.unwrap_or_default().contains("x-llmshim-served-model"));
    }
    let chunk=stream.push(&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}).to_string()).unwrap().unwrap();
    let parsed: Value = serde_json::from_str(&chunk).unwrap();
    assert_eq!(
        parsed.get("x-llmshim-served-model").is_some(),
        cfg!(feature = "signature-introspection")
    );
    stream.finish().unwrap();
}

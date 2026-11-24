//! Production stream producer, driven by an in-memory HTTP 200 response.
use super::*;
use crate::{
    provider::Provider,
    providers::bedrock::{AwsProfile, Bedrock, BedrockError, BedrockErrorKind},
};

fn recorded_response(fixture: &str) -> reqwest::Response {
    let bytes: Vec<u8> = fixture
        .split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect();
    http::Response::builder()
        .status(200)
        .header("x-amzn-requestid", "fixture-stream-request")
        .header("content-type", "application/vnd.amazon.eventstream")
        .body(bytes)
        .unwrap()
        .into()
}

#[tokio::test]
async fn binary_http_200_uses_decoder_and_shared_normalizer() {
    let provider = Bedrock::new(AwsProfile::new("fixture", "us-east-1"));
    for (fixture, fails) in [
        (
            include_str!("../../tests/fixtures/bedrock/stream.hex"),
            false,
        ),
        (
            include_str!("../../tests/fixtures/bedrock/exception.hex"),
            true,
        ),
    ] {
        let stream = eager_stream(
            recorded_response(fixture),
            None,
            None,
            provider.replay_target("model"),
            Arc::new(std::sync::Mutex::new(None)),
            AttemptDeadlines::default(),
            tokio::time::Instant::now() + Duration::from_secs(60),
            crate::streaming::StreamRetentionLimits::default(),
            provider.transport(),
        );
        let chunks: Vec<_> = tokio::time::timeout(Duration::from_secs(60), stream.collect())
            .await
            .unwrap();
        if fails {
            let error = chunks.last().unwrap().as_ref().unwrap_err();
            assert_eq!(
                BedrockError::from_shim(error).unwrap().kind,
                BedrockErrorKind::Throttled
            );
        } else {
            assert!(chunks.iter().all(Result::is_ok));
            let chunks = chunks
                .iter()
                .map(|chunk| {
                    serde_json::from_str::<serde_json::Value>(chunk.as_ref().unwrap()).unwrap()
                })
                .collect::<Vec<_>>();
            assert!(chunks
                .iter()
                .all(|chunk| chunk["id"] == "fixture-stream-request"));
            let last = chunks.last().unwrap();
            assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
            assert_eq!(last["usage"]["prompt_tokens"], 15);
            assert_eq!(last["usage"]["uncached_input_tokens"], 10);
            assert_eq!(last["usage"]["cache_read_tokens"], 3);
            assert_eq!(last["usage"]["cache_write_tokens"], 2);
            assert_eq!(last["usage"]["bedrock"]["inputTokens"], 10);
        }
    }
}

#[test]
fn converse_native_usage_reaches_policy_accounting_with_cache_buckets() {
    let provider = Bedrock::new(AwsProfile::new("fixture", "us-east-1"));
    let native: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/bedrock/converse-response.json"
    ))
    .unwrap();
    let observation = crate::usage::normalize_native_response_usage_observation(
        &provider.replay_target("model"),
        &provider.response_for_usage(&native),
    )
    .unwrap();
    assert_eq!(observation.usage["uncached_input_tokens"], 10);
    assert_eq!(observation.usage["cache_read_tokens"], 3);
    assert_eq!(observation.usage["cache_write_tokens"], 2);
    assert_eq!(observation.usage["completion_tokens"], 4);
}

#[test]
fn absent_or_invalid_usage_never_claims_complete_zero_accounting() {
    let provider = Bedrock::new(AwsProfile::new("fixture", "us-east-1"));
    for usage in [
        serde_json::json!({}),
        serde_json::json!({"inputTokens":"bad","outputTokens":null}),
        serde_json::json!({"inputTokens":7,"cacheReadInputTokens":2}),
    ] {
        let native = serde_json::json!({"usage":usage});
        let observation = crate::usage::normalize_native_response_usage_observation(
            &provider.replay_target("model"),
            &provider.response_for_usage(&native),
        );
        assert!(observation
            .as_ref()
            .is_none_or(|o| !o.counters_complete && !o.explicit_zero));
        if usage["inputTokens"] == 7 {
            assert_eq!(observation.unwrap().usage["uncached_input_tokens"], 7);
        }
    }
}

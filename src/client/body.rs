use super::DispatchFailure;
use crate::error::ShimError;
use bytes::Bytes;
use futures::{Stream, StreamExt};

#[derive(Clone, Copy)]
pub(super) struct ResponseBodyLimits {
    pub success_bytes: usize,
    pub error_bytes: usize,
}

impl Default for ResponseBodyLimits {
    fn default() -> Self {
        Self {
            success_bytes: 32 * 1024 * 1024,
            error_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug)]
pub(super) enum BodyReadError {
    Http(reqwest::Error),
    TooLarge,
}

impl BodyReadError {
    pub(super) fn into_dispatch_failure(self) -> DispatchFailure {
        match self {
            Self::Http(error) => DispatchFailure::Upstream(error.into()),
            Self::TooLarge => DispatchFailure::Local(ShimError::ProviderError {
                status: 502,
                body: "upstream response body exceeds size limit".into(),
                retry_after: None,
            }),
        }
    }
}

pub(super) async fn read(
    response: reqwest::Response,
    maximum_bytes: usize,
) -> Result<Vec<u8>, BodyReadError> {
    if response
        .content_length()
        .is_some_and(|declared_bytes| declared_bytes > maximum_bytes as u64)
    {
        return Err(BodyReadError::TooLarge);
    }
    collect_decoded_chunks(response.bytes_stream(), maximum_bytes).await
}

async fn collect_decoded_chunks(
    response_chunks: impl Stream<Item = Result<Bytes, reqwest::Error>>,
    maximum_bytes: usize,
) -> Result<Vec<u8>, BodyReadError> {
    futures::pin_mut!(response_chunks);
    let mut decoded_body = Vec::new();
    while let Some(response_chunk) = response_chunks.next().await {
        let response_chunk = response_chunk.map_err(BodyReadError::Http)?;
        if response_chunk.len() > maximum_bytes.saturating_sub(decoded_body.len()) {
            return Err(BodyReadError::TooLarge);
        }
        decoded_body.extend_from_slice(&response_chunk);
    }
    Ok(decoded_body)
}

pub(super) async fn read_json(
    response: reqwest::Response,
    maximum_bytes: usize,
) -> Result<serde_json::Value, BodyReadError> {
    let decoded_body = read(response, maximum_bytes).await?;
    // Retain reqwest's public decode-error type after the body is bounded.
    reqwest::Response::from(http::Response::new(decoded_body))
        .json()
        .await
        .map_err(BodyReadError::Http)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{RetryConfig, ShimClient};
    use crate::policy::{
        AttemptAccounting, AttemptEvent, AttemptIdentity, AttemptOutcome, AttemptPolicy,
        AttemptPolicyError, AttemptPolicyFuture, AttemptPolicyRefusal, DispatchPolicyContext,
        PreparedAttempt,
    };
    use crate::provider::ProviderRequest;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn bounded_client() -> ShimClient {
        ShimClient {
            response_body_limits: ResponseBodyLimits {
                success_bytes: 64,
                error_bytes: 16,
            },
            retry: RetryConfig {
                max_retries: 1,
                base: Duration::ZERO,
                cap: Duration::ZERO,
            },
            ..ShimClient::new()
        }
    }

    fn request(server_url: &str) -> ProviderRequest {
        ProviderRequest {
            url: format!("{server_url}/completion"),
            headers: vec![],
            body: json!({"messages": [{"role": "user", "content": "test"}]}),
        }
    }

    #[tokio::test]
    async fn decoded_chunks_accept_the_exact_limit_and_stop_before_eof() {
        let exact_chunks = futures::stream::iter([
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"def")),
        ]);
        assert_eq!(
            collect_decoded_chunks(exact_chunks, 6).await.unwrap(),
            b"abcdef"
        );

        let overflowing_chunks = futures::stream::iter([
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"def")),
        ])
        .chain(futures::stream::pending());
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            collect_decoded_chunks(overflowing_chunks, 5),
        )
        .await
        .expect("the reader must not wait for EOF after crossing the limit");
        assert!(matches!(result, Err(BodyReadError::TooLarge)));
    }

    #[tokio::test]
    async fn gzip_is_limited_after_decoding() {
        let compressed_body = vec![
            31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 75, 76, 28, 88, 0, 0, 140, 54, 43, 241, 128, 0, 0, 0,
        ];
        assert!(compressed_body.len() < 64);
        let mut upstream_server = mockito::Server::new_async().await;
        let response_mock = upstream_server
            .mock("GET", "/compressed")
            .with_header("content-encoding", "gzip")
            .with_body(compressed_body)
            .expect(2)
            .create_async()
            .await;
        for maximum_bytes in [64, 128] {
            let response = reqwest::Client::new()
                .get(format!("{}/compressed", upstream_server.url()))
                .send()
                .await
                .unwrap();
            let result = read(response, maximum_bytes).await;
            if maximum_bytes == 64 {
                assert!(matches!(result, Err(BodyReadError::TooLarge)));
            } else {
                assert_eq!(result.unwrap(), vec![b'a'; 128]);
            }
        }
        response_mock.assert_async().await;
    }

    #[tokio::test]
    async fn oversized_final_error_uses_a_fixed_diagnostic() {
        let mut upstream_server = mockito::Server::new_async().await;
        let response_mock = upstream_server
            .mock("POST", "/completion")
            .with_status(401)
            .with_chunked_body(|writer| writer.write_all(b"provider-specific diagnostic text"))
            .expect(1)
            .create_async()
            .await;
        let error = bounded_client()
            .send(&request(&upstream_server.url()))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ShimError::ProviderError { status: 502, ref body, .. }
            if body == "upstream response body exceeds size limit")
        );
        response_mock.assert_async().await;
    }

    #[tokio::test]
    async fn oversized_retry_body_is_dropped_before_the_next_attempt() {
        let mut upstream_server = mockito::Server::new_async().await;
        let retry_mock = upstream_server
            .mock("POST", "/completion")
            .with_status(500)
            .with_chunked_body(|writer| writer.write_all(b"retry body beyond the small test limit"))
            .expect(1)
            .create_async()
            .await;
        let success_mock = upstream_server
            .mock("POST", "/completion")
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create_async()
            .await;
        let response = bounded_client()
            .send(&request(&upstream_server.url()))
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        retry_mock.assert_async().await;
        success_mock.assert_async().await;
    }

    #[tokio::test]
    async fn bounded_json_preserves_reqwest_decode_errors() {
        let mut upstream_server = mockito::Server::new_async().await;
        let response_mock = upstream_server
            .mock("GET", "/invalid-json")
            .with_body("not json")
            .create_async()
            .await;
        let response = reqwest::Client::new()
            .get(format!("{}/invalid-json", upstream_server.url()))
            .send()
            .await
            .unwrap();
        assert!(matches!(read_json(response, 64).await,
            Err(BodyReadError::Http(error)) if error.is_decode()));
        response_mock.assert_async().await;
    }

    #[derive(Default)]
    struct RecordingPolicy {
        outcomes: Mutex<Vec<AttemptOutcome>>,
    }

    impl AttemptPolicy for RecordingPolicy {
        fn acquire<'a>(
            &'a self,
            _attempt: &'a PreparedAttempt<'a>,
        ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
            Box::pin(async { Ok(()) })
        }

        fn observe<'a>(
            &'a self,
            _attempt: &'a AttemptIdentity,
            event: AttemptEvent<'a>,
        ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
            Box::pin(async move {
                if let AttemptEvent::Finished(outcome) = event {
                    self.outcomes.lock().unwrap().push(outcome);
                }
                Ok(())
            })
        }

        fn observe_abandoned(
            &self,
            _attempt: &AttemptIdentity,
            outcome: AttemptOutcome,
        ) -> Result<(), AttemptPolicyError> {
            self.outcomes.lock().unwrap().push(outcome);
            Ok(())
        }
    }

    #[tokio::test]
    async fn oversized_success_finishes_the_attempt_with_unknown_accounting() {
        let mut upstream_server = mockito::Server::new_async().await;
        let response_mock = upstream_server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_body("a".repeat(65))
            .expect(1)
            .create_async()
            .await;
        let provider = crate::providers::openai_compat::OpenAiCompatible::new(
            "test-provider",
            upstream_server.url(),
            None,
        );
        let policy = Arc::new(RecordingPolicy::default());
        let context = DispatchPolicyContext::new(policy.clone());
        let error = bounded_client()
            .completion_with_policy(
                &provider,
                "test-model",
                &json!({"messages": [{"role": "user", "content": "test"}]}),
                &context,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ShimError::ProviderError { status: 502, .. }
        ));
        assert_eq!(
            *policy.outcomes.lock().unwrap(),
            vec![AttemptOutcome::InvalidResponse {
                accounting: AttemptAccounting::Unknown
            }]
        );
        response_mock.assert_async().await;
    }
}

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
    Timeout,
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
            Self::Timeout => DispatchFailure::LocalTimeout(ShimError::ProviderError {
                status: 504,
                body: "upstream response body timed out".into(),
                retry_after: None,
            }),
        }
    }
}

pub(super) async fn read(
    response: reqwest::Response,
    maximum_bytes: usize,
    idle_timeout: std::time::Duration,
    total_deadline: tokio::time::Instant,
) -> Result<Vec<u8>, BodyReadError> {
    if response
        .content_length()
        .is_some_and(|declared_bytes| declared_bytes > maximum_bytes as u64)
    {
        return Err(BodyReadError::TooLarge);
    }
    collect_decoded_chunks(
        response.bytes_stream(),
        maximum_bytes,
        idle_timeout,
        total_deadline,
    )
    .await
}

async fn collect_decoded_chunks(
    response_chunks: impl Stream<Item = Result<Bytes, reqwest::Error>>,
    maximum_bytes: usize,
    idle_timeout: std::time::Duration,
    total_deadline: tokio::time::Instant,
) -> Result<Vec<u8>, BodyReadError> {
    futures::pin_mut!(response_chunks);
    let mut decoded_body = Vec::new();
    loop {
        let idle_deadline = tokio::time::Instant::now()
            .checked_add(idle_timeout)
            .ok_or(BodyReadError::Timeout)?;
        let response_chunk = tokio::select! {
            response_chunk = response_chunks.next() => response_chunk,
            _ = tokio::time::sleep_until(idle_deadline) => return Err(BodyReadError::Timeout),
            _ = tokio::time::sleep_until(total_deadline) => return Err(BodyReadError::Timeout),
        };
        let Some(response_chunk) = response_chunk else {
            break;
        };
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
    idle_timeout: std::time::Duration,
    total_deadline: tokio::time::Instant,
) -> Result<serde_json::Value, BodyReadError> {
    let decoded_body = read(response, maximum_bytes, idle_timeout, total_deadline).await?;
    // Retain reqwest's public decode-error type after the body is bounded.
    reqwest::Response::from(http::Response::new(decoded_body))
        .json()
        .await
        .map_err(BodyReadError::Http)
}

pub(super) async fn read_text_and_bytes(
    response: reqwest::Response,
    maximum_bytes: usize,
    idle_timeout: std::time::Duration,
    total_deadline: tokio::time::Instant,
) -> Result<(String, Vec<u8>), BodyReadError> {
    let content_type = response.headers().get(http::header::CONTENT_TYPE).cloned();
    let decoded_body = read(response, maximum_bytes, idle_timeout, total_deadline).await?;
    let mut bounded_response = http::Response::new(decoded_body.clone());
    if let Some(content_type) = content_type {
        bounded_response
            .headers_mut()
            .insert(http::header::CONTENT_TYPE, content_type);
    }
    let text = reqwest::Response::from(bounded_response)
        .text()
        .await
        .map_err(BodyReadError::Http)?;
    Ok((text, decoded_body))
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
            collect_decoded_chunks(
                exact_chunks,
                6,
                Duration::from_secs(1),
                tokio::time::Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap(),
            b"abcdef"
        );

        let overflowing_chunks = futures::stream::iter([
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"def")),
        ])
        .chain(futures::stream::pending());
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            collect_decoded_chunks(
                overflowing_chunks,
                5,
                Duration::from_secs(1),
                tokio::time::Instant::now() + Duration::from_secs(1),
            ),
        )
        .await
        .expect("the reader must not wait for EOF after crossing the limit");
        assert!(matches!(result, Err(BodyReadError::TooLarge)));
    }

    #[tokio::test(start_paused = true)]
    async fn decoded_body_idle_timeout_is_independent_of_size_limit() {
        let result = collect_decoded_chunks(
            futures::stream::pending(),
            64,
            Duration::from_millis(5),
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await;
        assert!(matches!(result, Err(BodyReadError::Timeout)));
    }

    #[tokio::test(start_paused = true)]
    async fn decoded_body_total_timeout_does_not_reset_on_progress() {
        let chunks = futures::stream::unfold((), |_| async {
            tokio::time::sleep(Duration::from_millis(4)).await;
            Some((Ok(Bytes::from_static(b"x")), ()))
        });
        let result = collect_decoded_chunks(
            chunks,
            64,
            Duration::from_millis(5),
            tokio::time::Instant::now() + Duration::from_millis(10),
        )
        .await;
        assert!(matches!(result, Err(BodyReadError::Timeout)));
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
            let result = read(
                response,
                maximum_bytes,
                Duration::from_secs(1),
                tokio::time::Instant::now() + Duration::from_secs(1),
            )
            .await;
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
        assert!(matches!(read_json(
            response,
            64,
            Duration::from_secs(1),
            tokio::time::Instant::now() + Duration::from_secs(1),
        ).await,
            Err(BodyReadError::Http(error)) if error.is_decode()));
        response_mock.assert_async().await;
    }

    #[tokio::test]
    async fn bounded_text_preserves_reqwest_character_decoding() {
        let mut upstream_server = mockito::Server::new_async().await;
        let response_mock = upstream_server
            .mock("GET", "/encoded-text")
            .with_header("content-type", "text/plain; charset=iso-8859-1")
            .with_body(vec![b'c', b'a', b'f', 0xe9])
            .expect(2)
            .create_async()
            .await;
        let response_url = format!("{}/encoded-text", upstream_server.url());
        let http_client = reqwest::Client::new();
        let original_text = http_client
            .get(&response_url)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let bounded_text = read_text_and_bytes(
            http_client.get(response_url).send().await.unwrap(),
            4,
            Duration::from_secs(1),
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(bounded_text, original_text);
        response_mock.assert_async().await;
    }

    #[derive(Default)]
    struct RecordingPolicy {
        outcomes: Mutex<Vec<AttemptOutcome>>,
    }

    struct PendingAcquirePolicy;

    impl AttemptPolicy for PendingAcquirePolicy {
        fn acquire<'a>(
            &'a self,
            _attempt: &'a PreparedAttempt<'a>,
        ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
            Box::pin(futures::future::pending())
        }

        fn observe<'a>(
            &'a self,
            _attempt: &'a AttemptIdentity,
            _event: AttemptEvent<'a>,
        ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
            Box::pin(async { Ok(()) })
        }

        fn observe_abandoned(
            &self,
            _attempt: &AttemptIdentity,
            _outcome: AttemptOutcome,
        ) -> Result<(), AttemptPolicyError> {
            Ok(())
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum PendingObservation {
        Headers,
        Usage,
        Finish,
    }

    struct PendingObservationPolicy {
        pending: PendingObservation,
        abandoned: std::sync::atomic::AtomicBool,
    }

    impl AttemptPolicy for PendingObservationPolicy {
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
            let should_wait = matches!(
                (self.pending, event),
                (
                    PendingObservation::Headers,
                    AttemptEvent::ResponseHeaders { .. }
                ) | (PendingObservation::Usage, AttemptEvent::Usage { .. })
                    | (PendingObservation::Finish, AttemptEvent::Finished(_))
            );
            Box::pin(async move {
                if should_wait {
                    futures::future::pending().await
                } else {
                    Ok(())
                }
            })
        }

        fn observe_abandoned(
            &self,
            _attempt: &AttemptIdentity,
            _outcome: AttemptOutcome,
        ) -> Result<(), AttemptPolicyError> {
            self.abandoned
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pending_policy_acquire_returns_local_503_without_sending() {
        let mut upstream_server = mockito::Server::new_async().await;
        let response_mock = upstream_server
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let provider = crate::providers::openai_compat::OpenAiCompatible::new(
            "test-provider",
            upstream_server.url(),
            None,
        );
        let deadlines = crate::client::AttemptDeadlines::default()
            .with_policy_callback_timeout(Duration::from_millis(5))
            .unwrap();
        let client = ShimClient::new().with_attempt_deadlines(deadlines).unwrap();
        let context = DispatchPolicyContext::new(Arc::new(PendingAcquirePolicy));
        let error = client
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
            ShimError::ProviderError { status: 503, .. }
        ));
        response_mock.assert_async().await;
    }

    #[tokio::test]
    async fn pending_header_usage_and_finish_callbacks_are_bounded_and_abandoned() {
        for pending in [
            PendingObservation::Headers,
            PendingObservation::Usage,
            PendingObservation::Finish,
        ] {
            let mut upstream_server = mockito::Server::new_async().await;
            let response_mock = upstream_server
                .mock("POST", "/chat/completions")
                .with_status(200)
                .with_body(
                    json!({
                        "id": "response",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    })
                    .to_string(),
                )
                .expect(1)
                .create_async()
                .await;
            let provider = crate::providers::openai_compat::OpenAiCompatible::new(
                "test-provider",
                upstream_server.url(),
                None,
            );
            let deadlines = crate::client::AttemptDeadlines::default()
                .with_policy_callback_timeout(Duration::from_millis(20))
                .unwrap();
            let client = ShimClient::new().with_attempt_deadlines(deadlines).unwrap();
            let policy = Arc::new(PendingObservationPolicy {
                pending,
                abandoned: std::sync::atomic::AtomicBool::new(false),
            });
            let context = DispatchPolicyContext::new(policy.clone());
            let error = client
                .completion_with_policy(
                    &provider,
                    "test-model",
                    &json!({"messages": [{"role": "user", "content": "test"}]}),
                    &context,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, ShimError::ProviderError { status: 503, .. }),
                "unexpected {pending:?} error: {error:?}"
            );
            assert!(policy.abandoned.load(std::sync::atomic::Ordering::SeqCst));
            response_mock.assert_async().await;
        }
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

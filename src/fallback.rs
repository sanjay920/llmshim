use crate::error::{Result, ShimError};
use crate::log::{LogEntry, Logger, RequestTimer};
use crate::policy::DispatchPolicyContext;
use crate::router::Router;
use serde_json::Value;
use std::time::Duration;

/// Configuration for retry and fallback behavior.
#[derive(Debug, Clone)]
pub struct FallbackConfig {
    /// Ordered list of model strings to try. The first is the primary.
    pub models: Vec<String>,
    /// Maximum number of retries per model before moving to the next.
    pub max_retries: u32,
    /// Initial backoff duration (doubles on each retry).
    pub initial_backoff: Duration,
    /// HTTP status codes that trigger a retry/fallback (e.g., 429, 500, 502, 503).
    pub retryable_statuses: Vec<u16>,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            models: Vec::new(),
            max_retries: 2,
            initial_backoff: Duration::from_millis(500),
            retryable_statuses: vec![429, 500, 502, 503, 529],
        }
    }
}

impl FallbackConfig {
    pub fn new(models: Vec<String>) -> Self {
        Self {
            models,
            ..Default::default()
        }
    }

    pub fn max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    pub fn initial_backoff(mut self, d: Duration) -> Self {
        self.initial_backoff = d;
        self
    }
}

fn is_retryable(err: &ShimError, retryable_statuses: &[u16]) -> bool {
    match err {
        ShimError::ProviderError { status, .. } => retryable_statuses.contains(status),
        ShimError::Http(_) => true, // network errors are always retryable
        _ => false,
    }
}

/// Run a completion with retry + fallback across multiple models.
pub async fn completion_with_fallback(
    router: &Router,
    request: &Value,
    config: &FallbackConfig,
    logger: Option<&Logger>,
) -> Result<Value> {
    completion_with_fallback_inner(router, request, config, logger, None, None).await
}

pub async fn completion_with_fallback_and_policy(
    router: &Router,
    request: &Value,
    config: &FallbackConfig,
    logger: Option<&Logger>,
    policy_context: &DispatchPolicyContext,
) -> Result<Value> {
    completion_with_fallback_inner(router, request, config, logger, Some(policy_context), None)
        .await
}

async fn completion_with_fallback_inner(
    router: &Router,
    request: &Value,
    config: &FallbackConfig,
    logger: Option<&Logger>,
    policy_context: Option<&DispatchPolicyContext>,
    client_override: Option<crate::client::ShimClient>,
) -> Result<Value> {
    let models = if config.models.is_empty() {
        // No fallback configured — just use the model from the request
        vec![request
            .get("model")
            .and_then(|m| m.as_str())
            .ok_or(ShimError::MissingModel)?
            .to_string()]
    } else {
        config.models.clone()
    };

    let mut errors: Vec<String> = Vec::new();
    let mut policy_limited_providers = std::collections::HashSet::new();
    let mut provider_limit_error: Option<ShimError> = None;
    let mut attempted_distinct_provider_after_limit = false;
    // Every attempt below is counted by the client against this router's
    // breaker; the loop only asks `admit` before dialling.
    let client = client_override.unwrap_or_else(|| crate::bound_client(router));

    for model_str in &models {
        // Build request with this model. A named route expands to its model and
        // settings here, so a chain entry may itself be a `route/<name>`.
        let mut req = request.clone();
        req["model"] = Value::String(model_str.clone());
        let req = match router.expand_route(&req) {
            Ok(expanded) => expanded.into_owned(),
            Err(e) => {
                errors.push(format!("{}: {}", model_str, e));
                continue;
            }
        };

        let (provider, model) = match router.resolve(model_str) {
            Ok(r) => r,
            Err(e) => {
                errors.push(format!("{}: {}", model_str, e));
                continue;
            }
        };
        if policy_limited_providers.contains(provider.name()) {
            errors.push(format!(
                "{}: skipped provider {} after attempt policy refusal",
                model_str,
                provider.name()
            ));
            continue;
        }
        if !policy_limited_providers.is_empty() {
            attempted_distinct_provider_after_limit = true;
        }

        let mut backoff = config.initial_backoff;

        for attempt in 0..=config.max_retries {
            // Provider health, not rate-limit backoff. Checked per attempt, not
            // once per chain entry: the attempt that opens a circuit is usually
            // this loop's own, and continuing to retry past it is exactly the
            // "retrying into a known-dead target" the breaker exists to stop.
            if !router.breaker().admit(provider.name()).await {
                errors.push(format!(
                    "{}: circuit open for provider {}",
                    model_str,
                    provider.name()
                ));
                break; // move to next model
            }

            let timer = RequestTimer::start();
            // Keep OAuth preparation, SSE-only providers, reasoning provenance,
            // and tool normalization identical to an ordinary completion.
            let dispatch_outcome = client
                .completion_dispatch(provider, &model, &req, policy_context)
                .await;
            if let Some(outcome) = crate::client::ShimClient::breaker_outcome(&dispatch_outcome) {
                client.observe(provider, outcome).await;
            }
            match dispatch_outcome {
                Ok(result) => {
                    if let Some(logger) = logger {
                        logger.log(&LogEntry::from_response(
                            provider.name(),
                            model_str,
                            &result,
                            timer.elapsed(),
                        ));
                    }
                    return Ok(result);
                }
                Err(crate::client::DispatchFailure::Upstream(error)) => {
                    if is_retryable(&error, &config.retryable_statuses)
                        && attempt < config.max_retries
                    {
                        errors.push(format!(
                            "{} (attempt {}): {}",
                            model_str,
                            attempt + 1,
                            error
                        ));
                        tokio::time::sleep(backoff).await;
                        backoff *= 2;
                        continue;
                    }
                    if let Some(logger) = logger {
                        logger.log(&LogEntry::from_error(
                            provider.name(),
                            model_str,
                            &error.to_string(),
                            timer.elapsed(),
                        ));
                    }
                    errors.push(format!("{}: {}", model_str, error));
                    break; // move to next model
                }
                Err(crate::client::DispatchFailure::Local(error)) => {
                    if let Some(logger) = logger {
                        logger.log(&LogEntry::from_error(
                            provider.name(),
                            model_str,
                            &error.to_string(),
                            timer.elapsed(),
                        ));
                    }
                    errors.push(format!("{}: {}", model_str, error));
                    break;
                }
                Err(crate::client::DispatchFailure::LocalTimeout(error)) => {
                    if let Some(logger) = logger {
                        logger.log(&LogEntry::from_error(
                            provider.name(),
                            model_str,
                            &error.to_string(),
                            timer.elapsed(),
                        ));
                    }
                    if is_retryable(&error, &config.retryable_statuses) {
                        errors.push(format!("{}: {}", model_str, error));
                        break;
                    }
                    return Err(error);
                }
                Err(crate::client::DispatchFailure::LogicalTimeout(error)) => {
                    if let Some(logger) = logger {
                        logger.log(&LogEntry::from_error(
                            provider.name(),
                            model_str,
                            &error.to_string(),
                            timer.elapsed(),
                        ));
                    }
                    return Err(error);
                }
                Err(crate::client::DispatchFailure::PolicyRefusal(refusal))
                    if refusal.kind() == crate::policy::AttemptPolicyRefusalKind::ProviderLimit =>
                {
                    let error = refusal.into_shim_error();
                    errors.push(format!("{}: {}", model_str, error));
                    policy_limited_providers.insert(provider.name().to_owned());
                    provider_limit_error = Some(error);
                    attempted_distinct_provider_after_limit = false;
                    break;
                }
                Err(crate::client::DispatchFailure::PolicyRefusal(refusal)) => {
                    let error = refusal.into_shim_error();
                    if let Some(logger) = logger {
                        logger.log(&LogEntry::from_error(
                            provider.name(),
                            model_str,
                            &error.to_string(),
                            timer.elapsed(),
                        ));
                    }
                    return Err(error);
                }
                Err(crate::client::DispatchFailure::PolicyObservation(policy_error)) => {
                    let error = policy_error.into_shim_error();
                    if let Some(logger) = logger {
                        logger.log(&LogEntry::from_error(
                            provider.name(),
                            model_str,
                            &error.to_string(),
                            timer.elapsed(),
                        ));
                    }
                    return Err(error);
                }
            }
        }
    }

    if !attempted_distinct_provider_after_limit {
        if let Some(error) = provider_limit_error {
            return Err(error);
        }
    }
    Err(ShimError::AllFailed(errors))
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use crate::policy::{
        AttemptEvent, AttemptIdentity, AttemptOutcome, AttemptPolicy, AttemptPolicyError,
        AttemptPolicyFuture, AttemptPolicyRefusal, PreparedAttempt,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Copy, Debug)]
    enum PendingCallback {
        Acquire,
        Headers,
        Usage,
        Finish,
    }

    struct PendingPolicy(PendingCallback);

    impl AttemptPolicy for PendingPolicy {
        fn acquire<'a>(
            &'a self,
            _attempt: &'a PreparedAttempt<'a>,
        ) -> AttemptPolicyFuture<'a, std::result::Result<(), AttemptPolicyRefusal>> {
            if matches!(self.0, PendingCallback::Acquire) {
                Box::pin(futures::future::pending())
            } else {
                Box::pin(async { Ok(()) })
            }
        }

        fn observe<'a>(
            &'a self,
            _attempt: &'a AttemptIdentity,
            event: AttemptEvent<'a>,
        ) -> AttemptPolicyFuture<'a, std::result::Result<(), AttemptPolicyError>> {
            let pending = matches!(
                (self.0, event),
                (
                    PendingCallback::Headers,
                    AttemptEvent::ResponseHeaders { .. }
                ) | (PendingCallback::Usage, AttemptEvent::Usage { .. })
                    | (PendingCallback::Finish, AttemptEvent::Finished(_))
            );
            Box::pin(async move {
                if pending {
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
        ) -> std::result::Result<(), AttemptPolicyError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn callback_timeouts_stop_fallback_and_do_not_penalize_provider_health() {
        for pending_callback in [
            PendingCallback::Acquire,
            PendingCallback::Headers,
            PendingCallback::Usage,
            PendingCallback::Finish,
        ] {
            let mut first_server = mockito::Server::new_async().await;
            let first_send = first_server
                .mock("POST", "/chat/completions")
                .with_status(200)
                .with_body(
                    serde_json::json!({
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
                .expect(usize::from(!matches!(
                    pending_callback,
                    PendingCallback::Acquire
                )))
                .create_async()
                .await;
            let mut second_server = mockito::Server::new_async().await;
            let second_send = second_server
                .mock("POST", "/chat/completions")
                .expect(0)
                .create_async()
                .await;
            let breaker = Arc::new(crate::breaker::ProviderBreaker::with_config(
                crate::breaker::BreakerConfig {
                    window: Duration::from_secs(60),
                    trip_threshold: 1,
                    cooldown: Duration::from_secs(300),
                },
            ));
            let router = Router::new()
                .with_breaker(breaker.clone())
                .register(
                    "first",
                    Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                        "first",
                        first_server.url(),
                        None,
                    )),
                )
                .register(
                    "second",
                    Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                        "second",
                        second_server.url(),
                        None,
                    )),
                );
            let deadlines = crate::client::AttemptDeadlines::default()
                .with_policy_callback_timeout(Duration::from_millis(20))
                .unwrap();
            let client = crate::client::ShimClient::new()
                .with_attempt_deadlines(deadlines)
                .unwrap()
                .with_breaker(breaker.clone());
            let policy = DispatchPolicyContext::new(Arc::new(PendingPolicy(pending_callback)));
            let config = FallbackConfig::new(vec!["first/test".into(), "second/test".into()])
                .max_retries(2)
                .initial_backoff(Duration::ZERO);
            let error = completion_with_fallback_inner(
                &router,
                &serde_json::json!({"model": "first/test", "messages": []}),
                &config,
                None,
                Some(&policy),
                Some(client),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, ShimError::ProviderError { status: 503, .. }),
                "unexpected {pending_callback:?} error: {error:?}"
            );
            assert!(breaker.admit("first").await);
            first_send.assert_async().await;
            second_send.assert_async().await;
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum LocalTimeoutKind {
        Headers,
        SuccessBody,
    }

    async fn local_timeout_server(
        kind: LocalTimeoutKind,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let sends = Arc::new(AtomicUsize::new(0));
        let server_sends = sends.clone();
        let expected_sends = match kind {
            LocalTimeoutKind::Headers => 4,
            LocalTimeoutKind::SuccessBody => 1,
        };
        let task = tokio::spawn(async move {
            for _ in 0..expected_sends {
                let (mut socket, _) = listener.accept().await.unwrap();
                server_sends.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut request = [0_u8; 2048];
                    let _ = socket.read(&mut request).await;
                    match kind {
                        LocalTimeoutKind::Headers => {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        LocalTimeoutKind::SuccessBody => {
                            socket
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                                .await
                                .unwrap();
                            socket.flush().await.unwrap();
                            let mut byte = [0_u8; 1];
                            let _ = socket.read(&mut byte).await;
                        }
                    }
                });
            }
        });
        (format!("http://{address}"), sends, task)
    }

    #[tokio::test]
    async fn local_504_requires_explicit_fallback_opt_in_without_same_model_replay() {
        for kind in [LocalTimeoutKind::Headers, LocalTimeoutKind::SuccessBody] {
            for opt_in in [false, true] {
                let (first_url, first_sends, first_server) = local_timeout_server(kind).await;
                let mut second_server = mockito::Server::new_async().await;
                let second_send = second_server
                    .mock("POST", "/chat/completions")
                    .with_status(200)
                    .with_body(
                        serde_json::json!({
                            "id": "fallback",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "fallback"},
                                "finish_reason": "stop"
                            }],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                        })
                        .to_string(),
                    )
                    .expect(usize::from(opt_in))
                    .create_async()
                    .await;
                let router = Router::new()
                    .register(
                        "first",
                        Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                            "first", first_url, None,
                        )),
                    )
                    .register(
                        "second",
                        Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                            "second",
                            second_server.url(),
                            None,
                        )),
                    );
                let deadlines = crate::client::AttemptDeadlines::default()
                    .with_response_header_timeout(Duration::from_millis(20))
                    .unwrap()
                    .with_unary_timeouts(Duration::from_millis(20), Duration::from_secs(1))
                    .unwrap();
                let client = crate::client::ShimClient::new()
                    .with_attempt_deadlines(deadlines)
                    .unwrap();
                let mut config =
                    FallbackConfig::new(vec!["first/test".into(), "second/test".into()])
                        .max_retries(2)
                        .initial_backoff(Duration::ZERO);
                if opt_in {
                    config.retryable_statuses.push(504);
                }
                let result = completion_with_fallback_inner(
                    &router,
                    &serde_json::json!({"model": "first/test", "messages": []}),
                    &config,
                    None,
                    None,
                    Some(client),
                )
                .await;
                if opt_in {
                    assert_eq!(
                        result.unwrap()["choices"][0]["message"]["content"],
                        "fallback"
                    );
                } else {
                    assert!(matches!(
                        result.unwrap_err(),
                        ShimError::ProviderError { status: 504, .. }
                    ));
                }
                tokio::time::timeout(Duration::from_secs(1), first_server)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    first_sends.load(Ordering::SeqCst),
                    match kind {
                        LocalTimeoutKind::Headers => 4,
                        LocalTimeoutKind::SuccessBody => 1,
                    },
                    "unexpected same-model replay for {kind:?}, opt_in={opt_in}"
                );
                second_send.assert_async().await;
            }
        }
    }

    #[tokio::test]
    async fn logical_timeout_stops_explicit_504_fallback_before_every_send() {
        let mut first_server = mockito::Server::new_async().await;
        let first_send = first_server
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let mut second_server = mockito::Server::new_async().await;
        let second_send = second_server
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let router = Router::new()
            .register(
                "first",
                Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                    "first",
                    first_server.url(),
                    None,
                )),
            )
            .register(
                "second",
                Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                    "second",
                    second_server.url(),
                    None,
                )),
            );
        let policy = DispatchPolicyContext::new(Arc::new(PendingPolicy(PendingCallback::Headers)))
            .with_logical_deadline(tokio::time::Instant::now());
        let mut config = FallbackConfig::new(vec!["first/test".into(), "second/test".into()])
            .max_retries(2)
            .initial_backoff(Duration::ZERO);
        config.retryable_statuses.push(504);
        let error = completion_with_fallback_inner(
            &router,
            &serde_json::json!({"model": "first/test", "messages": []}),
            &config,
            None,
            Some(&policy),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            ShimError::ProviderError {
                status: 504,
                ref body,
                ..
            } if body == "proxy logical request timed out"
        ));
        first_send.assert_async().await;
        second_send.assert_async().await;
    }
}

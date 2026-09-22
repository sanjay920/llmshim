use futures::StreamExt;
use llmshim::{
    client::ShimClient,
    policy::{
        AttemptAccounting, AttemptEvent, AttemptIdentity, AttemptKind, AttemptOutcome,
        AttemptPolicy, AttemptPolicyError, AttemptPolicyErrorKind, AttemptPolicyFuture,
        AttemptPolicyRefusal, AttemptPolicyRefusalKind, DispatchPolicyContext, PreparedAttempt,
    },
    provider::{Provider, ProviderRequest},
    providers::{
        anthropic::Anthropic,
        chatgpt::{ChatGpt, ChatGptAuth},
        gemini::Gemini,
        openai::OpenAi,
        openai_compat::OpenAiCompatible,
        xai::Xai,
    },
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

struct InvalidHeaderProvider {
    endpoint: String,
}

struct CancellationPolicy {
    acquired: AtomicBool,
    usage_started: tokio::sync::Notify,
    abandoned: AtomicBool,
}

impl CancellationPolicy {
    fn new() -> Self {
        Self {
            acquired: AtomicBool::new(false),
            usage_started: tokio::sync::Notify::new(),
            abandoned: AtomicBool::new(false),
        }
    }
}

impl AttemptPolicy for CancellationPolicy {
    fn acquire<'a>(
        &'a self,
        _attempt: &'a PreparedAttempt<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
        Box::pin(async move {
            self.acquired.store(true, Ordering::SeqCst);
            Ok(())
        })
    }

    fn observe<'a>(
        &'a self,
        _attempt: &'a AttemptIdentity,
        event: AttemptEvent<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
        Box::pin(async move {
            if matches!(event, AttemptEvent::Usage { .. }) {
                self.usage_started.notify_one();
                futures::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn observe_abandoned(
        &self,
        _attempt: &AttemptIdentity,
        _outcome: AttemptOutcome,
    ) -> Result<(), AttemptPolicyError> {
        self.abandoned.store(true, Ordering::SeqCst);
        Ok(())
    }
}

impl Provider for InvalidHeaderProvider {
    fn name(&self) -> &str {
        "invalid-header"
    }

    fn transform_request(
        &self,
        model: &str,
        _request: &Value,
    ) -> llmshim::error::Result<ProviderRequest> {
        Ok(ProviderRequest {
            url: self.endpoint.clone(),
            headers: vec![("x-invalid".into(), "line one\nline two".into())],
            body: json!({"model": model}),
        })
    }

    fn transform_response(&self, _model: &str, response: Value) -> llmshim::error::Result<Value> {
        Ok(response)
    }

    fn transform_stream_chunk(
        &self,
        _model: &str,
        _chunk: &str,
    ) -> llmshim::error::Result<Option<String>> {
        Ok(None)
    }
}

#[derive(Clone, Debug)]
enum RecordedEvent {
    Acquired {
        id: uuid::Uuid,
        kind: AttemptKind,
        provider_name: String,
        resolved_model: String,
        native_model: String,
        wire: llmshim::reasoning::WireFormat,
        account_fingerprint: Option<String>,
        endpoint: String,
        method: String,
        native_body: Value,
    },
    ResponseHeaders {
        id: uuid::Uuid,
        status: u16,
    },
    Usage {
        id: uuid::Uuid,
        usage: Value,
    },
    Finished {
        id: uuid::Uuid,
        outcome: AttemptOutcome,
    },
    Abandoned {
        id: uuid::Uuid,
        outcome: AttemptOutcome,
    },
}

impl RecordedEvent {
    fn id(&self) -> uuid::Uuid {
        match self {
            Self::Acquired { id, .. }
            | Self::ResponseHeaders { id, .. }
            | Self::Usage { id, .. }
            | Self::Finished { id, .. }
            | Self::Abandoned { id, .. } => *id,
        }
    }
}

struct RecordingPolicy {
    events: Mutex<Vec<RecordedEvent>>,
    refuse_acquire_number: Option<usize>,
    refusal_kind: AttemptPolicyRefusalKind,
    failing_event: Option<FailingEvent>,
    fail_abandonment: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FailingEvent {
    Headers,
    Usage,
    Finished,
}

impl Default for RecordingPolicy {
    fn default() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            refuse_acquire_number: None,
            refusal_kind: AttemptPolicyRefusalKind::Budget,
            failing_event: None,
            fail_abandonment: false,
        }
    }
}

impl RecordingPolicy {
    fn refusing(acquire_number: usize) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            refuse_acquire_number: Some(acquire_number),
            refusal_kind: AttemptPolicyRefusalKind::Budget,
            failing_event: None,
            fail_abandonment: false,
        }
    }

    fn refusing_with_kind(acquire_number: usize, refusal_kind: AttemptPolicyRefusalKind) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            refuse_acquire_number: Some(acquire_number),
            refusal_kind,
            failing_event: None,
            fail_abandonment: false,
        }
    }

    fn failing(failing_event: FailingEvent) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            refuse_acquire_number: None,
            refusal_kind: AttemptPolicyRefusalKind::Budget,
            failing_event: Some(failing_event),
            fail_abandonment: false,
        }
    }

    fn failing_abandonment() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            refuse_acquire_number: None,
            refusal_kind: AttemptPolicyRefusalKind::Budget,
            failing_event: None,
            fail_abandonment: true,
        }
    }

    fn events(&self) -> Vec<RecordedEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl AttemptPolicy for RecordingPolicy {
    fn acquire<'a>(
        &'a self,
        attempt: &'a PreparedAttempt<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
        Box::pin(async move {
            let identity = attempt.identity();
            let mut events = self.events.lock().unwrap();
            let acquire_number = events
                .iter()
                .filter(|event| matches!(event, RecordedEvent::Acquired { .. }))
                .count()
                + 1;
            events.push(RecordedEvent::Acquired {
                id: identity.id(),
                kind: identity.kind(),
                provider_name: identity.provider_name().to_owned(),
                resolved_model: identity.resolved_model().to_owned(),
                native_model: identity.native_model().to_owned(),
                wire: identity.wire(),
                account_fingerprint: identity.account_fingerprint().map(str::to_owned),
                endpoint: identity.endpoint().to_owned(),
                method: attempt.method().to_owned(),
                native_body: attempt.native_body().clone(),
            });
            if self.refuse_acquire_number == Some(acquire_number) {
                return Err(AttemptPolicyRefusal::new(self.refusal_kind, None));
            }
            Ok(())
        })
    }

    fn observe<'a>(
        &'a self,
        attempt: &'a AttemptIdentity,
        event: AttemptEvent<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
        Box::pin(async move {
            let (recorded, event_kind) = match event {
                AttemptEvent::ResponseHeaders { status } => (
                    RecordedEvent::ResponseHeaders {
                        id: attempt.id(),
                        status,
                    },
                    FailingEvent::Headers,
                ),
                AttemptEvent::Usage { usage } => (
                    RecordedEvent::Usage {
                        id: attempt.id(),
                        usage: usage.clone(),
                    },
                    FailingEvent::Usage,
                ),
                AttemptEvent::Finished(outcome) => (
                    RecordedEvent::Finished {
                        id: attempt.id(),
                        outcome,
                    },
                    FailingEvent::Finished,
                ),
            };
            self.events.lock().unwrap().push(recorded);
            if self.failing_event == Some(event_kind) {
                return Err(AttemptPolicyError::new(
                    AttemptPolicyErrorKind::CoordinatorUnavailable,
                ));
            }
            Ok(())
        })
    }

    fn observe_abandoned(
        &self,
        attempt: &AttemptIdentity,
        outcome: AttemptOutcome,
    ) -> Result<(), AttemptPolicyError> {
        self.events.lock().unwrap().push(RecordedEvent::Abandoned {
            id: attempt.id(),
            outcome,
        });
        if self.fail_abandonment {
            Err(AttemptPolicyError::new(
                AttemptPolicyErrorKind::CoordinatorUnavailable,
            ))
        } else {
            Ok(())
        }
    }
}

fn context(policy: Arc<RecordingPolicy>) -> DispatchPolicyContext {
    DispatchPolicyContext::new(policy)
}

fn request(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "answer"}],
    })
}

fn structured_request(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "answer"}],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "answer",
                "schema": {"type": "integer", "minimum": 3}
            }
        },
        "x-shim": {"structured_output": "prompt"}
    })
}

fn response(content: Value) -> String {
    json!({
        "id": "response",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
    })
    .to_string()
}

fn successful_stream(content: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"response","choices":[{"index":0,"delta":{"content":content},"finish_reason":null}]}),
        json!({"id":"response","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}),
    )
}

#[tokio::test]
async fn a_refused_retry_never_reaches_the_network() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_status(500)
        .with_body("retryable")
        .expect(1)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("local", server.url(), None);
    let policy = Arc::new(RecordingPolicy::refusing(2));

    let error = ShimClient::new()
        .completion_with_policy(
            &provider,
            "test",
            &request("local/test"),
            &context(policy.clone()),
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("attempt budget exhausted"));
    upstream.assert_async().await;
    let events = policy.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RecordedEvent::Acquired { .. }))
            .count(),
        2
    );
    assert!(events.iter().all(|event| event.id() != uuid::Uuid::nil()));
    assert!(events
        .iter()
        .any(|event| matches!(event, RecordedEvent::ResponseHeaders { status: 500, .. })));
    assert!(events.iter().any(|event| matches!(
        event,
        RecordedEvent::Finished {
            outcome: AttemptOutcome::HttpFailure {
                status: 500,
                accounting: AttemptAccounting::Unknown
            },
            ..
        }
    )));
}

#[tokio::test]
async fn request_build_failure_happens_before_attempt_acquisition() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server.mock("POST", "/never").expect(0).create_async().await;
    let provider = InvalidHeaderProvider {
        endpoint: format!("{}/never", server.url()),
    };
    let policy = Arc::new(RecordingPolicy::default());

    let result = ShimClient::new()
        .completion_with_policy(
            &provider,
            "test",
            &request("invalid-header/test"),
            &context(policy.clone()),
        )
        .await;

    assert!(result.is_err());
    assert!(policy.events().is_empty());
    upstream.assert_async().await;
}

#[tokio::test]
async fn structured_refusals_keep_typed_provenance_on_unary_and_borrowed_streams() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .expect(0)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("local", server.url(), None);

    for stream in [false, true] {
        let policy = Arc::new(RecordingPolicy::refusing(1));
        let client = ShimClient::new();
        let result = if stream {
            client
                .stream_with_policy(
                    &provider,
                    "test",
                    &structured_request("local/test"),
                    &context(policy),
                )
                .await
                .map(|_| Value::Null)
        } else {
            client
                .completion_with_policy(
                    &provider,
                    "test",
                    &structured_request("local/test"),
                    &context(policy),
                )
                .await
        };
        let error = result.unwrap_err().to_string();
        assert!(error.contains("attempt budget exhausted"));
        assert!(!error.contains("output contract"));
    }
    upstream.assert_async().await;
}

#[tokio::test]
async fn owned_stream_repair_refusal_is_not_rewritten_as_provider_failure() {
    let mut server = mockito::Server::new_async().await;
    let first_attempt = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(successful_stream("1"))
        .expect(1)
        .create_async()
        .await;
    let provider: Arc<dyn Provider> = Arc::new(OpenAiCompatible::new("local", server.url(), None));
    let policy = Arc::new(RecordingPolicy::refusing(2));
    let stream = ShimClient::new()
        .stream_owned_with_policy(
            provider,
            "test",
            &structured_request("local/test"),
            &context(policy),
        )
        .await
        .unwrap();

    let chunks: Vec<_> = stream.collect().await;
    assert_eq!(chunks.len(), 1);
    let error = chunks[0].as_ref().unwrap_err().to_string();
    assert!(error.contains("attempt budget exhausted"));
    assert!(!error.contains("output contract"));
    first_attempt.assert_async().await;
}

#[tokio::test]
async fn a_policy_coordinator_refusal_does_not_poison_provider_health() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .expect(0)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("local", server.url(), None);
    let breaker = Arc::new(llmshim::breaker::ProviderBreaker::with_config(
        llmshim::breaker::BreakerConfig {
            window: std::time::Duration::from_secs(60),
            trip_threshold: 1,
            cooldown: std::time::Duration::from_secs(300),
        },
    ));
    let policy = Arc::new(RecordingPolicy::refusing_with_kind(
        1,
        AttemptPolicyRefusalKind::CoordinatorUnavailable,
    ));

    let error = ShimClient::new()
        .with_breaker(breaker.clone())
        .completion_with_policy(&provider, "test", &request("local/test"), &context(policy))
        .await
        .unwrap_err();

    assert!(error
        .to_string()
        .contains("attempt policy coordinator unavailable"));
    assert!(breaker.admit("local").await);
    upstream.assert_async().await;
}

#[tokio::test]
async fn local_repair_exhaustion_does_not_poison_provider_health() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_body(response(json!("1")))
        .expect(2)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("local", server.url(), None);
    let breaker = Arc::new(llmshim::breaker::ProviderBreaker::with_config(
        llmshim::breaker::BreakerConfig {
            window: std::time::Duration::from_secs(60),
            trip_threshold: 1,
            cooldown: std::time::Duration::from_secs(300),
        },
    ));
    let policy = Arc::new(RecordingPolicy::default());

    let result = ShimClient::new()
        .with_breaker(breaker.clone())
        .completion_with_policy(
            &provider,
            "test",
            &structured_request("local/test"),
            &context(policy),
        )
        .await;

    assert!(result.is_err());
    assert!(breaker.admit("local").await);
    upstream.assert_async().await;
}

#[tokio::test]
async fn fallback_acquires_the_actual_provider_for_each_model() {
    let mut primary_server = mockito::Server::new_async().await;
    let mut secondary_server = mockito::Server::new_async().await;
    let primary = primary_server
        .mock("POST", "/chat/completions")
        .with_status(400)
        .with_body("unsupported")
        .expect(1)
        .create_async()
        .await;
    let secondary = secondary_server
        .mock("POST", "/chat/completions")
        .with_body(response(json!("ok")))
        .expect(1)
        .create_async()
        .await;
    let router = llmshim::router::Router::new()
        .register(
            "primary",
            Box::new(OpenAiCompatible::new("primary", primary_server.url(), None)),
        )
        .register(
            "secondary",
            Box::new(OpenAiCompatible::new(
                "secondary",
                secondary_server.url(),
                None,
            )),
        );
    let policy = Arc::new(RecordingPolicy::default());
    let fallback = llmshim::FallbackConfig::new(vec![
        "primary/first".to_owned(),
        "secondary/second".to_owned(),
    ])
    .max_retries(0);

    let result = llmshim::completion_with_fallback_and_policy(
        &router,
        &request("ignored/model"),
        &fallback,
        None,
        &context(policy.clone()),
    )
    .await
    .unwrap();

    assert_eq!(result["choices"][0]["message"]["content"], "ok");
    primary.assert_async().await;
    secondary.assert_async().await;
    let acquired_targets: Vec<_> = policy
        .events()
        .into_iter()
        .filter_map(|event| match event {
            RecordedEvent::Acquired {
                provider_name,
                resolved_model,
                ..
            } => Some((provider_name, resolved_model)),
            _ => None,
        })
        .collect();
    assert_eq!(
        acquired_targets,
        vec![
            ("primary".to_owned(), "first".to_owned()),
            ("secondary".to_owned(), "second".to_owned()),
        ]
    );
}

#[tokio::test]
async fn a_terminal_policy_refusal_stops_the_fallback_chain() {
    let mut primary_server = mockito::Server::new_async().await;
    let mut secondary_server = mockito::Server::new_async().await;
    let primary = primary_server
        .mock("POST", "/chat/completions")
        .expect(0)
        .create_async()
        .await;
    let secondary = secondary_server
        .mock("POST", "/chat/completions")
        .expect(0)
        .create_async()
        .await;
    let router = llmshim::router::Router::new()
        .register(
            "primary",
            Box::new(OpenAiCompatible::new("primary", primary_server.url(), None)),
        )
        .register(
            "secondary",
            Box::new(OpenAiCompatible::new(
                "secondary",
                secondary_server.url(),
                None,
            )),
        );
    let policy = Arc::new(RecordingPolicy::refusing(1));
    let fallback = llmshim::FallbackConfig::new(vec![
        "primary/first".to_owned(),
        "secondary/second".to_owned(),
    ]);

    let error = llmshim::completion_with_fallback_and_policy(
        &router,
        &structured_request("ignored/model"),
        &fallback,
        None,
        &context(policy),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("attempt budget exhausted"));
    primary.assert_async().await;
    secondary.assert_async().await;
}

#[tokio::test]
async fn an_upstream_body_matching_a_policy_message_keeps_upstream_provenance() {
    let mut primary_server = mockito::Server::new_async().await;
    let mut secondary_server = mockito::Server::new_async().await;
    let primary = primary_server
        .mock("POST", "/chat/completions")
        .with_status(400)
        .with_body("attempt budget exhausted")
        .expect(1)
        .create_async()
        .await;
    let secondary = secondary_server
        .mock("POST", "/chat/completions")
        .with_body(response(json!("ok")))
        .expect(1)
        .create_async()
        .await;
    let router = llmshim::router::Router::new()
        .register(
            "primary",
            Box::new(OpenAiCompatible::new("primary", primary_server.url(), None)),
        )
        .register(
            "secondary",
            Box::new(OpenAiCompatible::new(
                "secondary",
                secondary_server.url(),
                None,
            )),
        );
    let policy = Arc::new(RecordingPolicy::default());
    let fallback = llmshim::FallbackConfig::new(vec![
        "primary/first".to_owned(),
        "secondary/second".to_owned(),
    ])
    .max_retries(0);

    let result = llmshim::completion_with_fallback_and_policy(
        &router,
        &request("ignored/model"),
        &fallback,
        None,
        &context(policy),
    )
    .await
    .unwrap();

    assert_eq!(result["choices"][0]["message"]["content"], "ok");
    primary.assert_async().await;
    secondary.assert_async().await;
}

#[tokio::test]
async fn failed_repair_observes_each_usage_before_returning_the_error() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_body(response(json!("1")))
        .expect(2)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("openai", server.url(), None);
    let policy = Arc::new(RecordingPolicy::default());

    let result = ShimClient::new()
        .completion_with_policy(
            &provider,
            "gpt-5.6-luna",
            &structured_request("openai/gpt-5.6-luna"),
            &context(policy.clone()),
        )
        .await;

    assert!(result.is_err());
    upstream.assert_async().await;
    let events = policy.events();
    let usage_events: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            RecordedEvent::Usage { usage, .. } => Some(usage),
            _ => None,
        })
        .collect();
    assert_eq!(usage_events.len(), 2);
    assert!(usage_events
        .iter()
        .all(|usage| usage["cost_usd"].as_f64().is_some_and(|cost| cost > 0.0)));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                RecordedEvent::Finished {
                    outcome: AttemptOutcome::Completed {
                        accounting: AttemptAccounting::UsageObserved
                    },
                    ..
                }
            ))
            .count(),
        2
    );
}

#[tokio::test]
async fn prepared_metadata_uses_resolved_and_native_targets_without_headers() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_body(response(json!("ok")))
        .expect(1)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("resolved-provider", server.url(), Some("secret".into()));
    let policy = Arc::new(RecordingPolicy::default());

    let mut native_override_request = request("caller/model");
    native_override_request["x-resolved-provider"] = json!({"model": "native-model"});
    ShimClient::new()
        .completion_with_policy(
            &provider,
            "resolved-model",
            &native_override_request,
            &context(policy.clone()),
        )
        .await
        .unwrap();

    upstream.assert_async().await;
    let events = policy.events();
    let acquired = events
        .iter()
        .find_map(|event| match event {
            RecordedEvent::Acquired {
                id,
                kind,
                provider_name,
                resolved_model,
                native_model,
                wire,
                account_fingerprint,
                endpoint,
                method,
                native_body,
            } => Some((
                id,
                kind,
                provider_name,
                resolved_model,
                native_model,
                wire,
                account_fingerprint,
                endpoint,
                method,
                native_body,
            )),
            _ => None,
        })
        .unwrap();
    assert_ne!(*acquired.0, uuid::Uuid::nil());
    assert_eq!(*acquired.1, AttemptKind::Completion);
    assert_eq!(acquired.2, "resolved-provider");
    assert_eq!(acquired.3, "resolved-model");
    assert_eq!(acquired.4, "native-model");
    assert_eq!(*acquired.5, llmshim::reasoning::WireFormat::OpenAiChat);
    assert!(acquired
        .6
        .as_deref()
        .is_some_and(|account| { account.starts_with("sha256:") && !account.contains("secret") }));
    assert_eq!(acquired.7, &format!("{}/chat/completions", server.url()));
    assert_eq!(acquired.8, "POST");
    assert_eq!(acquired.9["model"], "native-model");
    assert!(!format!("{acquired:?}").contains("secret"));
}

#[tokio::test]
async fn native_model_override_drives_attempt_pricing() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_body(response(json!("ok")))
        .expect(1)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("openai", server.url(), None);
    let policy = Arc::new(RecordingPolicy::default());
    let mut overridden_request = request("openai/gpt-5.6-luna");
    overridden_request["x-openai"] = json!({"model": "gpt-6-astra"});

    let result = ShimClient::new()
        .completion_with_policy(
            &provider,
            "gpt-5.6-luna",
            &overridden_request,
            &context(policy.clone()),
        )
        .await
        .unwrap();

    upstream.assert_async().await;
    let usage = policy
        .events()
        .into_iter()
        .find_map(|event| match event {
            RecordedEvent::Usage { usage, .. } => Some(usage),
            _ => None,
        })
        .unwrap();
    let expected = llmshim::cost::cost_usd("openai", "gpt-6-astra", &usage).unwrap();
    assert!((usage["cost_usd"].as_f64().unwrap() - expected).abs() < 1e-12);
    assert!((result["usage"]["cost_usd"].as_f64().unwrap() - expected).abs() < 1e-12);
}

async fn assert_native_usage_precedes_invalid_response(
    provider: &dyn Provider,
    model: &str,
    request_model: &str,
) {
    let policy = Arc::new(RecordingPolicy::default());
    let result = ShimClient::new()
        .completion_with_policy(
            provider,
            model,
            &request(request_model),
            &context(policy.clone()),
        )
        .await;
    assert!(result.is_err());
    let events = policy.events();
    let usage_index = events
        .iter()
        .position(|event| matches!(event, RecordedEvent::Usage { .. }))
        .unwrap_or_else(|| {
            panic!(
                "{} native usage must be observed: {events:?}",
                provider.name()
            )
        });
    let invalid_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                RecordedEvent::Finished {
                    outcome: AttemptOutcome::InvalidResponse {
                        accounting: AttemptAccounting::UsageObserved
                    },
                    ..
                }
            )
        })
        .unwrap_or_else(|| panic!("{} invalid response missing: {events:?}", provider.name()));
    assert!(usage_index < invalid_index);
}

#[tokio::test]
async fn native_adapter_usage_is_observed_before_terminal_validation_failure() {
    let mut openai_server = mockito::Server::new_async().await;
    let openai_mock = openai_server
        .mock("POST", "/responses")
        .with_body(
            json!({
                "status":"failed",
                "output":[],
                "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}
            })
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    assert_native_usage_precedes_invalid_response(
        &OpenAi::new("unused".into()).with_base_url(openai_server.url()),
        "gpt-5.6-luna",
        "openai/gpt-5.6-luna",
    )
    .await;
    openai_mock.assert_async().await;

    let mut anthropic_server = mockito::Server::new_async().await;
    let anthropic_mock = anthropic_server
        .mock("POST", "/messages")
        .with_body(
            json!({
                "stop_reason":"pause_turn",
                "content":[],
                "usage":{"input_tokens":7,"output_tokens":3}
            })
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    assert_native_usage_precedes_invalid_response(
        &Anthropic::new("unused".into()).with_base_url(anthropic_server.url()),
        "claude-sonnet-5",
        "anthropic/claude-sonnet-5",
    )
    .await;
    anthropic_mock.assert_async().await;

    let mut gemini_server = mockito::Server::new_async().await;
    let gemini_mock = gemini_server
        .mock("POST", "/models/gemini-3.8-flash:generateContent")
        .match_query(mockito::Matcher::UrlEncoded("key".into(), "unused".into()))
        .with_body(
            json!({
                "candidates":[{"finishReason":"OTHER","content":{"parts":[]}}],
                "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"totalTokenCount":10}
            })
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    assert_native_usage_precedes_invalid_response(
        &Gemini::new("unused".into()).with_base_url(gemini_server.url()),
        "gemini-3.8-flash",
        "gemini/gemini-3.8-flash",
    )
    .await;
    gemini_mock.assert_async().await;

    let mut xai_server = mockito::Server::new_async().await;
    let xai_mock = xai_server
        .mock("POST", "/responses")
        .with_body(
            json!({
                "status":"failed",
                "output":[],
                "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}
            })
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    assert_native_usage_precedes_invalid_response(
        &Xai::new("unused".into()).with_base_url(xai_server.url()),
        "grok-4.7",
        "xai/grok-4.7",
    )
    .await;
    xai_mock.assert_async().await;
}

#[tokio::test]
async fn chatgpt_terminal_usage_and_account_binding_survive_collection_failure() {
    let mut server = mockito::Server::new_async().await;
    let auth_directory = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(auth_directory.path().join("auth.json"));
    std::fs::write(
        auth.auth_path(),
        json!({
            "access_token":"test-access",
            "refresh_token":"test-refresh",
            "account_id":"private-account",
            "expires_at":chrono::Utc::now().timestamp() + 3600
        })
        .to_string(),
    )
    .unwrap();
    let terminal = json!({
        "type":"response.completed",
        "response":{
            "id":"response",
            "status":"failed",
            "output":[],
            "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}
        }
    });
    let upstream = server
        .mock("POST", "/responses")
        .match_header("chatgpt-account-id", "private-account")
        .with_header("content-type", "text/event-stream")
        .with_body(format!(
            "event: response.completed\r\ndata:{terminal}\r\n\r\n"
        ))
        .expect(1)
        .create_async()
        .await;
    let provider = ChatGpt::new(auth).with_base_url(server.url());
    let policy = Arc::new(RecordingPolicy::default());

    let result = ShimClient::new()
        .completion_with_policy(
            &provider,
            "gpt-6-astra",
            &request("chatgpt/gpt-6-astra"),
            &context(policy.clone()),
        )
        .await;

    assert!(result.is_err());
    upstream.assert_async().await;
    let events = policy.events();
    assert!(events.iter().any(|event| matches!(
        event,
        RecordedEvent::Usage { usage, .. } if usage["total_tokens"] == 10
    )));
    let account = events.iter().find_map(|event| match event {
        RecordedEvent::Acquired {
            account_fingerprint,
            ..
        } => account_fingerprint.as_deref(),
        _ => None,
    });
    assert!(account.is_some_and(|account| {
        account.starts_with("sha256:") && !account.contains("private-account")
    }));
    assert!(!format!("{events:?}").contains("private-account"));
}

async fn assert_native_stream_usage_precedes_failure(
    provider: &dyn Provider,
    model: &str,
    request_model: &str,
) {
    let policy = Arc::new(RecordingPolicy::default());
    let chunks: Vec<_> = ShimClient::new()
        .stream_with_policy(
            provider,
            model,
            &request(request_model),
            &context(policy.clone()),
        )
        .await
        .unwrap_or_else(|error| panic!("{} stream did not open: {error:?}", provider.name()))
        .collect()
        .await;
    assert!(chunks.iter().any(Result::is_err));
    let events = policy.events();
    let usage_index = events
        .iter()
        .position(|event| matches!(event, RecordedEvent::Usage { .. }))
        .unwrap_or_else(|| {
            panic!(
                "{} native stream usage must be observed: {events:?}",
                provider.name()
            )
        });
    let failure_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                RecordedEvent::Finished {
                    outcome: AttemptOutcome::StreamFailure {
                        accounting: AttemptAccounting::UsageObserved
                    },
                    ..
                }
            )
        })
        .unwrap_or_else(|| panic!("{} stream failure missing: {events:?}", provider.name()));
    assert!(usage_index < failure_index);
}

#[tokio::test]
async fn native_stream_usage_is_observed_before_normalization_failure() {
    let responses_failure = json!({
        "type":"response.failed",
        "response":{
            "status":"failed",
            "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}
        }
    });

    let mut openai_server = mockito::Server::new_async().await;
    let openai_mock = openai_server
        .mock("POST", "/responses")
        .with_header("content-type", "text/event-stream")
        .with_body(format!("data: {responses_failure}\n\n"))
        .expect(1)
        .create_async()
        .await;
    assert_native_stream_usage_precedes_failure(
        &OpenAi::new("unused".into()).with_base_url(openai_server.url()),
        "gpt-5.6-luna",
        "openai/gpt-5.6-luna",
    )
    .await;
    openai_mock.assert_async().await;

    let mut anthropic_server = mockito::Server::new_async().await;
    let anthropic_mock = anthropic_server
        .mock("POST", "/messages")
        .with_header("content-type", "text/event-stream")
        .with_body(format!(
            "data: {}\n\n",
            json!({
                "type":"error",
                "usage":{"input_tokens":7,"output_tokens":3},
                "error":{"type":"overloaded_error"}
            })
        ))
        .expect(1)
        .create_async()
        .await;
    assert_native_stream_usage_precedes_failure(
        &Anthropic::new("unused".into()).with_base_url(anthropic_server.url()),
        "claude-sonnet-5",
        "anthropic/claude-sonnet-5",
    )
    .await;
    anthropic_mock.assert_async().await;

    let mut gemini_server = mockito::Server::new_async().await;
    let gemini_mock = gemini_server
        .mock("POST", "/models/gemini-3.8-flash:streamGenerateContent")
        .match_query(mockito::Matcher::Exact("key=unused&alt=sse".into()))
        .with_header("content-type", "text/event-stream")
        .with_body(format!(
            "data: {}\n\n",
            json!({
                "error":{"code":500},
                "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"totalTokenCount":10}
            })
        ))
        .expect(1)
        .create_async()
        .await;
    assert_native_stream_usage_precedes_failure(
        &Gemini::new("unused".into()).with_base_url(gemini_server.url()),
        "gemini-3.8-flash",
        "gemini/gemini-3.8-flash",
    )
    .await;
    gemini_mock.assert_async().await;

    let mut xai_server = mockito::Server::new_async().await;
    let xai_mock = xai_server
        .mock("POST", "/responses")
        .with_header("content-type", "text/event-stream")
        .with_body(format!("data: {responses_failure}\n\n"))
        .expect(1)
        .create_async()
        .await;
    assert_native_stream_usage_precedes_failure(
        &Xai::new("unused".into()).with_base_url(xai_server.url()),
        "grok-4.7",
        "xai/grok-4.7",
    )
    .await;
    xai_mock.assert_async().await;
}

#[tokio::test]
async fn usage_and_finish_observer_failures_stop_unary_work_and_retain_liability() {
    for failing_event in [FailingEvent::Usage, FailingEvent::Finished] {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(response(json!("ok")))
            .expect(1)
            .create_async()
            .await;
        let provider = OpenAiCompatible::new("local", server.url(), None);
        let policy = Arc::new(RecordingPolicy::failing(failing_event));

        let error = ShimClient::new()
            .completion_with_policy(
                &provider,
                "test",
                &request("local/test"),
                &context(policy.clone()),
            )
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("attempt policy coordinator unavailable"));
        upstream.assert_async().await;
        assert!(policy
            .events()
            .iter()
            .any(|event| matches!(event, RecordedEvent::Abandoned { .. })));
    }
}

#[tokio::test]
async fn stream_observer_failure_is_caller_visible_before_completion() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(successful_stream("ok"))
        .expect(1)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("local", server.url(), None);
    let policy = Arc::new(RecordingPolicy::failing(FailingEvent::Usage));
    let chunks: Vec<_> = ShimClient::new()
        .stream_with_policy(
            &provider,
            "test",
            &request("local/test"),
            &context(policy.clone()),
        )
        .await
        .unwrap()
        .collect()
        .await;

    assert!(chunks
        .iter()
        .any(|chunk| chunk.as_ref().is_err_and(|error| {
            error
                .to_string()
                .contains("attempt policy coordinator unavailable")
        })));
    assert!(policy
        .events()
        .iter()
        .any(|event| matches!(event, RecordedEvent::Abandoned { .. })));
    upstream.assert_async().await;
}

#[tokio::test]
async fn failed_drop_observation_is_conservative_and_does_not_panic() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(successful_stream("unused"))
        .expect(1)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("local", server.url(), None);
    let policy = Arc::new(RecordingPolicy::failing_abandonment());
    let stream = ShimClient::new()
        .stream_with_policy(
            &provider,
            "test",
            &request("local/test"),
            &context(policy.clone()),
        )
        .await
        .unwrap();

    drop(stream);
    assert!(policy
        .events()
        .iter()
        .any(|event| matches!(event, RecordedEvent::Abandoned { .. })));
    upstream.assert_async().await;
}

#[tokio::test]
async fn cancelling_an_inflight_usage_observation_marks_the_attempt_abandoned() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(successful_stream("ok"))
        .expect(1)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("local", server.url(), None);
    let policy = Arc::new(CancellationPolicy::new());
    let mut stream = ShimClient::new()
        .stream_with_policy(
            &provider,
            "test",
            &request("local/test"),
            &DispatchPolicyContext::new(policy.clone()),
        )
        .await
        .unwrap();
    let task = tokio::spawn(async move { while stream.next().await.is_some() {} });

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        policy.usage_started.notified(),
    )
    .await
    .unwrap();
    task.abort();
    let _ = task.await;

    assert!(policy.acquired.load(Ordering::SeqCst));
    assert!(policy.abandoned.load(Ordering::SeqCst));
    upstream.assert_async().await;
}

#[tokio::test]
async fn streams_report_usage_terminal_failure_and_abandonment() {
    let mut server = mockito::Server::new_async().await;
    let successful = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(successful_stream("ok"))
        .expect(1)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("local", server.url(), None);
    let successful_policy = Arc::new(RecordingPolicy::default());
    let chunks: Vec<_> = ShimClient::new()
        .stream_with_policy(
            &provider,
            "test",
            &request("local/test"),
            &context(successful_policy.clone()),
        )
        .await
        .unwrap()
        .collect()
        .await;
    assert!(chunks.iter().all(Result::is_ok));
    successful.assert_async().await;
    let successful_events = successful_policy.events();
    assert!(successful_events.iter().any(|event| matches!(
        event,
        RecordedEvent::Usage { usage, .. } if usage["total_tokens"] == 10
    )));
    assert!(successful_events.iter().any(|event| matches!(
        event,
        RecordedEvent::Finished {
            outcome: AttemptOutcome::Completed {
                accounting: AttemptAccounting::UsageObserved
            },
            ..
        }
    )));

    successful.remove_async().await;
    let failed = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body("data: {\"error\":{\"message\":\"failed\"}}\n\n")
        .expect(1)
        .create_async()
        .await;
    let failed_policy = Arc::new(RecordingPolicy::default());
    let failed_chunks: Vec<_> = ShimClient::new()
        .stream_with_policy(
            &provider,
            "test",
            &request("local/test"),
            &context(failed_policy.clone()),
        )
        .await
        .unwrap()
        .collect()
        .await;
    assert!(failed_chunks.iter().any(Result::is_err));
    failed.assert_async().await;
    assert!(failed_policy.events().iter().any(|event| matches!(
        event,
        RecordedEvent::Finished {
            outcome: AttemptOutcome::StreamFailure {
                accounting: AttemptAccounting::Unknown
            },
            ..
        }
    )));

    failed.remove_async().await;
    let abandoned = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(successful_stream("unused"))
        .expect(1)
        .create_async()
        .await;
    let abandoned_policy = Arc::new(RecordingPolicy::default());
    let stream = ShimClient::new()
        .stream_with_policy(
            &provider,
            "test",
            &request("local/test"),
            &context(abandoned_policy.clone()),
        )
        .await
        .unwrap();
    drop(stream);
    abandoned.assert_async().await;
    assert!(abandoned_policy.events().iter().any(|event| matches!(
        event,
        RecordedEvent::Abandoned {
            outcome: AttemptOutcome::Abandoned {
                kind: AttemptKind::Stream,
                accounting: AttemptAccounting::Unknown
            },
            ..
        }
    )));
}

#[tokio::test]
async fn cumulative_and_duplicate_stream_usage_share_one_attempt_identity() {
    let mut server = mockito::Server::new_async().await;
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"response","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}),
        json!({"id":"response","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":1,"total_tokens":8}}),
        json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":1,"total_tokens":8}}),
        json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}),
    );
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .expect(1)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("local", server.url(), None);
    let policy = Arc::new(RecordingPolicy::default());

    let chunks: Vec<_> = ShimClient::new()
        .stream_with_policy(
            &provider,
            "test",
            &request("local/test"),
            &context(policy.clone()),
        )
        .await
        .unwrap()
        .collect()
        .await;

    assert!(chunks.iter().all(Result::is_ok));
    upstream.assert_async().await;
    let events = policy.events();
    let usage_events: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            RecordedEvent::Usage { id, usage } => Some((*id, usage["total_tokens"].as_u64())),
            _ => None,
        })
        .collect();
    assert_eq!(usage_events.len(), 3);
    assert_eq!(usage_events[0].1, Some(8));
    assert_eq!(usage_events[1].1, Some(8));
    assert_eq!(usage_events[2].1, Some(10));
    assert_eq!(usage_events[0].0, usage_events[1].0);
    assert_eq!(usage_events[1].0, usage_events[2].0);
    let finished_id = events.iter().find_map(|event| match event {
        RecordedEvent::Finished {
            id,
            outcome:
                AttemptOutcome::Completed {
                    accounting: AttemptAccounting::UsageObserved,
                },
        } => Some(*id),
        _ => None,
    });
    assert_eq!(finished_id, Some(usage_events[0].0));
}

#[tokio::test]
async fn owned_buffered_stream_failed_repair_observes_both_attempts() {
    let mut server = mockito::Server::new_async().await;
    let invalid_streams = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(successful_stream("1"))
        .expect(2)
        .create_async()
        .await;
    let router = llmshim::router::Router::new().register(
        "local",
        Box::new(OpenAiCompatible::new("local", server.url(), None)),
    );
    let policy = Arc::new(RecordingPolicy::default());
    let stream = llmshim::stream_with_policy(
        &router,
        &structured_request("local/test"),
        &context(policy.clone()),
    )
    .await
    .unwrap();
    let chunks: Vec<_> = stream.collect().await;

    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].is_err());
    invalid_streams.assert_async().await;
    let events = policy.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RecordedEvent::Acquired { .. }))
            .count(),
        2
    );
    let attempts_with_usage: std::collections::HashSet<_> = events
        .iter()
        .filter_map(|event| match event {
            RecordedEvent::Usage { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(attempts_with_usage.len(), 2);
}

#[test]
fn policy_diagnostics_do_not_expose_native_request_content() {
    struct NoopPolicy;
    impl AttemptPolicy for NoopPolicy {
        fn acquire<'a>(
            &'a self,
            _attempt: &'a PreparedAttempt<'a>,
        ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
            Box::pin(async { Ok(()) })
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

    let context = DispatchPolicyContext::new(Arc::new(NoopPolicy));
    assert_eq!(format!("{context:?}"), "DispatchPolicyContext { .. }");
}

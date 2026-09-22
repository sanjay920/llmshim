use futures::StreamExt;
use llmshim::{
    client::ShimClient,
    policy::{
        AttemptAccounting, AttemptEvent, AttemptIdentity, AttemptKind, AttemptOutcome,
        AttemptPolicy, AttemptPolicyFuture, AttemptPolicyRefusal, AttemptPolicyRefusalKind,
        DispatchPolicyContext, PreparedAttempt,
    },
    providers::openai_compat::OpenAiCompatible,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
enum RecordedEvent {
    Acquired {
        id: uuid::Uuid,
        kind: AttemptKind,
        provider_name: String,
        resolved_model: String,
        native_model: Option<String>,
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
}

impl Default for RecordingPolicy {
    fn default() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            refuse_acquire_number: None,
            refusal_kind: AttemptPolicyRefusalKind::Budget,
        }
    }
}

impl RecordingPolicy {
    fn refusing(acquire_number: usize) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            refuse_acquire_number: Some(acquire_number),
            refusal_kind: AttemptPolicyRefusalKind::Budget,
        }
    }

    fn refusing_with_kind(acquire_number: usize, refusal_kind: AttemptPolicyRefusalKind) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            refuse_acquire_number: Some(acquire_number),
            refusal_kind,
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
                native_model: identity.native_model().map(str::to_owned),
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
    ) -> AttemptPolicyFuture<'a, ()> {
        Box::pin(async move {
            let recorded = match event {
                AttemptEvent::ResponseHeaders { status } => RecordedEvent::ResponseHeaders {
                    id: attempt.id(),
                    status,
                },
                AttemptEvent::Usage { usage } => RecordedEvent::Usage {
                    id: attempt.id(),
                    usage: usage.clone(),
                },
                AttemptEvent::Finished(outcome) => RecordedEvent::Finished {
                    id: attempt.id(),
                    outcome,
                },
            };
            self.events.lock().unwrap().push(recorded);
        })
    }

    fn observe_abandoned(&self, attempt: &AttemptIdentity, outcome: AttemptOutcome) {
        self.events.lock().unwrap().push(RecordedEvent::Abandoned {
            id: attempt.id(),
            outcome,
        });
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
        &request("ignored/model"),
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

    ShimClient::new()
        .completion_with_policy(
            &provider,
            "resolved-model",
            &request("caller/model"),
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
                endpoint,
                method,
                native_body,
            } => Some((
                id,
                kind,
                provider_name,
                resolved_model,
                native_model,
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
    assert_eq!(acquired.4.as_deref(), Some("resolved-model"));
    assert_eq!(acquired.5, &format!("{}/chat/completions", server.url()));
    assert_eq!(acquired.6, "POST");
    assert_eq!(acquired.7["model"], "resolved-model");
    assert!(!format!("{acquired:?}").contains("secret"));
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
        ) -> AttemptPolicyFuture<'a, ()> {
            Box::pin(async {})
        }

        fn observe_abandoned(&self, _attempt: &AttemptIdentity, _outcome: AttemptOutcome) {}
    }

    let context = DispatchPolicyContext::new(Arc::new(NoopPolicy));
    assert_eq!(format!("{context:?}"), "DispatchPolicyContext { .. }");
}

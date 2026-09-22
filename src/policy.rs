//! Trusted, out-of-band policy hooks for individual provider attempts.

use crate::error::ShimError;
use crate::reasoning::{ReplayTarget, WireFormat};
use serde_json::Value;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

pub type AttemptPolicyFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone)]
pub struct DispatchPolicyContext {
    policy: Arc<dyn AttemptPolicy>,
    state: Arc<DispatchPolicyState>,
}

struct DispatchPolicyState {
    last_refusal: std::sync::Mutex<Option<AttemptPolicyRefusal>>,
    logical_deadline: std::sync::Mutex<Option<tokio::time::Instant>>,
}

impl DispatchPolicyContext {
    pub fn new(policy: Arc<dyn AttemptPolicy>) -> Self {
        Self {
            policy,
            state: Arc::new(DispatchPolicyState {
                last_refusal: std::sync::Mutex::new(None),
                logical_deadline: std::sync::Mutex::new(None),
            }),
        }
    }

    pub(crate) fn with_logical_deadline(self, deadline: tokio::time::Instant) -> Self {
        *self.state.logical_deadline.lock().unwrap() = Some(deadline);
        self
    }

    pub(crate) fn ensure_logical_active(&self) -> Result<(), LogicalDeadlineExpired> {
        if self
            .state
            .logical_deadline
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|deadline| tokio::time::Instant::now() >= *deadline)
        {
            Err(LogicalDeadlineExpired)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn acquire(
        &self,
        kind: AttemptKind,
        resolved_model: &str,
        prepared_target: &ReplayTarget,
        endpoint: &str,
        native_body: &Value,
    ) -> Result<AttemptTracker, AttemptAcquireError> {
        self.ensure_logical_active()
            .map_err(|_| AttemptAcquireError::LogicalDeadline)?;
        let identity = AttemptIdentity {
            id: uuid::Uuid::new_v4(),
            kind,
            provider_name: prepared_target.provider.clone(),
            resolved_model: resolved_model.to_owned(),
            native_model: prepared_target.model.clone(),
            wire: prepared_target.wire,
            account_fingerprint: prepared_target.account.clone(),
            endpoint: sanitized_endpoint(endpoint),
        };
        let prepared_attempt = PreparedAttempt {
            identity: &identity,
            method: "POST",
            native_body,
        };
        *self.state.last_refusal.lock().unwrap() = None;
        let admission_result = self.policy.acquire(&prepared_attempt).await;
        if let Err(refusal) = admission_result {
            if self.ensure_logical_active().is_err() {
                return Err(AttemptAcquireError::LogicalDeadline);
            }
            *self.state.last_refusal.lock().unwrap() = Some(refusal);
            return Err(AttemptAcquireError::Refusal(refusal));
        }
        let tracker = AttemptTracker {
            shared: Arc::new(AttemptTrackerShared {
                context: self.clone(),
                identity,
                state: std::sync::Mutex::new(AttemptTrackerState {
                    usage_observed: false,
                    finished: false,
                }),
            }),
        };
        if self.ensure_logical_active().is_err() {
            drop(tracker);
            return Err(AttemptAcquireError::LogicalDeadline);
        }
        Ok(tracker)
    }

    async fn observe(
        &self,
        identity: &AttemptIdentity,
        event: AttemptEvent<'_>,
    ) -> Result<(), AttemptPolicyError> {
        self.policy.observe(identity, event).await
    }

    async fn observe_usage(
        &self,
        identity: &AttemptIdentity,
        observation: AttemptUsageObservation<'_>,
    ) -> Result<(), AttemptPolicyError> {
        self.policy.observe_usage(identity, observation).await
    }

    #[cfg(feature = "gateway")]
    pub(crate) fn take_last_refusal(&self) -> Option<AttemptPolicyRefusal> {
        self.state.last_refusal.lock().unwrap().take()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LogicalDeadlineExpired;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptAcquireError {
    Refusal(AttemptPolicyRefusal),
    LogicalDeadline,
}

impl std::fmt::Debug for DispatchPolicyContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DispatchPolicyContext")
            .finish_non_exhaustive()
    }
}

pub trait AttemptPolicy: Send + Sync {
    /// Admission must retain any conservative liability until a successful
    /// terminal observation resolves it. Callback failure, cancellation, and
    /// stream abandonment must never release that liability as zero.
    fn acquire<'a>(
        &'a self,
        attempt: &'a PreparedAttempt<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>>;

    fn observe<'a>(
        &'a self,
        attempt: &'a AttemptIdentity,
        event: AttemptEvent<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>>;

    fn observe_usage<'a>(
        &'a self,
        attempt: &'a AttemptIdentity,
        observation: AttemptUsageObservation<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
        self.observe(
            attempt,
            AttemptEvent::Usage {
                usage: observation.usage(),
            },
        )
    }

    /// The acquire-time liability remains authoritative when this best-effort
    /// drop notification fails.
    fn observe_abandoned(
        &self,
        attempt: &AttemptIdentity,
        outcome: AttemptOutcome,
    ) -> Result<(), AttemptPolicyError>;
}

#[derive(Clone, Copy, Debug)]
pub struct AttemptUsageObservation<'a> {
    usage: &'a Value,
    terminal: bool,
    counters_complete: bool,
    explicit_zero: bool,
}

impl<'a> AttemptUsageObservation<'a> {
    pub fn new(
        usage: &'a Value,
        terminal: bool,
        counters_complete: bool,
        explicit_zero: bool,
    ) -> Self {
        Self {
            usage,
            terminal,
            counters_complete,
            explicit_zero,
        }
    }

    pub fn usage(self) -> &'a Value {
        self.usage
    }

    pub fn terminal(self) -> bool {
        self.terminal
    }

    pub fn counters_complete(self) -> bool {
        self.counters_complete
    }

    pub fn explicit_zero(self) -> bool {
        self.explicit_zero
    }
}

pub struct PreparedAttempt<'a> {
    identity: &'a AttemptIdentity,
    method: &'static str,
    native_body: &'a Value,
}

impl<'a> PreparedAttempt<'a> {
    pub fn identity(&self) -> &AttemptIdentity {
        self.identity
    }

    pub fn method(&self) -> &'static str {
        self.method
    }

    pub fn native_body(&self) -> &'a Value {
        self.native_body
    }
}

impl std::fmt::Debug for PreparedAttempt<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedAttempt")
            .field("identity", self.identity)
            .field("method", &self.method)
            .field("native_body", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AttemptIdentity {
    id: uuid::Uuid,
    kind: AttemptKind,
    provider_name: String,
    resolved_model: String,
    native_model: String,
    wire: WireFormat,
    account_fingerprint: Option<String>,
    endpoint: String,
}

impl AttemptIdentity {
    pub fn id(&self) -> uuid::Uuid {
        self.id
    }

    pub fn kind(&self) -> AttemptKind {
        self.kind
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub fn resolved_model(&self) -> &str {
        &self.resolved_model
    }

    pub fn native_model(&self) -> &str {
        &self.native_model
    }

    pub fn wire(&self) -> WireFormat {
        self.wire
    }

    pub fn account_fingerprint(&self) -> Option<&str> {
        self.account_fingerprint.as_deref()
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl std::fmt::Debug for AttemptIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AttemptIdentity")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("provider_name", &self.provider_name)
            .field("resolved_model", &self.resolved_model)
            .field("native_model", &self.native_model)
            .field("wire", &self.wire)
            .field(
                "account_fingerprint",
                &self.account_fingerprint.as_ref().map(|_| "<redacted>"),
            )
            .field("endpoint", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptKind {
    Completion,
    Stream,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptAccounting {
    UsageObserved,
    NoUsageReported,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    Completed {
        accounting: AttemptAccounting,
    },
    HttpFailure {
        status: u16,
        accounting: AttemptAccounting,
    },
    TransportFailure {
        accounting: AttemptAccounting,
    },
    InvalidResponse {
        accounting: AttemptAccounting,
    },
    StreamFailure {
        accounting: AttemptAccounting,
    },
    Abandoned {
        kind: AttemptKind,
        accounting: AttemptAccounting,
    },
}

#[derive(Debug)]
pub enum AttemptEvent<'a> {
    ResponseHeaders {
        status: u16,
    },
    /// A cumulative accounting snapshot. Streams may update it more than once;
    /// consumers must key settlement by [`AttemptIdentity::id`].
    Usage {
        usage: &'a Value,
    },
    Finished(AttemptOutcome),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptPolicyRefusalKind {
    /// Provider-wide capacity refusal. Fallback may only advance to a target
    /// resolved to a different provider.
    ProviderLimit,
    TenantLimit,
    Budget,
    Unpriceable,
    CoordinatorUnavailable,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttemptPolicyRefusal {
    kind: AttemptPolicyRefusalKind,
    retry_after: Option<Duration>,
}

impl AttemptPolicyRefusal {
    pub fn new(kind: AttemptPolicyRefusalKind, retry_after: Option<Duration>) -> Self {
        Self { kind, retry_after }
    }

    pub fn kind(&self) -> AttemptPolicyRefusalKind {
        self.kind
    }

    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    pub(crate) fn into_shim_error(self) -> ShimError {
        let (status, body) = match self.kind {
            AttemptPolicyRefusalKind::ProviderLimit => (429, "provider attempt limit exceeded"),
            AttemptPolicyRefusalKind::TenantLimit => (429, "tenant attempt limit exceeded"),
            AttemptPolicyRefusalKind::Budget => (429, "attempt budget exhausted"),
            AttemptPolicyRefusalKind::Unpriceable => {
                (400, "attempt cannot be admitted under the active policy")
            }
            AttemptPolicyRefusalKind::CoordinatorUnavailable => {
                (503, "attempt policy coordinator unavailable")
            }
            AttemptPolicyRefusalKind::Other => (429, "attempt refused by policy"),
        };
        ShimError::ProviderError {
            status,
            body: body.to_owned(),
            retry_after: self.retry_after,
        }
    }
}

impl std::fmt::Display for AttemptPolicyRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "attempt refused by policy ({:?})", self.kind)
    }
}

impl std::error::Error for AttemptPolicyRefusal {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptPolicyErrorKind {
    CoordinatorUnavailable,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttemptPolicyError {
    kind: AttemptPolicyErrorKind,
}

impl AttemptPolicyError {
    pub fn new(kind: AttemptPolicyErrorKind) -> Self {
        Self { kind }
    }

    pub fn kind(&self) -> AttemptPolicyErrorKind {
        self.kind
    }

    pub(crate) fn into_shim_error(self) -> ShimError {
        ShimError::ProviderError {
            status: 503,
            body: match self.kind {
                AttemptPolicyErrorKind::CoordinatorUnavailable => {
                    "attempt policy coordinator unavailable"
                }
                AttemptPolicyErrorKind::Other => "attempt policy observation failed",
            }
            .to_owned(),
            retry_after: None,
        }
    }
}

impl std::fmt::Display for AttemptPolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "attempt policy observation failed ({:?})",
            self.kind
        )
    }
}

impl std::error::Error for AttemptPolicyError {}

pub(crate) struct AttemptTracker {
    shared: Arc<AttemptTrackerShared>,
}

struct AttemptTrackerShared {
    context: DispatchPolicyContext,
    identity: AttemptIdentity,
    state: std::sync::Mutex<AttemptTrackerState>,
}

struct AttemptTrackerState {
    usage_observed: bool,
    finished: bool,
}

pub(crate) struct AttemptCancellationGuard {
    shared: Arc<AttemptTrackerShared>,
}

impl AttemptTracker {
    pub(crate) async fn response_headers(&self, status: u16) -> Result<(), AttemptPolicyError> {
        self.shared
            .context
            .observe(
                &self.shared.identity,
                AttemptEvent::ResponseHeaders { status },
            )
            .await
    }

    pub(crate) async fn usage(
        &mut self,
        usage: &Value,
        terminal: bool,
        counters_complete: bool,
        explicit_zero: bool,
    ) -> Result<(), AttemptPolicyError> {
        self.shared
            .context
            .observe_usage(
                &self.shared.identity,
                AttemptUsageObservation::new(usage, terminal, counters_complete, explicit_zero),
            )
            .await?;
        self.shared.state.lock().unwrap().usage_observed = true;
        Ok(())
    }

    pub(crate) fn accounting(&self, completed: bool) -> AttemptAccounting {
        if self.shared.state.lock().unwrap().usage_observed {
            AttemptAccounting::UsageObserved
        } else if completed {
            AttemptAccounting::NoUsageReported
        } else {
            AttemptAccounting::Unknown
        }
    }

    pub(crate) async fn finish(
        &mut self,
        outcome: AttemptOutcome,
    ) -> Result<(), AttemptPolicyError> {
        if self.shared.state.lock().unwrap().finished {
            return Ok(());
        }
        self.shared
            .context
            .observe(&self.shared.identity, AttemptEvent::Finished(outcome))
            .await?;
        self.shared.state.lock().unwrap().finished = true;
        Ok(())
    }

    pub(crate) fn cancellation_guard(&self) -> AttemptCancellationGuard {
        AttemptCancellationGuard {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for AttemptTracker {
    fn drop(&mut self) {
        abandon_attempt(&self.shared);
    }
}

impl Drop for AttemptCancellationGuard {
    fn drop(&mut self) {
        abandon_attempt(&self.shared);
    }
}

fn abandon_attempt(shared: &AttemptTrackerShared) {
    let outcome = {
        let mut state = shared.state.lock().unwrap();
        if state.finished {
            return;
        }
        state.finished = true;
        AttemptOutcome::Abandoned {
            kind: shared.identity.kind,
            accounting: if state.usage_observed {
                AttemptAccounting::UsageObserved
            } else {
                AttemptAccounting::Unknown
            },
        }
    };
    let _ = shared
        .context
        .policy
        .observe_abandoned(&shared.identity, outcome);
}

fn sanitized_endpoint(endpoint: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(endpoint) else {
        return "<invalid-endpoint>".to_owned();
    };
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingAdmissionPolicy {
        acquisitions: AtomicUsize,
        abandonments: AtomicUsize,
        acquisition_delay: Option<Duration>,
        finish_delay: Option<Duration>,
    }

    impl AttemptPolicy for RecordingAdmissionPolicy {
        fn acquire<'a>(
            &'a self,
            _attempt: &'a PreparedAttempt<'a>,
        ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
            Box::pin(async move {
                self.acquisitions.fetch_add(1, Ordering::SeqCst);
                if let Some(delay) = self.acquisition_delay {
                    tokio::time::sleep(delay).await;
                }
                Ok(())
            })
        }

        fn observe<'a>(
            &'a self,
            _attempt: &'a AttemptIdentity,
            event: AttemptEvent<'a>,
        ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
            Box::pin(async move {
                if matches!(event, AttemptEvent::Finished(_)) {
                    if let Some(delay) = self.finish_delay {
                        tokio::time::sleep(delay).await;
                    }
                }
                Ok(())
            })
        }

        fn observe_abandoned(
            &self,
            _attempt: &AttemptIdentity,
            _outcome: AttemptOutcome,
        ) -> Result<(), AttemptPolicyError> {
            self.abandonments.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn test_target() -> ReplayTarget {
        ReplayTarget::new("test", "model", WireFormat::OpenAiChat)
    }

    #[test]
    fn endpoint_metadata_never_keeps_query_credentials() {
        assert_eq!(
            sanitized_endpoint("https://example.test/models/m:generate?key=secret&alt=sse"),
            "https://example.test/models/m:generate"
        );
        assert_eq!(
            sanitized_endpoint("https://user:password@example.test/v1"),
            "https://example.test/v1"
        );
        assert_eq!(sanitized_endpoint("not a url"), "<invalid-endpoint>");
    }

    #[tokio::test(start_paused = true)]
    async fn expired_deadline_refuses_later_repair_and_fallback_admissions() {
        let policy = Arc::new(RecordingAdmissionPolicy {
            acquisitions: AtomicUsize::new(0),
            abandonments: AtomicUsize::new(0),
            acquisition_delay: None,
            finish_delay: None,
        });
        let context = DispatchPolicyContext::new(policy.clone())
            .with_logical_deadline(tokio::time::Instant::now() + Duration::from_secs(2));
        let target = test_target();
        let body = serde_json::json!({"model":"model"});
        let mut first = context
            .acquire(
                AttemptKind::Completion,
                "model",
                &target,
                "http://127.0.0.1:1/v1",
                &body,
            )
            .await
            .unwrap();
        first
            .finish(AttemptOutcome::Completed {
                accounting: AttemptAccounting::NoUsageReported,
            })
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;

        for later_attempt in ["repair", "fallback"] {
            let refusal = context
                .acquire(
                    AttemptKind::Completion,
                    later_attempt,
                    &target,
                    "http://127.0.0.1:1/v1",
                    &body,
                )
                .await;
            assert!(
                matches!(refusal, Err(AttemptAcquireError::LogicalDeadline)),
                "{later_attempt} must stop at the original deadline"
            );
        }
        assert_eq!(policy.acquisitions.load(Ordering::SeqCst), 1);
        assert_eq!(policy.abandonments.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn successful_but_late_admission_is_abandoned_exactly_once() {
        let policy = Arc::new(RecordingAdmissionPolicy {
            acquisitions: AtomicUsize::new(0),
            abandonments: AtomicUsize::new(0),
            acquisition_delay: Some(Duration::from_secs(3)),
            finish_delay: None,
        });
        let context = DispatchPolicyContext::new(policy.clone())
            .with_logical_deadline(tokio::time::Instant::now() + Duration::from_secs(2));
        let target = test_target();
        let body = serde_json::json!({"model":"model"});
        let result = context
            .acquire(
                AttemptKind::Completion,
                "model",
                &target,
                "http://127.0.0.1:1/v1",
                &body,
            )
            .await;
        assert!(matches!(result, Err(AttemptAcquireError::LogicalDeadline)));
        assert_eq!(policy.acquisitions.load(Ordering::SeqCst), 1);
        assert_eq!(policy.abandonments.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn repair_attempt_cannot_start_after_the_original_logical_deadline() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_body(
                serde_json::json!({
                    "id": "response",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "1"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let provider =
            crate::providers::openai_compat::OpenAiCompatible::new("local", server.url(), None);
        let policy = Arc::new(RecordingAdmissionPolicy {
            acquisitions: AtomicUsize::new(0),
            abandonments: AtomicUsize::new(0),
            acquisition_delay: None,
            finish_delay: Some(Duration::from_millis(1100)),
        });
        let context = DispatchPolicyContext::new(policy.clone())
            .with_logical_deadline(tokio::time::Instant::now() + Duration::from_secs(1));
        let error = crate::client::ShimClient::new()
            .completion_with_policy(
                &provider,
                "test",
                &serde_json::json!({
                    "model": "local/test",
                    "messages": [{"role": "user", "content": "answer"}],
                    "response_format": {
                        "type": "json_schema",
                        "json_schema": {
                            "name": "answer",
                            "schema": {"type": "integer", "minimum": 3}
                        }
                    },
                    "x-shim": {"structured_output": "prompt"}
                }),
                &context,
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
        assert_eq!(policy.acquisitions.load(Ordering::SeqCst), 1);
        upstream.assert_async().await;
    }
}

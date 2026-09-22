use crate::breaker::ProviderBreaker;
use crate::error::{Result, ShimError};
use crate::policy::{
    AttemptAcquireError, AttemptKind, AttemptOutcome, AttemptPolicyError, AttemptPolicyRefusal,
    AttemptTracker, DispatchPolicyContext,
};
use crate::provider::{Provider, ProviderRequest};
use crate::reasoning::ReplayTarget;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{Stream, StreamExt};
use reqwest::header::HeaderMap;
use reqwest::Client;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

mod body;
mod deadline;
pub use deadline::AttemptDeadlines;

/// Retry bounds, resolved once from the environment (with defaults) at
/// construction time. Internal/additive to `ShimClient` — not part of the
/// public API surface.
#[derive(Clone, Copy, Debug)]
struct RetryConfig {
    /// Number of *retries* after the initial attempt.
    max_retries: u32,
    /// Base for exponential backoff (attempt 0 → `base`, 1 → 2·base, …).
    base: Duration,
    /// Hard cap on any single wait, whether server-dictated or computed.
    cap: Duration,
}

pub(crate) enum DispatchFailure {
    Upstream(ShimError),
    Local(ShimError),
    LocalTimeout(ShimError),
    LogicalTimeout(ShimError),
    PolicyRefusal(AttemptPolicyRefusal),
    PolicyObservation(AttemptPolicyError),
}

pub(crate) type DispatchResult<T> = std::result::Result<T, DispatchFailure>;

impl DispatchFailure {
    pub(crate) fn into_public(self) -> ShimError {
        match self {
            Self::Upstream(error)
            | Self::Local(error)
            | Self::LocalTimeout(error)
            | Self::LogicalTimeout(error) => error,
            Self::PolicyRefusal(refusal) => refusal.into_shim_error(),
            Self::PolicyObservation(error) => error.into_shim_error(),
        }
    }

    fn map_upstream(self, map: impl FnOnce(ShimError) -> ShimError) -> Self {
        match self {
            Self::Upstream(error) => Self::Upstream(map(error)),
            other => other,
        }
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
        }
    }
}

impl RetryConfig {
    /// Read `LLMSHIM_MAX_RETRIES` and `LLMSHIM_MAX_BACKOFF_SECS`, falling back
    /// to defaults on absence or unparseable values (never panics).
    fn from_env() -> Self {
        let d = Self::default();
        let max_retries = env_parse("LLMSHIM_MAX_RETRIES").unwrap_or(d.max_retries);
        let cap_secs = env_parse::<u64>("LLMSHIM_MAX_BACKOFF_SECS").unwrap_or(d.cap.as_secs());
        Self {
            max_retries,
            base: d.base,
            cap: Duration::from_secs(cap_secs),
        }
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.trim().parse().ok()
}

fn build_http_client(connect_timeout: Duration) -> Client {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect_timeout)
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(4)
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_nodelay(true)
        .build()
        .expect("failed to build HTTP client")
}

fn deadline_after(duration: Duration) -> Option<tokio::time::Instant> {
    tokio::time::Instant::now().checked_add(duration)
}

fn earlier_deadline(
    first: tokio::time::Instant,
    second: tokio::time::Instant,
) -> tokio::time::Instant {
    first.min(second)
}

fn body_deadline(
    attempt_deadline: tokio::time::Instant,
    body_total: Duration,
) -> tokio::time::Instant {
    deadline_after(body_total)
        .map(|deadline| earlier_deadline(attempt_deadline, deadline))
        .unwrap_or(attempt_deadline)
}

async fn bounded_policy<T>(
    future: impl std::future::Future<Output = T>,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
) -> std::result::Result<T, ()> {
    let callback_deadline = deadline_after(callback_timeout).ok_or(())?;
    tokio::time::timeout_at(
        earlier_deadline(attempt_deadline, callback_deadline),
        future,
    )
    .await
    .map_err(|_| ())
}

fn coordinator_timeout_refusal() -> AttemptPolicyRefusal {
    AttemptPolicyRefusal::new(
        crate::policy::AttemptPolicyRefusalKind::CoordinatorUnavailable,
        None,
    )
}

fn coordinator_timeout_observation() -> DispatchFailure {
    DispatchFailure::PolicyObservation(AttemptPolicyError::new(
        crate::policy::AttemptPolicyErrorKind::CoordinatorUnavailable,
    ))
}

fn timeout_504() -> DispatchFailure {
    DispatchFailure::LocalTimeout(ShimError::ProviderError {
        status: 504,
        body: "upstream attempt timed out".into(),
        retry_after: None,
    })
}

fn logical_timeout_504() -> DispatchFailure {
    DispatchFailure::LogicalTimeout(ShimError::ProviderError {
        status: 504,
        body: "proxy logical request timed out".into(),
        retry_after: None,
    })
}

fn map_attempt_acquire_error(error: AttemptAcquireError) -> DispatchFailure {
    match error {
        AttemptAcquireError::Refusal(refusal) => DispatchFailure::PolicyRefusal(refusal),
        AttemptAcquireError::LogicalDeadline => logical_timeout_504(),
    }
}

async fn finish_transport_failure(
    tracker: &mut Option<AttemptTracker>,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
) -> DispatchResult<()> {
    let Some(tracker) = tracker.as_mut() else {
        return Ok(());
    };
    let accounting = tracker.accounting(false);
    bounded_policy(
        tracker.finish(AttemptOutcome::TransportFailure { accounting }),
        callback_timeout,
        attempt_deadline,
    )
    .await
    .map_err(|_| coordinator_timeout_observation())?
    .map_err(DispatchFailure::PolicyObservation)
}

#[derive(Clone)]
pub struct ShimClient {
    http: Client,
    retry: RetryConfig,
    deadlines: AttemptDeadlines,
    response_body_limits: body::ResponseBodyLimits,
    stream_retention_limits: crate::streaming::StreamRetentionLimits,
    /// Provider health, fed by every dispatch this client makes. `None` means
    /// this client reports to nobody — see [`ShimClient::with_breaker`].
    breaker: Option<Arc<ProviderBreaker>>,
}

impl Default for ShimClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ShimClient {
    pub fn new() -> Self {
        let deadlines = AttemptDeadlines::from_env();
        Self {
            http: build_http_client(deadlines.connect),
            retry: RetryConfig::from_env(),
            deadlines,
            response_body_limits: body::ResponseBodyLimits::default(),
            stream_retention_limits: crate::streaming::StreamRetentionLimits::default(),
            breaker: None,
        }
    }

    pub fn with_attempt_deadlines(
        mut self,
        deadlines: AttemptDeadlines,
    ) -> std::result::Result<Self, &'static str> {
        self.deadlines = deadlines.validate()?;
        self.http = build_http_client(self.deadlines.connect);
        Ok(self)
    }

    pub fn with_stream_retention_limits(
        mut self,
        limits: crate::streaming::StreamRetentionLimits,
    ) -> Result<Self> {
        self.stream_retention_limits = crate::streaming::StreamRetentionLimits::new(
            limits.normalizer_bytes,
            limits.normalizer_entries,
            limits.native_usage_bytes,
            limits.native_usage_entries,
        )?;
        Ok(self)
    }

    /// Report every dispatch's outcome to `breaker`.
    ///
    /// The breaker lives on the [`Router`](crate::router::Router), so a caller
    /// that resolves a provider itself and comes straight here has to hand it
    /// over: `ShimClient::new().with_breaker(router.breaker().clone())`. The
    /// crate's own entry points (`llmshim::completion`, `stream`,
    /// `completion_with_fallback`) bind the router's breaker this way, so this
    /// is the one place a dispatch is counted — whichever door it came in by.
    ///
    /// Cheap: the HTTP connection pool is shared by clone, so binding a breaker
    /// per call costs an `Arc` clone, not a new pool.
    pub fn with_breaker(mut self, breaker: Arc<ProviderBreaker>) -> Self {
        self.breaker = Some(breaker);
        self
    }

    /// Record one dispatch against the attached breaker, if any. Called once
    /// per public entry point on its *final* result — after transport retries
    /// and any output-contract repair — so one caller-visible call is one
    /// observation, never one per attempt.
    ///
    /// Takes the projected outcome rather than the result itself so a stream's
    /// non-`Sync` body is never borrowed across the await.
    pub(crate) async fn observe(
        &self,
        provider: &dyn Provider,
        outcome: std::result::Result<(), &ShimError>,
    ) {
        if let Some(breaker) = &self.breaker {
            breaker.observe(provider.name(), outcome).await;
        }
    }

    pub(crate) fn breaker_outcome<T>(
        result: &DispatchResult<T>,
    ) -> Option<std::result::Result<(), &ShimError>> {
        match result {
            Ok(_) => Some(Ok(())),
            Err(DispatchFailure::Upstream(error)) => Some(Err(error)),
            Err(
                DispatchFailure::Local(_)
                | DispatchFailure::LocalTimeout(_)
                | DispatchFailure::LogicalTimeout(_)
                | DispatchFailure::PolicyRefusal(_)
                | DispatchFailure::PolicyObservation(_),
            ) => None,
        }
    }

    /// Pre-establish TCP+TLS connections to provider endpoints.
    /// Call this after creating the Router to warm the connection pool.
    pub async fn warmup(&self, urls: &[&str]) {
        let futs: Vec<_> = urls
            .iter()
            .map(|url| {
                let client = self.http.clone();
                let url = url.to_string();
                tokio::spawn(async move {
                    // HEAD request — cheapest way to establish a connection
                    let _ = client
                        .head(&url)
                        .timeout(Duration::from_secs(5))
                        .send()
                        .await;
                })
            })
            .collect();
        for f in futs {
            let _ = f.await;
        }
    }

    const RETRYABLE_STATUSES: &'static [u16] = &[429, 500, 502, 503, 504, 529];

    pub async fn send(&self, req: &ProviderRequest) -> Result<reqwest::Response> {
        self.send_prepared(None, AttemptKind::Completion, "", None, req)
            .await
            .map(|attempt_response| attempt_response.response)
            .map_err(DispatchFailure::into_public)
    }

    async fn send_prepared(
        &self,
        policy_context: Option<&DispatchPolicyContext>,
        attempt_kind: AttemptKind,
        resolved_model: &str,
        prepared_target: Option<&ReplayTarget>,
        req: &ProviderRequest,
    ) -> DispatchResult<AttemptResponse> {
        let max_retries = self.retry.max_retries;

        for attempt in 0..=max_retries {
            let mut builder = self.http.post(&req.url);
            for (key, value) in &req.headers {
                builder = builder.header(key, value);
            }
            let http_request = builder
                .json(&req.body)
                .build()
                .map_err(|error| DispatchFailure::Local(error.into()))?;
            let attempt_total = match attempt_kind {
                AttemptKind::Completion => self.deadlines.unary_attempt_total,
                AttemptKind::Stream => self.deadlines.stream_attempt_total,
            };
            let attempt_deadline = deadline_after(attempt_total).ok_or_else(timeout_504)?;
            let mut attempt_tracker = match policy_context {
                Some(context) => {
                    let Some(target) = prepared_target else {
                        return Err(DispatchFailure::Local(ShimError::Stream(
                            "missing prepared dispatch target".into(),
                        )));
                    };
                    let acquire =
                        context.acquire(attempt_kind, resolved_model, target, &req.url, &req.body);
                    Some(
                        match bounded_policy(
                            acquire,
                            self.deadlines.policy_callback,
                            attempt_deadline,
                        )
                        .await
                        {
                            Ok(result) => result.map_err(map_attempt_acquire_error)?,
                            Err(()) => {
                                if context.ensure_logical_active().is_err() {
                                    return Err(logical_timeout_504());
                                }
                                return Err(DispatchFailure::PolicyRefusal(
                                    coordinator_timeout_refusal(),
                                ));
                            }
                        },
                    )
                }
                None => None,
            };

            if let Some(context) = policy_context {
                context
                    .ensure_logical_active()
                    .map_err(|_| logical_timeout_504())?;
            }

            let header_deadline = earlier_deadline(
                attempt_deadline,
                deadline_after(self.deadlines.response_headers).ok_or_else(timeout_504)?,
            );
            let response_result =
                tokio::time::timeout_at(header_deadline, self.http.execute(http_request)).await;
            match response_result {
                Err(_) if attempt < max_retries => {
                    finish_transport_failure(
                        &mut attempt_tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    tokio::time::sleep(backoff_with_jitter(
                        attempt,
                        self.retry.base,
                        self.retry.cap,
                    ))
                    .await;
                    continue;
                }
                Err(_) => {
                    finish_transport_failure(
                        &mut attempt_tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    return Err(timeout_504());
                }
                Ok(Ok(resp)) => {
                    let status = resp.status();
                    if let Some(tracker) = attempt_tracker.as_ref() {
                        bounded_policy(
                            tracker.response_headers(status.as_u16()),
                            self.deadlines.policy_callback,
                            attempt_deadline,
                        )
                        .await
                        .map_err(|_| coordinator_timeout_observation())?
                        .map_err(DispatchFailure::PolicyObservation)?;
                    }
                    if status.is_success() {
                        return Ok(AttemptResponse {
                            response: resp,
                            tracker: attempt_tracker,
                            attempt_deadline,
                        });
                    }
                    let status_code = status.as_u16();
                    if Self::RETRYABLE_STATUSES.contains(&status_code) && attempt < max_retries {
                        // Prefer server-provided timing (Retry-After / provider
                        // reset hints); otherwise fall back to jittered backoff.
                        let wait =
                            retry_after_wait(resp.headers(), self.retry.cap).unwrap_or_else(|| {
                                backoff_with_jitter(attempt, self.retry.base, self.retry.cap)
                            });
                        let error_body_deadline =
                            body_deadline(attempt_deadline, self.deadlines.error_body_total);
                        let error_body = body::read(
                            resp,
                            self.response_body_limits.error_bytes,
                            self.deadlines.error_body_idle,
                            error_body_deadline,
                        )
                        .await;
                        if let (Ok(error_body), Some(target)) =
                            (error_body.as_ref(), prepared_target)
                        {
                            observe_bounded_error_usage(
                                target,
                                error_body,
                                &mut attempt_tracker,
                                self.deadlines.policy_callback,
                                attempt_deadline,
                            )
                            .await?;
                        }
                        if let Some(tracker) = attempt_tracker.as_mut() {
                            let accounting = tracker.accounting(false);
                            let finish = tracker.finish(AttemptOutcome::HttpFailure {
                                status: status_code,
                                accounting,
                            });
                            bounded_policy(
                                finish,
                                self.deadlines.policy_callback,
                                attempt_deadline,
                            )
                            .await
                            .map_err(|_| coordinator_timeout_observation())?
                            .map_err(DispatchFailure::PolicyObservation)?;
                        }
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    // Read before the body is consumed: the header is the
                    // server's own wait, and a caller with its own backoff
                    // above this client gets to honour it too.
                    let retry_after = parse_retry_after(resp.headers());
                    let error_body_deadline =
                        body_deadline(attempt_deadline, self.deadlines.error_body_total);
                    let error_body = body::read_text_and_bytes(
                        resp,
                        self.response_body_limits.error_bytes,
                        self.deadlines.error_body_idle,
                        error_body_deadline,
                    )
                    .await;
                    if let (Ok((_, error_body)), Some(target)) =
                        (error_body.as_ref(), prepared_target)
                    {
                        observe_bounded_error_usage(
                            target,
                            error_body,
                            &mut attempt_tracker,
                            self.deadlines.policy_callback,
                            attempt_deadline,
                        )
                        .await?;
                    }
                    if let Some(tracker) = attempt_tracker.as_mut() {
                        let accounting = tracker.accounting(false);
                        let finish = tracker.finish(AttemptOutcome::HttpFailure {
                            status: status_code,
                            accounting,
                        });
                        bounded_policy(finish, self.deadlines.policy_callback, attempt_deadline)
                            .await
                            .map_err(|_| coordinator_timeout_observation())?
                            .map_err(DispatchFailure::PolicyObservation)?;
                    }
                    let body = match error_body {
                        Ok((error_body_text, _)) => error_body_text,
                        Err(body::BodyReadError::TooLarge) => {
                            return Err(body::BodyReadError::TooLarge.into_dispatch_failure());
                        }
                        Err(body::BodyReadError::Http(_)) => String::new(),
                        Err(body::BodyReadError::Timeout) => "upstream error body timed out".into(),
                        Err(body::BodyReadError::Complexity) => {
                            "upstream error body exceeds JSON complexity limit".into()
                        }
                    };
                    return Err(DispatchFailure::Upstream(ShimError::ProviderError {
                        status: status_code,
                        body,
                        retry_after,
                    }));
                }
                // Transport errors carry no headers: always jittered backoff.
                Ok(Err(e)) if Self::is_retryable_transport(&e) && attempt < max_retries => {
                    finish_transport_failure(
                        &mut attempt_tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    tokio::time::sleep(backoff_with_jitter(
                        attempt,
                        self.retry.base,
                        self.retry.cap,
                    ))
                    .await;
                    continue;
                }
                Ok(Err(error)) => {
                    finish_transport_failure(
                        &mut attempt_tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    return Err(DispatchFailure::Upstream(error.into()));
                }
            }
        }
        unreachable!()
    }

    fn is_retryable_transport(err: &reqwest::Error) -> bool {
        // Retry all transport-level failures: connect, timeout, request build,
        // body read errors, connection reset, incomplete messages, etc.
        err.is_connect() || err.is_timeout() || err.is_request() || err.is_body()
    }

    pub async fn completion(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let result = self
            .completion_dispatch(provider, model, request, None)
            .await;
        if let Some(outcome) = Self::breaker_outcome(&result) {
            self.observe(provider, outcome).await;
        }
        result.map_err(DispatchFailure::into_public)
    }

    pub async fn completion_with_policy(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &serde_json::Value,
        policy_context: &DispatchPolicyContext,
    ) -> Result<serde_json::Value> {
        let result = self
            .completion_dispatch(provider, model, request, Some(policy_context))
            .await;
        if let Some(outcome) = Self::breaker_outcome(&result) {
            self.observe(provider, outcome).await;
        }
        result.map_err(DispatchFailure::into_public)
    }

    pub(crate) async fn completion_dispatch(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &serde_json::Value,
        policy_context: Option<&DispatchPolicyContext>,
    ) -> DispatchResult<serde_json::Value> {
        let plan = crate::shim::Plan::new(
            provider.name(),
            model,
            provider.replay_target(model).wire,
            request,
        )
        .map_err(DispatchFailure::Local)?;
        let mut rendered = plan.render().map_err(DispatchFailure::Local)?;
        rendered["stream"] = serde_json::json!(false);
        let mut usage = serde_json::json!({});
        for attempt in 0..2 {
            let (mut result, target) = self
                .completion_once(provider, model, &rendered, policy_context)
                .await
                .map_err(|error| error.map_upstream(|error| plan.dispatch_error(error)))?;
            crate::shim::add_usage(&mut usage, &result);
            match plan.finish(&mut result, &target) {
                Ok(()) => {
                    if attempt > 0 {
                        result["usage"] = usage;
                    }
                    // Price after the repair path has settled the final usage,
                    // so a repaired answer is costed on both attempts' tokens.
                    crate::cost::stamp(&target.provider, &target.model, &mut result);
                    return Ok(result);
                }
                Err(feedback)
                    if feedback
                        .iter()
                        .any(|error| error == crate::shim::JSON_COMPLEXITY_ERROR) =>
                {
                    return Err(DispatchFailure::Local(ShimError::ProviderError {
                        status: 502,
                        body: "upstream JSON exceeds complexity limit".into(),
                        retry_after: None,
                    }));
                }
                Err(feedback) if attempt == 0 && plan.can_repair(&result) => {
                    rendered = plan.repair(&feedback).map_err(DispatchFailure::Local)?;
                    rendered["stream"] = serde_json::json!(false);
                }
                Err(_) => return Err(DispatchFailure::Local(crate::shim::failed())),
            }
        }
        unreachable!()
    }

    async fn completion_once(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &serde_json::Value,
        policy_context: Option<&DispatchPolicyContext>,
    ) -> DispatchResult<(serde_json::Value, crate::reasoning::ReplayTarget)> {
        let provider_req = provider
            .prepare_request(model, request)
            .await
            .map_err(DispatchFailure::Local)?;
        let target = provider.request_replay_target(model, &provider_req);
        let AttemptResponse {
            response,
            mut tracker,
            attempt_deadline,
        } = self
            .send_prepared(
                policy_context,
                AttemptKind::Completion,
                model,
                Some(&target),
                &provider_req,
            )
            .await?;
        if provider.name() == "chatgpt"
            && target.wire == crate::reasoning::WireFormat::OpenAiResponses
        {
            let collected = crate::providers::chatgpt::collect_response_with_terminal(
                model,
                response,
                self.deadlines.stream_semantic_idle,
                attempt_deadline,
                self.stream_retention_limits,
            )
            .await;
            if let Some(native_usage) = collected.native_usage.as_ref() {
                observe_native_response_usage(
                    &target,
                    native_usage,
                    &mut tracker,
                    self.deadlines.policy_callback,
                    attempt_deadline,
                )
                .await?;
            }
            let native_response = match collected.result {
                Ok(result) => result,
                Err(error) => {
                    finish_invalid_response(
                        &mut tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    return Err(
                        if matches!(
                            &error,
                            ShimError::ProviderError { status: 504, body, .. }
                                if body == "upstream response body timed out"
                        ) {
                            DispatchFailure::LocalTimeout(error)
                        } else {
                            DispatchFailure::Upstream(error)
                        },
                    );
                }
            };
            let mut result = match crate::providers::chatgpt::transform_collected_response(
                &target,
                model,
                native_response,
            ) {
                Ok(result) => result,
                Err(error) => {
                    finish_invalid_response(
                        &mut tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    return Err(DispatchFailure::Upstream(error));
                }
            };
            if let Err(error) = crate::derived_response::bind_unary_context(&mut result, &target) {
                finish_invalid_response(
                    &mut tracker,
                    self.deadlines.policy_callback,
                    attempt_deadline,
                )
                .await?;
                return Err(DispatchFailure::Upstream(error));
            }
            finish_completed_response(
                &mut tracker,
                self.deadlines.policy_callback,
                attempt_deadline,
            )
            .await?;
            return Ok((result, target));
        }
        let body = match body::read_json(
            response,
            self.response_body_limits.success_bytes,
            self.deadlines.unary_body_idle,
            attempt_deadline,
        )
        .await
        {
            Ok(body) => body,
            Err(error) => {
                finish_invalid_response(
                    &mut tracker,
                    self.deadlines.policy_callback,
                    attempt_deadline,
                )
                .await?;
                return Err(error.into_dispatch_failure());
            }
        };
        observe_native_response_usage(
            &target,
            &body,
            &mut tracker,
            self.deadlines.policy_callback,
            attempt_deadline,
        )
        .await?;
        let mut result = match provider.transform_response(model, body) {
            Ok(result) => result,
            Err(error) => {
                finish_invalid_response(
                    &mut tracker,
                    self.deadlines.policy_callback,
                    attempt_deadline,
                )
                .await?;
                return Err(DispatchFailure::Upstream(error));
            }
        };
        if let Err(error) = crate::derived_response::bind_unary_context(&mut result, &target) {
            finish_invalid_response(
                &mut tracker,
                self.deadlines.policy_callback,
                attempt_deadline,
            )
            .await?;
            return Err(DispatchFailure::Upstream(error));
        }
        finish_completed_response(
            &mut tracker,
            self.deadlines.policy_callback,
            attempt_deadline,
        )
        .await?;
        Ok((result, target))
    }

    pub async fn stream(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &serde_json::Value,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        // A stream's health verdict is whether it opened; per-chunk failures
        // are the transport's business, not the breaker's.
        let opened = self.stream_dispatch(provider, model, request, None).await;
        if let Some(outcome) = Self::breaker_outcome(&opened) {
            self.observe(provider, outcome).await;
        }
        opened.map_err(DispatchFailure::into_public)
    }

    pub async fn stream_with_policy(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &serde_json::Value,
        policy_context: &DispatchPolicyContext,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let opened = self
            .stream_dispatch(provider, model, request, Some(policy_context))
            .await;
        if let Some(outcome) = Self::breaker_outcome(&opened) {
            self.observe(provider, outcome).await;
        }
        opened.map_err(DispatchFailure::into_public)
    }

    async fn stream_dispatch(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &serde_json::Value,
        policy_context: Option<&DispatchPolicyContext>,
    ) -> DispatchResult<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let plan = crate::shim::Plan::new(
            provider.name(),
            model,
            provider.replay_target(model).wire,
            request,
        )
        .map_err(DispatchFailure::Local)?;
        let mut rendered = plan.render().map_err(DispatchFailure::Local)?;
        if !plan.buffered() {
            return self
                .stream_once(provider, model, &rendered, policy_context)
                .await
                .map(|(stream, _)| stream.stream)
                .map_err(|error| error.map_upstream(|error| plan.dispatch_error(error)));
        }
        let mut usage = serde_json::json!({});
        for attempt in 0..2 {
            let (stream_dispatch, target) = self
                .stream_once(provider, model, &rendered, policy_context)
                .await
                .map_err(|error| error.map_upstream(|error| plan.dispatch_error(error)))?;
            let mut result = collect_stream_dispatch(stream_dispatch)
                .await
                .map_err(|error| error.map_upstream(|error| plan.dispatch_error(error)))?;
            crate::shim::add_usage(&mut usage, &result);
            match plan.finish(&mut result, &target) {
                Ok(()) => {
                    if attempt > 0 {
                        result["usage"] = usage;
                    }
                    crate::cost::stamp(&target.provider, &target.model, &mut result);
                    return Ok(Box::pin(futures::stream::iter(crate::shim::chunks(result))));
                }
                Err(feedback)
                    if feedback
                        .iter()
                        .any(|error| error == crate::shim::JSON_COMPLEXITY_ERROR) =>
                {
                    return Err(DispatchFailure::Local(ShimError::ProviderError {
                        status: 502,
                        body: "upstream JSON exceeds complexity limit".into(),
                        retry_after: None,
                    }));
                }
                Err(feedback) if attempt == 0 && plan.can_repair(&result) => {
                    rendered = plan.repair(&feedback).map_err(DispatchFailure::Local)?
                }
                Err(_) => return Err(DispatchFailure::Local(crate::shim::failed())),
            }
        }
        unreachable!()
    }

    /// Owned provider variant opens the first response before returning, then
    /// buffers managed output inside the stream. HTTP frontends can send headers
    /// and keepalives while validation and a possible repair are in progress.
    pub async fn stream_owned(
        &self,
        provider: Arc<dyn Provider>,
        model: &str,
        request: &serde_json::Value,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        // Observed on the open only. A buffered plan's repair re-opens inside
        // the returned stream; that second dial is not a separate verdict.
        let opened = self
            .stream_owned_dispatch(provider.clone(), model, request, None)
            .await;
        if let Some(outcome) = Self::breaker_outcome(&opened) {
            self.observe(provider.as_ref(), outcome).await;
        }
        opened.map_err(DispatchFailure::into_public)
    }

    pub async fn stream_owned_with_policy(
        &self,
        provider: Arc<dyn Provider>,
        model: &str,
        request: &serde_json::Value,
        policy_context: &DispatchPolicyContext,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let opened = self
            .stream_owned_dispatch(
                provider.clone(),
                model,
                request,
                Some(policy_context.clone()),
            )
            .await;
        if let Some(outcome) = Self::breaker_outcome(&opened) {
            self.observe(provider.as_ref(), outcome).await;
        }
        opened.map_err(DispatchFailure::into_public)
    }

    async fn stream_owned_dispatch(
        &self,
        provider: Arc<dyn Provider>,
        model: &str,
        request: &serde_json::Value,
        policy_context: Option<DispatchPolicyContext>,
    ) -> DispatchResult<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let plan = crate::shim::Plan::new(
            provider.name(),
            model,
            provider.replay_target(model).wire,
            request,
        )
        .map_err(DispatchFailure::Local)?;
        let rendered = plan.render().map_err(DispatchFailure::Local)?;
        let (first_dispatch, target) = self
            .stream_once(provider.as_ref(), model, &rendered, policy_context.as_ref())
            .await
            .map_err(|error| error.map_upstream(|error| plan.dispatch_error(error)))?;
        if !plan.buffered() {
            return Ok(first_dispatch.stream);
        }
        let client = self.clone();
        let model = model.to_owned();
        Ok(Box::pin(futures::stream::once(async move {
            let mut effective_target = target.clone();
            let mut result = collect_stream_dispatch(first_dispatch)
                .await
                .map_err(|error| match error {
                    DispatchFailure::Upstream(error) => plan.dispatch_error(error),
                    other => other.into_public(),
                })?;
            let mut usage = serde_json::json!({});
            crate::shim::add_usage(&mut usage, &result);
            if let Err(feedback) = plan.finish(&mut result, &target) {
                if feedback
                    .iter()
                    .any(|error| error == crate::shim::JSON_COMPLEXITY_ERROR)
                {
                    return Err(ShimError::ProviderError {
                        status: 502,
                        body: "upstream JSON exceeds complexity limit".into(),
                        retry_after: None,
                    });
                }
                if !plan.can_repair(&result) {
                    return Err(crate::shim::failed());
                }
                let rendered = plan.repair(&feedback)?;
                let (second_dispatch, second_target) = client
                    .stream_once(
                        provider.as_ref(),
                        &model,
                        &rendered,
                        policy_context.as_ref(),
                    )
                    .await
                    .map_err(|error| match error {
                        DispatchFailure::Upstream(error) => plan.dispatch_error(error),
                        other => other.into_public(),
                    })?;
                result = collect_stream_dispatch(second_dispatch).await.map_err(
                    |error| match error {
                        DispatchFailure::Upstream(error) => plan.dispatch_error(error),
                        other => other.into_public(),
                    },
                )?;
                crate::shim::add_usage(&mut usage, &result);
                plan.finish(&mut result, &second_target)
                    .map_err(|_| crate::shim::failed())?;
                effective_target = second_target;
                result["usage"] = usage;
            }
            crate::cost::stamp(
                &effective_target.provider,
                &effective_target.model,
                &mut result,
            );
            crate::shim::chunks(result).pop().unwrap()
        })))
    }

    async fn stream_once(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &serde_json::Value,
        policy_context: Option<&DispatchPolicyContext>,
    ) -> DispatchResult<(StreamDispatch, crate::reasoning::ReplayTarget)> {
        let mut req_value = request.clone();
        req_value["stream"] = serde_json::Value::Bool(true);

        let provider_req = provider
            .prepare_request(model, &req_value)
            .await
            .map_err(DispatchFailure::Local)?;
        let target = provider.request_replay_target(model, &provider_req);
        let AttemptResponse {
            response,
            tracker,
            attempt_deadline,
        } = self
            .send_prepared(
                policy_context,
                AttemptKind::Stream,
                model,
                Some(&target),
                &provider_req,
            )
            .await?;
        let policy_failure = Arc::new(std::sync::Mutex::new(None));
        let stream = eager_stream(
            response,
            tracker,
            target.clone(),
            policy_failure.clone(),
            self.deadlines,
            attempt_deadline,
            self.stream_retention_limits,
        );

        Ok((
            StreamDispatch {
                stream,
                policy_failure,
            },
            target,
        ))
    }
}

struct AttemptResponse {
    response: reqwest::Response,
    tracker: Option<AttemptTracker>,
    attempt_deadline: tokio::time::Instant,
}

struct StreamDispatch {
    stream: Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
    policy_failure: Arc<std::sync::Mutex<Option<AttemptPolicyError>>>,
}

async fn observe_native_response_usage(
    target: &ReplayTarget,
    native_response: &serde_json::Value,
    tracker: &mut Option<AttemptTracker>,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
) -> DispatchResult<()> {
    let Some(tracker) = tracker.as_mut() else {
        return Ok(());
    };
    if let Some(mut observation) =
        crate::usage::normalize_native_response_usage_observation(target, native_response)
    {
        stamp_usage(target, &mut observation.usage);
        let usage = tracker.usage(
            &observation.usage,
            observation.terminal,
            observation.counters_complete,
            observation.explicit_zero,
        );
        bounded_policy(usage, callback_timeout, attempt_deadline)
            .await
            .map_err(|_| coordinator_timeout_observation())?
            .map_err(DispatchFailure::PolicyObservation)?;
    }
    Ok(())
}

async fn observe_bounded_error_usage(
    target: &ReplayTarget,
    bounded_body: &[u8],
    tracker: &mut Option<AttemptTracker>,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
) -> DispatchResult<()> {
    let Ok(native_error) =
        crate::json_bounds::parse_slice(bounded_body, crate::json_bounds::Limits::UNARY)
    else {
        return Ok(());
    };
    observe_native_response_usage(
        target,
        &native_error,
        tracker,
        callback_timeout,
        attempt_deadline,
    )
    .await
}

async fn finish_completed_response(
    tracker: &mut Option<AttemptTracker>,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
) -> DispatchResult<()> {
    let Some(tracker) = tracker.as_mut() else {
        return Ok(());
    };
    let accounting = tracker.accounting(true);
    bounded_policy(
        tracker.finish(AttemptOutcome::Completed { accounting }),
        callback_timeout,
        attempt_deadline,
    )
    .await
    .map_err(|_| coordinator_timeout_observation())?
    .map_err(DispatchFailure::PolicyObservation)
}

async fn finish_invalid_response(
    tracker: &mut Option<AttemptTracker>,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
) -> DispatchResult<()> {
    let Some(tracker) = tracker.as_mut() else {
        return Ok(());
    };
    let accounting = tracker.accounting(false);
    bounded_policy(
        tracker.finish(AttemptOutcome::InvalidResponse { accounting }),
        callback_timeout,
        attempt_deadline,
    )
    .await
    .map_err(|_| coordinator_timeout_observation())?
    .map_err(DispatchFailure::PolicyObservation)
}

fn stamp_usage(target: &ReplayTarget, usage: &mut serde_json::Value) {
    let mut response = serde_json::json!({"usage": usage.take()});
    crate::cost::stamp(&target.provider, &target.model, &mut response);
    *usage = response["usage"].take();
}

fn eager_stream(
    response: reqwest::Response,
    tracker: Option<AttemptTracker>,
    target: ReplayTarget,
    policy_failure: Arc<std::sync::Mutex<Option<AttemptPolicyError>>>,
    deadlines: AttemptDeadlines,
    attempt_deadline: tokio::time::Instant,
    retention_limits: crate::streaming::StreamRetentionLimits,
) -> Pin<Box<dyn Stream<Item = Result<String>> + Send>> {
    let cancellation_guard = tracker.as_ref().map(AttemptTracker::cancellation_guard);
    let (output_sender, output_receiver) = tokio::sync::mpsc::channel(1);
    let (cancellation_sender, cancellation_receiver) = tokio::sync::watch::channel(false);
    let terminal = Arc::new(std::sync::Mutex::new(None));
    let producer_terminal = terminal.clone();
    tokio::spawn(async move {
        run_eager_stream_producer(
            response,
            tracker,
            target,
            policy_failure,
            deadlines,
            attempt_deadline,
            output_sender,
            producer_terminal,
            cancellation_receiver,
            retention_limits,
        )
        .await;
    });
    Box::pin(EagerReceiverStream {
        receiver: output_receiver,
        terminal,
        terminal_emitted: false,
        cancellation_sender,
        _cancellation_guard: cancellation_guard,
    })
}

struct EagerReceiverStream {
    receiver: tokio::sync::mpsc::Receiver<Result<String>>,
    terminal: Arc<std::sync::Mutex<Option<Result<String>>>>,
    terminal_emitted: bool,
    cancellation_sender: tokio::sync::watch::Sender<bool>,
    _cancellation_guard: Option<crate::policy::AttemptCancellationGuard>,
}

impl Stream for EagerReceiverStream {
    type Item = Result<String>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.receiver.poll_recv(context) {
            std::task::Poll::Ready(Some(item)) => std::task::Poll::Ready(Some(item)),
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(None) if !self.terminal_emitted => {
                self.terminal_emitted = true;
                std::task::Poll::Ready(
                    self.terminal
                        .lock()
                        .ok()
                        .and_then(|mut terminal| terminal.take()),
                )
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
        }
    }
}

impl Drop for EagerReceiverStream {
    fn drop(&mut self) {
        let _ = self.cancellation_sender.send(true);
        self.receiver.close();
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_eager_stream_producer(
    response: reqwest::Response,
    mut tracker: Option<AttemptTracker>,
    target: ReplayTarget,
    policy_failure: Arc<std::sync::Mutex<Option<AttemptPolicyError>>>,
    deadlines: AttemptDeadlines,
    attempt_deadline: tokio::time::Instant,
    output_sender: tokio::sync::mpsc::Sender<Result<String>>,
    terminal: Arc<std::sync::Mutex<Option<Result<String>>>>,
    mut cancellation: tokio::sync::watch::Receiver<bool>,
    retention_limits: crate::streaming::StreamRetentionLimits,
) {
    let mut source = native_events(response.bytes_stream());
    let mut normalizer = match crate::streaming::StreamNormalizer::with_retention_limits(
        target.clone(),
        retention_limits,
    ) {
        Ok(normalizer) => normalizer,
        Err(error) => {
            set_stream_terminal(&terminal, Err(error));
            return;
        }
    };
    let mut native_usage =
        crate::usage::NativeStreamUsage::with_limits(target.clone(), retention_limits);
    let mut semantic_idle_remaining = deadlines.stream_semantic_idle;

    loop {
        let source_poll_started = tokio::time::Instant::now();
        let Some(semantic_idle_deadline) = source_poll_started.checked_add(semantic_idle_remaining)
        else {
            set_stream_terminal(&terminal, Err(stream_timeout_error()));
            return;
        };
        let next = tokio::select! {
            _ = cancellation.changed() => return,
            _ = output_sender.closed() => return,
            _ = tokio::time::sleep_until(attempt_deadline) => {
                settle_stream_timeout(
                    &mut tracker,
                    deadlines.policy_callback,
                    attempt_deadline,
                    &mut cancellation,
                ).await;
                set_stream_terminal(&terminal, Err(stream_timeout_error()));
                return;
            }
            _ = tokio::time::sleep_until(semantic_idle_deadline) => {
                settle_stream_timeout(
                    &mut tracker,
                    deadlines.policy_callback,
                    attempt_deadline,
                    &mut cancellation,
                ).await;
                set_stream_terminal(&terminal, Err(stream_timeout_error()));
                return;
            }
            next = source.next() => next,
        };
        semantic_idle_remaining = semantic_idle_remaining.saturating_sub(
            tokio::time::Instant::now().saturating_duration_since(source_poll_started),
        );

        let Some(next) = next else {
            let normalized_terminal = normalizer.finish();
            if let Some(observation) = native_usage.take_terminal_candidate() {
                if observe_stream_usage(
                    &mut tracker,
                    &target,
                    observation,
                    deadlines.policy_callback,
                    attempt_deadline,
                    &policy_failure,
                    &mut cancellation,
                )
                .await
                .is_err()
                {
                    set_stream_terminal(&terminal, Err(coordinator_stream_error()));
                    return;
                }
            }
            match normalized_terminal {
                Ok(Some(chunk)) => {
                    let chunk = crate::cost::stamp_chunk(&target.provider, &target.model, chunk);
                    match send_stream_output(
                        &output_sender,
                        Ok(chunk),
                        attempt_deadline,
                        &mut cancellation,
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(StreamSendFailure::ReceiverDropped) => return,
                        Err(StreamSendFailure::Deadline) => {
                            settle_stream_timeout(
                                &mut tracker,
                                deadlines.policy_callback,
                                attempt_deadline,
                                &mut cancellation,
                            )
                            .await;
                            set_stream_terminal(&terminal, Err(stream_timeout_error()));
                            return;
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    finish_stream_failure(
                        &mut tracker,
                        deadlines.policy_callback,
                        attempt_deadline,
                        &policy_failure,
                        &mut cancellation,
                    )
                    .await;
                    set_stream_terminal(&terminal, Err(error));
                    return;
                }
            }
            if finish_stream_completed(
                &mut tracker,
                deadlines.policy_callback,
                attempt_deadline,
                &policy_failure,
                &mut cancellation,
            )
            .await
            .is_err()
            {
                set_stream_terminal(&terminal, Err(coordinator_stream_error()));
            }
            return;
        };

        let data = match next {
            Ok(data) => data,
            Err(error) => {
                normalizer.abort();
                finish_stream_failure(
                    &mut tracker,
                    deadlines.policy_callback,
                    attempt_deadline,
                    &policy_failure,
                    &mut cancellation,
                )
                .await;
                set_stream_terminal(&terminal, Err(error));
                return;
            }
        };
        if data.trim().is_empty() {
            continue;
        }
        semantic_idle_remaining = deadlines.stream_semantic_idle;
        let observation = match native_usage.ingest_bounded(&data) {
            Ok(observation) => observation,
            Err(error) => {
                normalizer.abort();
                finish_stream_failure(
                    &mut tracker,
                    deadlines.policy_callback,
                    attempt_deadline,
                    &policy_failure,
                    &mut cancellation,
                )
                .await;
                set_stream_terminal(&terminal, Err(error));
                return;
            }
        };
        let normalized = normalizer.push(&data);
        if let Some(observation) = observation {
            if observe_stream_usage(
                &mut tracker,
                &target,
                observation,
                deadlines.policy_callback,
                attempt_deadline,
                &policy_failure,
                &mut cancellation,
            )
            .await
            .is_err()
            {
                set_stream_terminal(&terminal, Err(coordinator_stream_error()));
                return;
            }
        }
        if let Some(error) = native_usage.take_retention_error() {
            normalizer.abort();
            finish_stream_failure(
                &mut tracker,
                deadlines.policy_callback,
                attempt_deadline,
                &policy_failure,
                &mut cancellation,
            )
            .await;
            set_stream_terminal(&terminal, Err(error));
            return;
        }
        match normalized {
            Ok(Some(chunk)) => {
                let chunk = crate::cost::stamp_chunk(&target.provider, &target.model, chunk);
                match send_stream_output(
                    &output_sender,
                    Ok(chunk),
                    attempt_deadline,
                    &mut cancellation,
                )
                .await
                {
                    Ok(()) => {}
                    Err(StreamSendFailure::ReceiverDropped) => return,
                    Err(StreamSendFailure::Deadline) => {
                        settle_stream_timeout(
                            &mut tracker,
                            deadlines.policy_callback,
                            attempt_deadline,
                            &mut cancellation,
                        )
                        .await;
                        set_stream_terminal(&terminal, Err(stream_timeout_error()));
                        return;
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                normalizer.abort();
                finish_stream_failure(
                    &mut tracker,
                    deadlines.policy_callback,
                    attempt_deadline,
                    &policy_failure,
                    &mut cancellation,
                )
                .await;
                set_stream_terminal(&terminal, Err(error));
                return;
            }
        }
        if normalizer.is_finished() {
            if let Some(observation) = native_usage.take_terminal_candidate() {
                if observe_stream_usage(
                    &mut tracker,
                    &target,
                    observation,
                    deadlines.policy_callback,
                    attempt_deadline,
                    &policy_failure,
                    &mut cancellation,
                )
                .await
                .is_err()
                {
                    set_stream_terminal(&terminal, Err(coordinator_stream_error()));
                    return;
                }
            }
            if finish_stream_completed(
                &mut tracker,
                deadlines.policy_callback,
                attempt_deadline,
                &policy_failure,
                &mut cancellation,
            )
            .await
            .is_err()
            {
                set_stream_terminal(&terminal, Err(coordinator_stream_error()));
            }
            return;
        }
    }
}

async fn send_stream_output(
    sender: &tokio::sync::mpsc::Sender<Result<String>>,
    item: Result<String>,
    attempt_deadline: tokio::time::Instant,
    cancellation: &mut tokio::sync::watch::Receiver<bool>,
) -> std::result::Result<(), StreamSendFailure> {
    tokio::select! {
        _ = cancellation.changed() => Err(StreamSendFailure::ReceiverDropped),
        result = sender.send(item) => result.map_err(|_| StreamSendFailure::ReceiverDropped),
        _ = tokio::time::sleep_until(attempt_deadline) => Err(StreamSendFailure::Deadline),
    }
}

enum StreamSendFailure {
    ReceiverDropped,
    Deadline,
}

enum StreamPolicyWaitError {
    Policy,
    ReceiverDropped,
}

async fn await_stream_policy<T>(
    future: impl std::future::Future<Output = T>,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
    cancellation: &mut tokio::sync::watch::Receiver<bool>,
) -> std::result::Result<T, StreamPolicyWaitError> {
    tokio::select! {
        _ = cancellation.changed() => Err(StreamPolicyWaitError::ReceiverDropped),
        result = bounded_policy(future, callback_timeout, attempt_deadline) => {
            result.map_err(|_| StreamPolicyWaitError::Policy)
        }
    }
}

fn set_stream_terminal(
    terminal: &Arc<std::sync::Mutex<Option<Result<String>>>>,
    item: Result<String>,
) {
    if let Ok(mut slot) = terminal.lock() {
        *slot = Some(item);
    }
}

fn stream_timeout_error() -> ShimError {
    ShimError::ProviderError {
        status: 504,
        body: "upstream stream timed out".into(),
        retry_after: None,
    }
}

fn coordinator_stream_error() -> ShimError {
    AttemptPolicyError::new(crate::policy::AttemptPolicyErrorKind::CoordinatorUnavailable)
        .into_shim_error()
}

async fn observe_stream_usage(
    tracker: &mut Option<AttemptTracker>,
    target: &ReplayTarget,
    mut observation: crate::usage::NativeUsageObservation,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
    policy_failure: &Arc<std::sync::Mutex<Option<AttemptPolicyError>>>,
    cancellation: &mut tokio::sync::watch::Receiver<bool>,
) -> std::result::Result<(), StreamPolicyWaitError> {
    let Some(tracker) = tracker.as_mut() else {
        return Ok(());
    };
    stamp_usage(target, &mut observation.usage);
    match await_stream_policy(
        tracker.usage(
            &observation.usage,
            observation.terminal,
            observation.counters_complete,
            observation.explicit_zero,
        ),
        callback_timeout,
        attempt_deadline,
        cancellation,
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            record_policy_failure(policy_failure, error);
            Err(StreamPolicyWaitError::Policy)
        }
        Err(StreamPolicyWaitError::Policy) => {
            record_policy_failure(
                policy_failure,
                AttemptPolicyError::new(
                    crate::policy::AttemptPolicyErrorKind::CoordinatorUnavailable,
                ),
            );
            Err(StreamPolicyWaitError::Policy)
        }
        Err(StreamPolicyWaitError::ReceiverDropped) => Err(StreamPolicyWaitError::ReceiverDropped),
    }
}

async fn finish_stream_completed(
    tracker: &mut Option<AttemptTracker>,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
    policy_failure: &Arc<std::sync::Mutex<Option<AttemptPolicyError>>>,
    cancellation: &mut tokio::sync::watch::Receiver<bool>,
) -> std::result::Result<(), StreamPolicyWaitError> {
    let Some(tracker) = tracker.as_mut() else {
        return Ok(());
    };
    let accounting = tracker.accounting(true);
    match await_stream_policy(
        tracker.finish(AttemptOutcome::Completed { accounting }),
        callback_timeout,
        attempt_deadline,
        cancellation,
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            record_policy_failure(policy_failure, error);
            Err(StreamPolicyWaitError::Policy)
        }
        Err(StreamPolicyWaitError::Policy) => {
            record_policy_failure(
                policy_failure,
                AttemptPolicyError::new(
                    crate::policy::AttemptPolicyErrorKind::CoordinatorUnavailable,
                ),
            );
            Err(StreamPolicyWaitError::Policy)
        }
        Err(StreamPolicyWaitError::ReceiverDropped) => Err(StreamPolicyWaitError::ReceiverDropped),
    }
}

async fn finish_stream_failure(
    tracker: &mut Option<AttemptTracker>,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
    policy_failure: &Arc<std::sync::Mutex<Option<AttemptPolicyError>>>,
    cancellation: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    let Some(tracker) = tracker.as_mut() else {
        return false;
    };
    let accounting = tracker.accounting(false);
    let result = await_stream_policy(
        tracker.finish(AttemptOutcome::StreamFailure { accounting }),
        callback_timeout,
        attempt_deadline,
        cancellation,
    )
    .await;
    match result {
        Ok(Ok(())) => false,
        Ok(Err(error)) => {
            record_policy_failure(policy_failure, error);
            false
        }
        Err(StreamPolicyWaitError::Policy) => {
            record_policy_failure(
                policy_failure,
                AttemptPolicyError::new(
                    crate::policy::AttemptPolicyErrorKind::CoordinatorUnavailable,
                ),
            );
            false
        }
        Err(StreamPolicyWaitError::ReceiverDropped) => true,
    }
}

async fn settle_stream_timeout(
    tracker: &mut Option<AttemptTracker>,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
    cancellation: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    let Some(tracker) = tracker.as_mut() else {
        return false;
    };
    let accounting = tracker.accounting(false);
    matches!(
        await_stream_policy(
            tracker.finish(AttemptOutcome::StreamFailure { accounting }),
            callback_timeout,
            attempt_deadline,
            cancellation,
        )
        .await,
        Err(StreamPolicyWaitError::ReceiverDropped)
    )
}

async fn collect_stream_dispatch(dispatch: StreamDispatch) -> DispatchResult<serde_json::Value> {
    match crate::shim::collect(dispatch.stream).await {
        Ok(result) => Ok(result),
        Err(error) => match dispatch.policy_failure.lock() {
            Ok(mut failure) => match failure.take() {
                Some(policy_error) => Err(DispatchFailure::PolicyObservation(policy_error)),
                None => Err(DispatchFailure::Upstream(error)),
            },
            Err(_) => Err(DispatchFailure::PolicyObservation(AttemptPolicyError::new(
                crate::policy::AttemptPolicyErrorKind::Other,
            ))),
        },
    }
}

fn record_policy_failure(
    destination: &Arc<std::sync::Mutex<Option<AttemptPolicyError>>>,
    error: AttemptPolicyError,
) {
    if let Ok(mut destination) = destination.lock() {
        *destination = Some(error);
    }
}

// ---------------------------------------------------------------------------
// Retry timing (reactive layer)
//
// Pure, network-free helpers so timing logic is unit-testable with no HTTP.
// ---------------------------------------------------------------------------

/// Compute how long to wait before retrying a retryable *response*, using
/// server-provided timing when available. Returns `None` when the server gives
/// no usable hint (caller then falls back to jittered backoff).
///
/// Priority: `Retry-After` header, then provider reset hints. The result is
/// clamped to `cap` and nudged with a little jitter so a fleet of clients
/// handed the same reset time don't retry in lockstep (thundering herd).
fn retry_after_wait(headers: &HeaderMap, cap: Duration) -> Option<Duration> {
    let base = parse_retry_after(headers).or_else(|| parse_provider_reset(headers))?;
    let capped = base.min(cap);
    Some(capped + small_jitter())
}

/// Parse the `Retry-After` header (RFC 7231): either an integer number of
/// seconds, or an HTTP-date. Returns `None` when absent or unparseable.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    parse_retry_after_at(headers, Utc::now())
}

fn parse_retry_after_at(headers: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    let raw = header_str(headers, "retry-after")?.trim();
    // Form 1: delay in whole seconds.
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // Form 2: HTTP-date (RFC 7231, e.g. "Wed, 21 Oct 2015 07:28:00 GMT").
    let when = DateTime::parse_from_rfc2822(raw).ok()?.with_timezone(&Utc);
    duration_until(when, now)
}

/// Best-effort provider-specific reset hints, used only when `Retry-After` is
/// absent. Takes the most conservative (largest) positive hint present.
///
/// - OpenAI: `x-ratelimit-reset-tokens` / `x-ratelimit-reset-requests`, which
///   are Go-duration-ish strings like `"1s"`, `"6m0s"`, `"100ms"`.
/// - Anthropic: any `anthropic-ratelimit-*-reset` header (RFC3339 timestamp).
fn parse_provider_reset(headers: &HeaderMap) -> Option<Duration> {
    parse_provider_reset_at(headers, Utc::now())
}

fn parse_provider_reset_at(headers: &HeaderMap, now: DateTime<Utc>) -> Option<Duration> {
    let mut best: Option<Duration> = None;
    let mut consider = |d: Option<Duration>| {
        if let Some(d) = d {
            best = Some(best.map_or(d, |b| b.max(d)));
        }
    };

    // OpenAI Go-duration reset hints.
    for name in ["x-ratelimit-reset-tokens", "x-ratelimit-reset-requests"] {
        if let Some(v) = header_str(headers, name) {
            consider(parse_go_duration(v));
        }
    }

    // Anthropic RFC3339 reset timestamps (header names vary by resource, e.g.
    // anthropic-ratelimit-requests-reset, -tokens-reset, -input-tokens-reset).
    for (name, value) in headers.iter() {
        let name = name.as_str();
        if name.starts_with("anthropic-ratelimit-") && name.ends_with("-reset") {
            if let Ok(v) = value.to_str() {
                if let Ok(when) = DateTime::parse_from_rfc3339(v.trim()) {
                    consider(duration_until(when.with_timezone(&Utc), now));
                }
            }
        }
    }

    best
}

/// Parse a Go-style duration string (`"1s"`, `"100ms"`, `"6m0s"`, `"1h2m3s"`).
/// Supports `h`, `m`, `s`, `ms`, `us`/`µs`, `ns` units. Returns `None` on any
/// unrecognized input.
fn parse_go_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut total = Duration::ZERO;
    let mut saw_unit = false;

    while i < bytes.len() {
        // Numeric part (integer or decimal).
        let num_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if i == num_start {
            return None; // expected a number
        }
        let value: f64 = s[num_start..i].parse().ok()?;

        // Unit part.
        let unit_start = i;
        while i < bytes.len() && !(bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        let unit = &s[unit_start..i];
        let parsed_duration_component = parse_go_duration_component(value, unit)?;
        total = total.saturating_add(parsed_duration_component);
        saw_unit = true;
    }

    saw_unit.then_some(total)
}

fn parse_go_duration_component(value: f64, unit: &str) -> Option<Duration> {
    if !value.is_finite() || value.is_sign_negative() {
        return None;
    }

    let seconds_per_unit = match unit {
        "h" => 3600.0,
        "m" => 60.0,
        "s" => 1.0,
        "ms" => 1.0 / 1_000.0,
        "us" | "µs" | "μs" => 1.0 / 1_000_000.0,
        "ns" => 1.0 / 1_000_000_000.0,
        _ => return None,
    };

    let maximum_duration_seconds = Duration::MAX.as_secs_f64();
    if value > maximum_duration_seconds / seconds_per_unit {
        return Some(Duration::MAX);
    }

    let seconds = value * seconds_per_unit;
    Some(Duration::try_from_secs_f64(seconds).unwrap_or(Duration::MAX))
}

/// Positive duration from `now` until `when`; `None`/zero if `when` is in the past.
fn duration_until(when: DateTime<Utc>, now: DateTime<Utc>) -> Option<Duration> {
    (when - now).to_std().ok()
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// Exponential backoff ceiling for `attempt`: `min(cap, base * 2^attempt)`.
fn backoff_bound(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let mult = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let ms = (base.as_millis() as u64).saturating_mul(mult);
    Duration::from_millis(ms).min(cap)
}

/// Full jitter: a uniform random point in `[0, bound]` given a random source.
/// Pure and deterministic for a fixed `rand` — the randomness is injected.
fn full_jitter(bound: Duration, rand: u64) -> Duration {
    let ms = bound.as_millis() as u64;
    if ms == 0 {
        return Duration::ZERO;
    }
    Duration::from_millis(rand % (ms + 1))
}

/// Full-jitter exponential backoff: uniform in `[0, min(cap, base·2^attempt)]`.
fn backoff_with_jitter(attempt: u32, base: Duration, cap: Duration) -> Duration {
    full_jitter(backoff_bound(attempt, base, cap), rand_u64())
}

/// A little jitter (0–250ms) added to server-dictated waits so a fleet handed
/// the same reset time spreads its retries instead of firing simultaneously.
fn small_jitter() -> Duration {
    Duration::from_millis(rand_u64() % 251)
}

/// Cheap non-cryptographic randomness derived from the clock — good enough for
/// retry jitter and keeps the dependency footprint at zero. SplitMix64 finalizer
/// over the current nanoseconds.
fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn native_events(
    stream: impl Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
) -> Pin<Box<dyn Stream<Item = Result<String>> + Send>> {
    Box::pin(crate::sse::data(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use reqwest::header::{HeaderMap, HeaderValue};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Copy, Debug)]
    enum PendingStreamCallback {
        Usage,
        Finish,
    }

    struct PendingStreamPolicy {
        pending: PendingStreamCallback,
        started: tokio::sync::Notify,
        abandoned: std::sync::atomic::AtomicBool,
    }

    impl crate::policy::AttemptPolicy for PendingStreamPolicy {
        fn acquire<'a>(
            &'a self,
            _attempt: &'a crate::policy::PreparedAttempt<'a>,
        ) -> crate::policy::AttemptPolicyFuture<
            'a,
            std::result::Result<(), crate::policy::AttemptPolicyRefusal>,
        > {
            Box::pin(async { Ok(()) })
        }

        fn observe<'a>(
            &'a self,
            _attempt: &'a crate::policy::AttemptIdentity,
            event: crate::policy::AttemptEvent<'a>,
        ) -> crate::policy::AttemptPolicyFuture<
            'a,
            std::result::Result<(), crate::policy::AttemptPolicyError>,
        > {
            let pending = matches!(
                (self.pending, event),
                (
                    PendingStreamCallback::Usage,
                    crate::policy::AttemptEvent::Usage { .. }
                ) | (
                    PendingStreamCallback::Finish,
                    crate::policy::AttemptEvent::Finished(_)
                )
            );
            Box::pin(async move {
                if pending {
                    self.started.notify_one();
                    futures::future::pending().await
                } else {
                    Ok(())
                }
            })
        }

        fn observe_abandoned(
            &self,
            _attempt: &crate::policy::AttemptIdentity,
            _outcome: crate::policy::AttemptOutcome,
        ) -> std::result::Result<(), crate::policy::AttemptPolicyError> {
            self.abandoned.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn fragmented_sse(
        provider: &dyn Provider,
        model: &str,
        wire: String,
        keep_open: bool,
    ) -> Pin<Box<dyn Stream<Item = Result<String>> + Send>> {
        let bytes: Vec<_> = wire
            .as_bytes()
            .iter()
            .map(|b| Ok(Bytes::copy_from_slice(&[*b])))
            .collect();
        let tail = if keep_open {
            Box::pin(futures::stream::pending())
                as Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>
        } else {
            Box::pin(futures::stream::empty())
        };
        let target = provider.replay_target(model);
        let events = native_events(futures::stream::iter(bytes).chain(tail));
        Box::pin(futures::stream::unfold(
            (
                events,
                crate::streaming::StreamNormalizer::new(target.clone()),
                target,
            ),
            |(mut events, mut normalizer, target)| async move {
                loop {
                    if normalizer.is_finished() {
                        return None;
                    }
                    let normalized = match events.next().await {
                        Some(Ok(data)) => normalizer.push(&data),
                        Some(Err(error)) => Err(error),
                        None => normalizer.finish(),
                    };
                    match normalized {
                        Ok(Some(chunk)) => {
                            let chunk =
                                crate::cost::stamp_chunk(&target.provider, &target.model, chunk);
                            return Some((Ok(chunk), (events, normalizer, target)));
                        }
                        Ok(None) => continue,
                        Err(error) => {
                            return Some((Err(error), (events, normalizer, target)));
                        }
                    }
                }
            },
        ))
    }

    #[tokio::test]
    async fn signed_reasoning_survives_utf8_byte_splits_multiline_sse_and_crlf() {
        let p = crate::providers::anthropic::Anthropic::new("key".into());
        let events = [
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"é雪🙂"}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque+/="}}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
        ];
        let mut wire = String::from(": keepalive\r\n\r\n");
        for e in events {
            let json = e.to_string();
            let (first, rest) = json.split_once(',').unwrap();
            wire.push_str(&format!("data: {first},\r\ndata: {rest}\r\n\r\n"));
        }
        let chunks: Vec<_> = fragmented_sse(&p, "claude-sonnet-4-6", wire, false)
            .collect()
            .await;
        let mut acc = crate::reasoning::ReasoningAccumulator::default();
        for c in chunks {
            acc.push(
                &serde_json::from_str::<serde_json::Value>(&c.unwrap()).unwrap()["choices"][0]
                    ["delta"],
            )
            .unwrap();
        }
        assert_eq!(acc.blocks()[0]["text"], "é雪🙂");
        assert_eq!(acc.blocks()[0]["signature"], "opaque+/=");
    }

    #[tokio::test]
    async fn done_closes_chat_stream_after_late_usage_without_waiting_for_http_eof() {
        let p = crate::providers::openai_compat::OpenAiCompatible::new("custom", "", None);
        let wire="data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_cache_hit_tokens\":7}}\n\ndata: [DONE]\n\n";
        let chunks: Vec<_> = tokio::time::timeout(
            Duration::from_secs(1),
            fragmented_sse(&p, "model", wire.into(), true).collect(),
        )
        .await
        .unwrap();
        let last: serde_json::Value =
            serde_json::from_str(chunks.last().unwrap().as_ref().unwrap()).unwrap();
        assert_eq!(last["usage"]["cache_read_tokens"], 7);
        assert_eq!(last["choices"][0]["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn done_without_a_terminal_chunk_is_an_error() {
        let p = crate::providers::openai_compat::OpenAiCompatible::new("custom", "", None);
        let chunks: Vec<_> = fragmented_sse(&p, "model", "data: [DONE]\n\n".into(), true)
            .collect()
            .await;
        assert_eq!(chunks.len(), 1);
        assert!(matches!(chunks[0], Err(ShimError::Stream(_))));
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    // --- Retry-After parsing ------------------------------------------------

    #[test]
    fn retry_after_integer_seconds() {
        let h = headers(&[("retry-after", "5")]);
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(parse_retry_after_at(&h, now), Some(Duration::from_secs(5)));
    }

    #[test]
    fn retry_after_zero_seconds() {
        let h = headers(&[("retry-after", "0")]);
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(parse_retry_after_at(&h, now), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_http_date_future() {
        // now = 07:28:00, header = 07:28:30 GMT → 30s.
        let now = Utc.with_ymd_and_hms(2015, 10, 21, 7, 28, 0).unwrap();
        let h = headers(&[("retry-after", "Wed, 21 Oct 2015 07:28:30 GMT")]);
        assert_eq!(parse_retry_after_at(&h, now), Some(Duration::from_secs(30)));
    }

    #[test]
    fn retry_after_http_date_in_past_is_none() {
        // Header date is before `now` → no positive wait.
        let now = Utc.with_ymd_and_hms(2015, 10, 21, 7, 29, 0).unwrap();
        let h = headers(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]);
        assert_eq!(parse_retry_after_at(&h, now), None);
    }

    #[test]
    fn retry_after_absent_or_garbage_is_none() {
        let now = Utc::now();
        assert_eq!(parse_retry_after_at(&HeaderMap::new(), now), None);
        let h = headers(&[("retry-after", "soon-ish")]);
        assert_eq!(parse_retry_after_at(&h, now), None);
    }

    // --- Provider reset hints -----------------------------------------------

    #[test]
    fn openai_reset_go_duration_takes_max() {
        let now = Utc::now();
        let h = headers(&[
            ("x-ratelimit-reset-requests", "1s"),
            ("x-ratelimit-reset-tokens", "6m0s"),
        ]);
        // max(1s, 6m) = 6m = 360s
        assert_eq!(
            parse_provider_reset_at(&h, now),
            Some(Duration::from_secs(360))
        );
    }

    #[test]
    fn openai_reset_millis() {
        let now = Utc::now();
        let h = headers(&[("x-ratelimit-reset-tokens", "100ms")]);
        assert_eq!(
            parse_provider_reset_at(&h, now),
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn anthropic_reset_rfc3339() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let h = headers(&[("anthropic-ratelimit-requests-reset", "2026-01-01T00:00:10Z")]);
        assert_eq!(
            parse_provider_reset_at(&h, now),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn anthropic_reset_takes_max_across_resources() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let h = headers(&[
            ("anthropic-ratelimit-requests-reset", "2026-01-01T00:00:05Z"),
            ("anthropic-ratelimit-tokens-reset", "2026-01-01T00:00:20Z"),
        ]);
        assert_eq!(
            parse_provider_reset_at(&h, now),
            Some(Duration::from_secs(20))
        );
    }

    #[test]
    fn provider_reset_unknown_format_is_none() {
        let now = Utc::now();
        let h = headers(&[("x-ratelimit-reset-tokens", "not-a-duration")]);
        assert_eq!(parse_provider_reset_at(&h, now), None);
        assert_eq!(parse_provider_reset_at(&HeaderMap::new(), now), None);
    }

    // --- Go duration parser -------------------------------------------------

    #[test]
    fn go_duration_variants() {
        assert_eq!(parse_go_duration("1s"), Some(Duration::from_secs(1)));
        assert_eq!(parse_go_duration("6m0s"), Some(Duration::from_secs(360)));
        assert_eq!(parse_go_duration("100ms"), Some(Duration::from_millis(100)));
        assert_eq!(parse_go_duration("1h2m3s"), Some(Duration::from_secs(3723)));
        assert_eq!(parse_go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("abc"), None);
        assert_eq!(parse_go_duration("10"), None); // no unit
        assert_eq!(parse_go_duration("5x"), None); // unknown unit
    }

    #[test]
    fn go_duration_oversized_finite_values_saturate_before_capping() {
        assert_eq!(
            parse_go_duration("99999999999999999999s"),
            Some(Duration::MAX)
        );
        assert_eq!(
            parse_go_duration("9999999999999999999h"),
            Some(Duration::MAX)
        );
        assert_eq!(
            parse_go_duration("9999999999999999999s9999999999999999999s"),
            Some(Duration::MAX)
        );

        let headers = headers(&[("x-ratelimit-reset-tokens", "99999999999999999999s")]);
        let wait = retry_after_wait(&headers, Duration::from_secs(60)).unwrap();
        assert!(wait >= Duration::from_secs(60));
        assert!(wait < Duration::from_secs(60) + Duration::from_millis(251));
    }

    #[test]
    fn go_duration_nonfinite_and_negative_values_are_ignored() {
        for invalid_duration in ["NaNs", "infinitys", "-1s"] {
            assert_eq!(parse_go_duration(invalid_duration), None);
        }
        assert_eq!(parse_go_duration(&format!("{}s", "9".repeat(400))), None);
    }

    // --- Backoff / jitter ---------------------------------------------------

    #[test]
    fn backoff_bound_doubles_and_caps() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(60);
        assert_eq!(backoff_bound(0, base, cap), Duration::from_secs(1));
        assert_eq!(backoff_bound(1, base, cap), Duration::from_secs(2));
        assert_eq!(backoff_bound(2, base, cap), Duration::from_secs(4));
        // 2^10 = 1024s clamped to cap.
        assert_eq!(backoff_bound(10, base, cap), cap);
        // Absurd attempt must not overflow/panic.
        assert_eq!(backoff_bound(200, base, cap), cap);
    }

    #[test]
    fn full_jitter_stays_within_bound() {
        let bound = Duration::from_millis(1000);
        for rand in [0u64, 1, 500, 1000, 1001, u64::MAX] {
            let j = full_jitter(bound, rand);
            assert!(j <= bound, "jitter {j:?} exceeded bound {bound:?}");
        }
        assert_eq!(full_jitter(bound, 0), Duration::ZERO);
        assert_eq!(full_jitter(Duration::ZERO, u64::MAX), Duration::ZERO);
    }

    #[test]
    fn backoff_with_jitter_within_bound_over_many_draws() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(60);
        for attempt in 0..4 {
            let bound = backoff_bound(attempt, base, cap);
            for _ in 0..200 {
                let d = backoff_with_jitter(attempt, base, cap);
                assert!(d <= bound, "{d:?} exceeded bound {bound:?}");
            }
        }
    }

    // --- retry_after_wait (combines + caps + jitters) -----------------------

    #[test]
    fn retry_after_wait_caps_bogus_header() {
        // 999999s Retry-After must be clamped to cap (+ small jitter < 251ms).
        let h = headers(&[("retry-after", "999999")]);
        let cap = Duration::from_secs(60);
        let w = retry_after_wait(&h, cap).unwrap();
        assert!(w >= cap && w < cap + Duration::from_millis(251));
    }

    #[test]
    fn retry_after_wait_none_without_hints() {
        assert_eq!(
            retry_after_wait(&HeaderMap::new(), Duration::from_secs(60)),
            None
        );
    }

    // --- Env config ---------------------------------------------------------

    #[test]
    fn retry_config_defaults() {
        let d = RetryConfig::default();
        assert_eq!(d.max_retries, 3);
        assert_eq!(d.base, Duration::from_secs(1));
        assert_eq!(d.cap, Duration::from_secs(60));
    }

    #[test]
    fn env_parse_valid_and_invalid() {
        // Unique keys so we never race the real LLMSHIM_* vars other tests use.
        std::env::set_var("LLMSHIM_TEST_ENV_PARSE_OK", "7");
        std::env::set_var("LLMSHIM_TEST_ENV_PARSE_BAD", "not-a-number");
        assert_eq!(env_parse::<u32>("LLMSHIM_TEST_ENV_PARSE_OK"), Some(7));
        assert_eq!(env_parse::<u32>("LLMSHIM_TEST_ENV_PARSE_BAD"), None);
        assert_eq!(env_parse::<u32>("LLMSHIM_TEST_ENV_PARSE_MISSING"), None);
        std::env::remove_var("LLMSHIM_TEST_ENV_PARSE_OK");
        std::env::remove_var("LLMSHIM_TEST_ENV_PARSE_BAD");
    }

    // --- Integration (mockito, local only — no provider API calls) ----------

    #[tokio::test]
    async fn redirects_do_not_forward_prompts_or_credentials() {
        for status in [301, 302, 303, 307, 308] {
            for same_origin in [false, true] {
                let mut origin = mockito::Server::new_async().await;
                let mut other = mockito::Server::new_async().await;
                let destination = if same_origin { &mut origin } else { &mut other };
                let location = format!("{}/moved", destination.url());
                let forwarded = destination
                    .mock(if status <= 303 { "GET" } else { "POST" }, "/moved")
                    .with_status(200)
                    .with_body("unexpected forwarding")
                    .expect(0)
                    .create_async()
                    .await;
                let redirect = origin
                    .mock("POST", "/v1/chat/completions")
                    .match_header("x-api-key", "test-secret")
                    .match_body(mockito::Matcher::Json(serde_json::json!({
                        "messages": [{"role": "user", "content": "private transcript"}]
                    })))
                    .with_status(status)
                    .with_header("location", &location)
                    .expect(1)
                    .create_async()
                    .await;
                let request = ProviderRequest {
                    url: format!("{}/v1/chat/completions", origin.url()),
                    headers: vec![("x-api-key".into(), "test-secret".into())],
                    body: serde_json::json!({
                        "messages": [{"role": "user", "content": "private transcript"}]
                    }),
                };
                let result = ShimClient::new().send(&request).await;
                redirect.assert_async().await;
                forwarded.assert_async().await;
                assert!(
                    matches!(result, Err(ShimError::ProviderError { status: actual, .. }) if actual == status as u16)
                );
            }
        }
    }

    fn raw_loopback_request(url: String) -> ProviderRequest {
        ProviderRequest {
            url,
            headers: vec![],
            body: serde_json::json!({"messages": []}),
        }
    }

    #[tokio::test]
    async fn response_header_timeout_uses_only_the_existing_transport_retry_count() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let server_accepts = accepts.clone();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                server_accepts.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut request = [0_u8; 1024];
                    let _ = socket.read(&mut request).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                });
            }
        });
        let deadlines = AttemptDeadlines::default()
            .with_response_header_timeout(Duration::from_millis(20))
            .unwrap()
            .with_unary_timeouts(Duration::from_secs(1), Duration::from_secs(1))
            .unwrap();
        let client = ShimClient {
            retry: RetryConfig {
                max_retries: 1,
                base: Duration::ZERO,
                cap: Duration::ZERO,
            },
            ..ShimClient::new().with_attempt_deadlines(deadlines).unwrap()
        };
        let error = client
            .send(&raw_loopback_request(format!("http://{address}/headers")))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ShimError::ProviderError { status: 504, .. }
        ));
        server.await.unwrap();
        assert_eq!(accepts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stalled_error_body_preserves_the_received_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 10\r\n\r\nx")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let deadlines = AttemptDeadlines::default()
            .with_error_body_timeouts(Duration::from_millis(20), Duration::from_millis(40))
            .unwrap();
        let client = ShimClient {
            retry: RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            },
            ..ShimClient::new().with_attempt_deadlines(deadlines).unwrap()
        };
        let error = client
            .send(&raw_loopback_request(format!("http://{address}/error")))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ShimError::ProviderError { status: 500, ref body, .. }
                if body == "upstream error body timed out"
        ));
    }

    #[tokio::test]
    async fn raw_send_returns_after_headers_without_attaching_a_body_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nx")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let deadlines = AttemptDeadlines::default()
            .with_unary_timeouts(Duration::from_millis(10), Duration::from_millis(20))
            .unwrap();
        let client = ShimClient::new().with_attempt_deadlines(deadlines).unwrap();
        let response = client
            .send(&raw_loopback_request(format!("http://{address}/raw")))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), response.bytes())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unpolled_stream_releases_source_at_total_and_retains_timeout_terminal() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (source_closed_sender, source_closed_receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await;
            let first = serde_json::json!({
                "choices": [{"index": 0, "delta": {"content": "first"}, "finish_reason": null}]
            });
            let second = serde_json::json!({
                "choices": [{"index": 0, "delta": {"content": "second"}, "finish_reason": null}]
            });
            let first_frame = format!("data: {first}\n\n");
            let second_frame = format!("data: {second}\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n{:x}\r\n{}\r\n",
                first_frame.len(),
                first_frame,
                second_frame.len(),
                second_frame,
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            let mut byte = [0_u8; 1];
            let closed = socket.read(&mut byte).await.unwrap() == 0;
            let _ = source_closed_sender.send(closed);
        });
        let deadlines = AttemptDeadlines::default()
            .with_stream_timeouts(Duration::from_secs(1), Duration::from_millis(50))
            .unwrap();
        let client = ShimClient::new().with_attempt_deadlines(deadlines).unwrap();
        let provider = crate::providers::openai_compat::OpenAiCompatible::new(
            "test-provider",
            format!("http://{address}"),
            None,
        );
        let mut stream = client
            .stream(
                &provider,
                "test-model",
                &serde_json::json!({"messages": []}),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), source_closed_receiver)
                .await
                .unwrap()
                .unwrap()
        );
        assert!(stream.next().await.unwrap().is_ok());
        assert!(matches!(
            stream.next().await.unwrap(),
            Err(ShimError::ProviderError { status: 504, .. })
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn semantic_idle_restarts_after_backpressure_before_total_expiry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            for content in ["first", "second"] {
                let event = serde_json::json!({
                    "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]
                });
                let frame = format!("data: {event}\n\n");
                let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
                socket.write_all(chunk.as_bytes()).await.unwrap();
            }
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(120)).await;
            let terminal = serde_json::json!({
                "choices": [{"index": 0, "delta": {"content": "third"}, "finish_reason": "stop"}]
            });
            for frame in [format!("data: {terminal}\n\n"), "data: [DONE]\n\n".into()] {
                let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
                socket.write_all(chunk.as_bytes()).await.unwrap();
            }
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let deadlines = AttemptDeadlines::default()
            .with_stream_timeouts(Duration::from_millis(50), Duration::from_secs(1))
            .unwrap();
        let client = ShimClient::new().with_attempt_deadlines(deadlines).unwrap();
        let provider = crate::providers::openai_compat::OpenAiCompatible::new(
            "test-provider",
            format!("http://{address}"),
            None,
        );
        let mut stream = client
            .stream(
                &provider,
                "test-model",
                &serde_json::json!({"messages": []}),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let chunks: Vec<_> = stream.by_ref().collect().await;
        assert!(chunks.iter().all(Result::is_ok));
        let text = chunks
            .into_iter()
            .map(|chunk| chunk.unwrap())
            .collect::<String>();
        assert!(text.contains("first"));
        assert!(text.contains("second"));
        assert!(text.contains("third"));
    }

    #[tokio::test]
    async fn receiver_drop_cancels_pending_callbacks_and_closes_provider_source() {
        for pending in [PendingStreamCallback::Usage, PendingStreamCallback::Finish] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (source_closed_sender, source_closed_receiver) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0_u8; 4096];
                let _ = socket.read(&mut request).await;
                let event = match pending {
                    PendingStreamCallback::Usage => serde_json::json!({
                        "choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    }),
                    PendingStreamCallback::Finish => serde_json::json!({
                        "choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": "stop"}]
                    }),
                };
                let frames = format!("data: {event}\n\ndata: [DONE]\n\n");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n",
                    frames.len(), frames
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.flush().await.unwrap();
                let mut byte = [0_u8; 1];
                let closed = socket.read(&mut byte).await.unwrap() == 0;
                let _ = source_closed_sender.send(closed);
            });
            let deadlines = AttemptDeadlines::default()
                .with_policy_callback_timeout(Duration::from_secs(1))
                .unwrap()
                .with_stream_timeouts(Duration::from_secs(1), Duration::from_secs(2))
                .unwrap();
            let client = ShimClient::new().with_attempt_deadlines(deadlines).unwrap();
            let provider = crate::providers::openai_compat::OpenAiCompatible::new(
                "test-provider",
                format!("http://{address}"),
                None,
            );
            let policy = Arc::new(PendingStreamPolicy {
                pending,
                started: tokio::sync::Notify::new(),
                abandoned: std::sync::atomic::AtomicBool::new(false),
            });
            let context = DispatchPolicyContext::new(policy.clone());
            let mut stream = client
                .stream_with_policy(
                    &provider,
                    "test-model",
                    &serde_json::json!({"messages": []}),
                    &context,
                )
                .await
                .unwrap();
            if matches!(pending, PendingStreamCallback::Finish) {
                assert!(stream.next().await.unwrap().is_ok());
            }
            tokio::time::timeout(Duration::from_secs(1), policy.started.notified())
                .await
                .unwrap_or_else(|_| panic!("{pending:?} callback did not start"));
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(200), source_closed_receiver)
                    .await
                    .unwrap()
                    .unwrap()
            );
            assert!(policy.abandoned.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn comments_whitespace_and_incomplete_frames_do_not_reset_semantic_idle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            for fragment in [": comment\n\n", "data:   \n\n", "data: {"] {
                let chunk = format!("{:x}\r\n{}\r\n", fragment.len(), fragment);
                if socket.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let deadlines = AttemptDeadlines::default()
            .with_stream_timeouts(Duration::from_millis(25), Duration::from_secs(1))
            .unwrap();
        let client = ShimClient::new().with_attempt_deadlines(deadlines).unwrap();
        let provider = crate::providers::openai_compat::OpenAiCompatible::new(
            "test-provider",
            format!("http://{address}"),
            None,
        );
        let mut stream = client
            .stream(
                &provider,
                "test-model",
                &serde_json::json!({"messages": []}),
            )
            .await
            .unwrap();
        assert!(matches!(
            stream.next().await.unwrap(),
            Err(ShimError::ProviderError { status: 504, .. })
        ));
    }

    #[tokio::test]
    async fn extreme_provider_reset_headers_retry_without_panicking() {
        for reset_header in [
            "99999999999999999999s".to_owned(),
            "9999999999999999999s9999999999999999999s".to_owned(),
            format!("{}s", "9".repeat(400)),
        ] {
            let mut upstream_server = mockito::Server::new_async().await;
            let rate_limited_response = upstream_server
                .mock("POST", "/v1/chat")
                .with_status(429)
                .with_header("x-ratelimit-reset-tokens", &reset_header)
                .with_body("rate limited")
                .expect(1)
                .create_async()
                .await;
            let successful_response = upstream_server
                .mock("POST", "/v1/chat")
                .with_status(200)
                .with_body("ok")
                .expect(1)
                .create_async()
                .await;
            let client = ShimClient {
                retry: RetryConfig {
                    max_retries: 1,
                    base: Duration::ZERO,
                    cap: Duration::from_millis(10),
                },
                ..ShimClient::new()
            };
            let request = ProviderRequest {
                url: format!("{}/v1/chat", upstream_server.url()),
                headers: vec![],
                body: serde_json::json!({"messages": []}),
            };
            let response = tokio::time::timeout(Duration::from_secs(5), client.send(&request))
                .await
                .expect("reset hint must remain bounded by the retry cap")
                .expect("bounded retry must succeed");
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(response.text().await.unwrap(), "ok");
            rate_limited_response.assert_async().await;
            successful_response.assert_async().await;
        }
    }

    #[tokio::test]
    async fn honors_retry_after_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        // First response: 429 with Retry-After: 1 (served once).
        let m429 = server
            .mock("POST", "/v1/chat")
            .with_status(429)
            .with_header("retry-after", "1")
            .with_body("rate limited")
            .expect(1)
            .create_async()
            .await;
        // Then: success.
        let m200 = server
            .mock("POST", "/v1/chat")
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create_async()
            .await;

        let client = ShimClient::new();
        let req = ProviderRequest {
            url: format!("{}/v1/chat", server.url()),
            headers: vec![],
            body: serde_json::json!({"hello": "world"}),
        };

        let start = std::time::Instant::now();
        let resp = client.send(&req).await.expect("should succeed after retry");
        let elapsed = start.elapsed();

        assert!(resp.status().is_success());
        // Proves we waited on Retry-After: 1 (allow small scheduling slack).
        assert!(
            elapsed >= Duration::from_millis(900),
            "expected ~1s Retry-After wait, got {elapsed:?}"
        );
        assert_eq!(resp.text().await.unwrap(), "ok");
        m429.assert_async().await;
        m200.assert_async().await;
    }

    #[tokio::test]
    async fn falls_back_to_jittered_backoff_without_header() {
        let mut server = mockito::Server::new_async().await;
        // 500 with NO Retry-After → client uses jittered backoff (bound 1s at attempt 0).
        let m500 = server
            .mock("POST", "/v1/chat")
            .with_status(500)
            .with_body("boom")
            .expect(1)
            .create_async()
            .await;
        let m200 = server
            .mock("POST", "/v1/chat")
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create_async()
            .await;

        let client = ShimClient::new();
        let req = ProviderRequest {
            url: format!("{}/v1/chat", server.url()),
            headers: vec![],
            body: serde_json::json!({}),
        };

        let start = std::time::Instant::now();
        let resp = client.send(&req).await.expect("should succeed after retry");
        let elapsed = start.elapsed();

        assert!(resp.status().is_success());
        // Full-jitter backoff at attempt 0 is bounded by base (1s); give slack.
        assert!(
            elapsed < Duration::from_secs(3),
            "backoff should be sub-cap jitter, got {elapsed:?}"
        );
        m500.assert_async().await;
        m200.assert_async().await;
    }
}

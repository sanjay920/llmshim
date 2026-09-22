use crate::breaker::ProviderBreaker;
use crate::error::{Result, ShimError};
use crate::policy::{
    AttemptKind, AttemptOutcome, AttemptPolicyError, AttemptPolicyRefusal, AttemptTracker,
    DispatchPolicyContext,
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
    PolicyRefusal(AttemptPolicyRefusal),
    PolicyObservation(AttemptPolicyError),
}

pub(crate) type DispatchResult<T> = std::result::Result<T, DispatchFailure>;

impl DispatchFailure {
    pub(crate) fn into_public(self) -> ShimError {
        match self {
            Self::Upstream(error) | Self::Local(error) => error,
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

#[derive(Clone)]
pub struct ShimClient {
    http: Client,
    retry: RetryConfig,
    response_body_limits: body::ResponseBodyLimits,
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
        Self {
            http: Client::builder()
                // Prompts and custom provider credentials belong only at the
                // configured endpoint, never an HTTP Location target.
                .redirect(reqwest::redirect::Policy::none())
                .pool_idle_timeout(Duration::from_secs(90))
                .pool_max_idle_per_host(4)
                .tcp_keepalive(Duration::from_secs(30))
                .tcp_nodelay(true)
                .build()
                .expect("failed to build HTTP client"),
            retry: RetryConfig::from_env(),
            response_body_limits: body::ResponseBodyLimits::default(),
            breaker: None,
        }
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
            let mut attempt_tracker = match policy_context {
                Some(context) => {
                    let Some(target) = prepared_target else {
                        return Err(DispatchFailure::Local(ShimError::Stream(
                            "missing prepared dispatch target".into(),
                        )));
                    };
                    Some(
                        context
                            .acquire(attempt_kind, resolved_model, target, &req.url, &req.body)
                            .await
                            .map_err(DispatchFailure::PolicyRefusal)?,
                    )
                }
                None => None,
            };

            match self.http.execute(http_request).await {
                Ok(resp) => {
                    let status = resp.status();
                    if let Some(tracker) = attempt_tracker.as_ref() {
                        tracker
                            .response_headers(status.as_u16())
                            .await
                            .map_err(DispatchFailure::PolicyObservation)?;
                    }
                    if status.is_success() {
                        return Ok(AttemptResponse {
                            response: resp,
                            tracker: attempt_tracker,
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
                        let error_body =
                            body::read(resp, self.response_body_limits.error_bytes).await;
                        if let (Ok(error_body), Some(target)) =
                            (error_body.as_ref(), prepared_target)
                        {
                            observe_bounded_error_usage(target, error_body, &mut attempt_tracker)
                                .await?;
                        }
                        if let Some(tracker) = attempt_tracker.as_mut() {
                            let accounting = tracker.accounting(false);
                            tracker
                                .finish(AttemptOutcome::HttpFailure {
                                    status: status_code,
                                    accounting,
                                })
                                .await
                                .map_err(DispatchFailure::PolicyObservation)?;
                        }
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    // Read before the body is consumed: the header is the
                    // server's own wait, and a caller with its own backoff
                    // above this client gets to honour it too.
                    let retry_after = parse_retry_after(resp.headers());
                    let error_body =
                        body::read_text_and_bytes(resp, self.response_body_limits.error_bytes)
                            .await;
                    if let (Ok((_, error_body)), Some(target)) =
                        (error_body.as_ref(), prepared_target)
                    {
                        observe_bounded_error_usage(target, error_body, &mut attempt_tracker)
                            .await?;
                    }
                    if let Some(tracker) = attempt_tracker.as_mut() {
                        let accounting = tracker.accounting(false);
                        tracker
                            .finish(AttemptOutcome::HttpFailure {
                                status: status_code,
                                accounting,
                            })
                            .await
                            .map_err(DispatchFailure::PolicyObservation)?;
                    }
                    let body = match error_body {
                        Ok((error_body_text, _)) => error_body_text,
                        Err(body::BodyReadError::TooLarge) => {
                            return Err(body::BodyReadError::TooLarge.into_dispatch_failure());
                        }
                        Err(body::BodyReadError::Http(_)) => String::new(),
                    };
                    return Err(DispatchFailure::Upstream(ShimError::ProviderError {
                        status: status_code,
                        body,
                        retry_after,
                    }));
                }
                // Transport errors carry no headers: always jittered backoff.
                Err(e) if Self::is_retryable_transport(&e) && attempt < max_retries => {
                    if let Some(tracker) = attempt_tracker.as_mut() {
                        let accounting = tracker.accounting(false);
                        tracker
                            .finish(AttemptOutcome::TransportFailure { accounting })
                            .await
                            .map_err(DispatchFailure::PolicyObservation)?;
                    }
                    tokio::time::sleep(backoff_with_jitter(
                        attempt,
                        self.retry.base,
                        self.retry.cap,
                    ))
                    .await;
                    continue;
                }
                Err(error) => {
                    if let Some(tracker) = attempt_tracker.as_mut() {
                        let accounting = tracker.accounting(false);
                        tracker
                            .finish(AttemptOutcome::TransportFailure { accounting })
                            .await
                            .map_err(DispatchFailure::PolicyObservation)?;
                    }
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
            let collected =
                crate::providers::chatgpt::collect_response_with_terminal(model, response).await;
            if let Some(native_terminal) = collected.native_terminal.as_ref() {
                observe_native_response_usage(&target, native_terminal, &mut tracker).await?;
            }
            let mut result = match collected.result {
                Ok(result) => result,
                Err(error) => {
                    finish_invalid_response(&mut tracker).await?;
                    return Err(DispatchFailure::Upstream(error));
                }
            };
            crate::reasoning::bind_response_context(&mut result, &target);
            crate::toolcall::bind_response_context(&mut result, &target);
            finish_completed_response(&mut tracker).await?;
            return Ok((result, target));
        }
        let body = match body::read_json(response, self.response_body_limits.success_bytes).await {
            Ok(body) => body,
            Err(error) => {
                finish_invalid_response(&mut tracker).await?;
                return Err(error.into_dispatch_failure());
            }
        };
        observe_native_response_usage(&target, &body, &mut tracker).await?;
        let mut result = match provider.transform_response(model, body) {
            Ok(result) => result,
            Err(error) => {
                finish_invalid_response(&mut tracker).await?;
                return Err(DispatchFailure::Upstream(error));
            }
        };
        crate::reasoning::bind_response_context(&mut result, &target);
        crate::toolcall::bind_response_context(&mut result, &target);
        finish_completed_response(&mut tracker).await?;
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
        let AttemptResponse { response, tracker } = self
            .send_prepared(
                policy_context,
                AttemptKind::Stream,
                model,
                Some(&target),
                &provider_req,
            )
            .await?;
        let events = native_events(response.bytes_stream());
        let sse = SseStream {
            inner: events,
            normalizer: crate::streaming::StreamNormalizer::new(target.clone()),
            native_usage: crate::usage::NativeStreamUsage::new(target.clone()),
            pending: None,
        };
        let policy_failure = Arc::new(std::sync::Mutex::new(None));
        let stream = match tracker {
            Some(tracker) => observe_stream(
                Box::pin(sse),
                tracker,
                target.clone(),
                policy_failure.clone(),
            ),
            None => normalized_stream(Box::pin(sse), target.clone()),
        };

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
}

struct StreamDispatch {
    stream: Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
    policy_failure: Arc<std::sync::Mutex<Option<AttemptPolicyError>>>,
}

async fn observe_native_response_usage(
    target: &ReplayTarget,
    native_response: &serde_json::Value,
    tracker: &mut Option<AttemptTracker>,
) -> DispatchResult<()> {
    let Some(tracker) = tracker.as_mut() else {
        return Ok(());
    };
    if let Some(mut observation) =
        crate::usage::normalize_native_response_usage_observation(target, native_response)
    {
        stamp_usage(target, &mut observation.usage);
        tracker
            .usage(
                &observation.usage,
                observation.terminal,
                observation.counters_complete,
                observation.explicit_zero,
            )
            .await
            .map_err(DispatchFailure::PolicyObservation)?;
    }
    Ok(())
}

async fn observe_bounded_error_usage(
    target: &ReplayTarget,
    bounded_body: &[u8],
    tracker: &mut Option<AttemptTracker>,
) -> DispatchResult<()> {
    let Ok(native_error) = serde_json::from_slice::<serde_json::Value>(bounded_body) else {
        return Ok(());
    };
    observe_native_response_usage(target, &native_error, tracker).await
}

async fn finish_completed_response(tracker: &mut Option<AttemptTracker>) -> DispatchResult<()> {
    let Some(tracker) = tracker.as_mut() else {
        return Ok(());
    };
    let accounting = tracker.accounting(true);
    tracker
        .finish(AttemptOutcome::Completed { accounting })
        .await
        .map_err(DispatchFailure::PolicyObservation)
}

async fn finish_invalid_response(tracker: &mut Option<AttemptTracker>) -> DispatchResult<()> {
    let Some(tracker) = tracker.as_mut() else {
        return Ok(());
    };
    let accounting = tracker.accounting(false);
    tracker
        .finish(AttemptOutcome::InvalidResponse { accounting })
        .await
        .map_err(DispatchFailure::PolicyObservation)
}

fn stamp_usage(target: &ReplayTarget, usage: &mut serde_json::Value) {
    let mut response = serde_json::json!({"usage": usage.take()});
    crate::cost::stamp(&target.provider, &target.model, &mut response);
    *usage = response["usage"].take();
}

struct PolicyStreamState {
    inner: Pin<Box<dyn Stream<Item = Result<SseOutput>> + Send>>,
    tracker: AttemptTracker,
    target: ReplayTarget,
    policy_failure: Arc<std::sync::Mutex<Option<AttemptPolicyError>>>,
    ended: bool,
}

fn observe_stream(
    stream: Pin<Box<dyn Stream<Item = Result<SseOutput>> + Send>>,
    tracker: AttemptTracker,
    target: ReplayTarget,
    policy_failure: Arc<std::sync::Mutex<Option<AttemptPolicyError>>>,
) -> Pin<Box<dyn Stream<Item = Result<String>> + Send>> {
    Box::pin(futures::stream::unfold(
        PolicyStreamState {
            inner: stream,
            tracker,
            target,
            policy_failure,
            ended: false,
        },
        |mut state| async move {
            loop {
                if state.ended {
                    return None;
                }
                match state.inner.next().await {
                    Some(Ok(SseOutput::Usage(mut observation))) => {
                        stamp_usage(&state.target, &mut observation.usage);
                        if let Err(error) = state
                            .tracker
                            .usage(
                                &observation.usage,
                                observation.terminal,
                                observation.counters_complete,
                                observation.explicit_zero,
                            )
                            .await
                        {
                            record_policy_failure(&state.policy_failure, error);
                            state.ended = true;
                            return Some((Err(error.into_shim_error()), state));
                        }
                    }
                    Some(Ok(SseOutput::Chunk(chunk))) => {
                        let chunk = crate::cost::stamp_chunk(
                            &state.target.provider,
                            &state.target.model,
                            chunk,
                        );
                        return Some((Ok(chunk), state));
                    }
                    Some(Err(error)) => {
                        let accounting = state.tracker.accounting(false);
                        if let Err(policy_error) = state
                            .tracker
                            .finish(AttemptOutcome::StreamFailure { accounting })
                            .await
                        {
                            record_policy_failure(&state.policy_failure, policy_error);
                            state.ended = true;
                            return Some((Err(policy_error.into_shim_error()), state));
                        }
                        state.ended = true;
                        return Some((Err(error), state));
                    }
                    None => {
                        let accounting = state.tracker.accounting(true);
                        if let Err(error) = state
                            .tracker
                            .finish(AttemptOutcome::Completed { accounting })
                            .await
                        {
                            record_policy_failure(&state.policy_failure, error);
                            state.ended = true;
                            return Some((Err(error.into_shim_error()), state));
                        }
                        return None;
                    }
                }
            }
        },
    ))
}

fn normalized_stream(
    stream: Pin<Box<dyn Stream<Item = Result<SseOutput>> + Send>>,
    target: ReplayTarget,
) -> Pin<Box<dyn Stream<Item = Result<String>> + Send>> {
    Box::pin(futures::stream::unfold(
        (stream, target),
        |(mut stream, target)| async move {
            loop {
                match stream.next().await {
                    Some(Ok(SseOutput::Usage(_))) => continue,
                    Some(Ok(SseOutput::Chunk(chunk))) => {
                        let chunk =
                            crate::cost::stamp_chunk(&target.provider, &target.model, chunk);
                        return Some((Ok(chunk), (stream, target)));
                    }
                    Some(Err(error)) => return Some((Err(error), (stream, target))),
                    None => return None,
                }
            }
        },
    ))
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

struct SseStream {
    inner: Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
    normalizer: crate::streaming::StreamNormalizer,
    native_usage: crate::usage::NativeStreamUsage,
    pending: Option<Result<Option<String>>>,
}

impl Stream for SseStream {
    type Item = Result<SseOutput>;
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        loop {
            if let Some(pending) = self.pending.take() {
                match pending {
                    Ok(Some(chunk)) => return Poll::Ready(Some(Ok(SseOutput::Chunk(chunk)))),
                    Ok(None) => continue,
                    Err(error) => return Poll::Ready(Some(Err(error))),
                }
            }
            if self.normalizer.is_finished() {
                return Poll::Ready(None);
            }
            let data = match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(data))) => data,
                Poll::Ready(Some(Err(error))) => {
                    self.normalizer.abort();
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) => {
                    return Poll::Ready(match self.normalizer.finish() {
                        Ok(Some(chunk)) => Some(Ok(SseOutput::Chunk(chunk))),
                        Ok(None) => None,
                        Err(error) => Some(Err(error)),
                    })
                }
                Poll::Pending => return Poll::Pending,
            };
            let native_usage = self.native_usage.ingest(&data);
            let normalized = self.normalizer.push(&data);
            if let Some(usage) = native_usage {
                self.pending = Some(normalized);
                return Poll::Ready(Some(Ok(SseOutput::Usage(usage))));
            }
            match normalized {
                Ok(Some(chunk)) => return Poll::Ready(Some(Ok(SseOutput::Chunk(chunk)))),
                Ok(None) => continue,
                Err(error) => {
                    self.normalizer.abort();
                    return Poll::Ready(Some(Err(error)));
                }
            }
        }
    }
}

enum SseOutput {
    Usage(crate::usage::NativeUsageObservation),
    Chunk(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use reqwest::header::{HeaderMap, HeaderValue};

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
        normalized_stream(
            Box::pin(SseStream {
                inner: native_events(futures::stream::iter(bytes).chain(tail)),
                normalizer: crate::streaming::StreamNormalizer::new(target.clone()),
                native_usage: crate::usage::NativeStreamUsage::new(target.clone()),
                pending: None,
            }),
            target,
        )
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
            );
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

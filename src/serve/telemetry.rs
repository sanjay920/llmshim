//! Trace export (0016), behind the default-off `serve-otel` feature.
//!
//! One server span per request. The attribute set is closed: a token, a header
//! value, a query string, a prompt, a completion or an error body is never one,
//! and a test pins the list. Telemetry fails open: spans go to a bounded queue
//! that a worker thread drains, a full queue drops the span and counts it, a
//! failed export is counted, and neither ever reaches a request.
//!
//! Without the feature this module is a stub with the same surface, so the
//! access-log layer needs no `cfg` of its own.

use super::config::Telemetry as TelemetryConfig;
use super::config::{read_secret, Issue};
use crate::gateway::usage::{AttemptBegun, AttemptSpan, AttemptSpans, UsageRecord};
use axum::http::HeaderMap;
use std::sync::Arc;

#[cfg(feature = "serve-otel")]
pub use real::{RequestSpan, Telemetry};
#[cfg(not(feature = "serve-otel"))]
pub use stub::{RequestSpan, Telemetry};

/// The caller as the key store resolved it, for the span's attributes.
pub struct SpanCaller {
    pub tenant: Option<String>,
    pub key: Option<String>,
}

/// Start telemetry from the file's `[telemetry]` table, or `None` without one.
/// Header values are read here, once; a failure names the setting and never the
/// value.
pub fn start(
    config: &TelemetryConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Arc<Telemetry>>, Issue> {
    let mut headers = Vec::new();
    for (name, secret) in &config.headers {
        let value = read_secret(secret, env).map_err(|message| Issue {
            path: format!("telemetry.headers.{name}"),
            message,
        })?;
        headers.push((name.clone(), value.trim().to_owned()));
    }
    Telemetry::start(config, headers).map(|telemetry| telemetry.map(Arc::new))
}

#[cfg(not(feature = "serve-otel"))]
mod stub {
    use super::*;

    pub struct Telemetry;
    pub struct RequestSpan;

    impl Telemetry {
        pub(super) fn start(
            _config: &TelemetryConfig,
            _headers: Vec<(String, String)>,
        ) -> Result<Option<Self>, Issue> {
            Ok(None)
        }

        pub fn request_span(
            &self,
            _method: &str,
            _route: &str,
            _request_id: &str,
            caller: &SpanCaller,
            _inbound: Option<&HeaderMap>,
        ) -> RequestSpan {
            let _ = (&caller.tenant, &caller.key);
            RequestSpan
        }

        pub fn shutdown(&self) {}
    }

    impl RequestSpan {
        pub fn end(self, _status: Option<u16>) {}
    }

    struct NoAttemptSpan;

    impl AttemptSpans for Telemetry {
        fn start(&self, _begun: &AttemptBegun<'_>) -> Box<dyn AttemptSpan> {
            Box::new(NoAttemptSpan)
        }
    }

    impl AttemptSpan for NoAttemptSpan {
        fn end(self: Box<Self>, _record: &UsageRecord) {}
    }
}

#[cfg(feature = "serve-otel")]
mod real {
    use super::*;
    use opentelemetry::trace::{
        Span as _, SpanKind, Status, TraceContextExt as _, Tracer as _, TracerProvider as _,
    };
    use opentelemetry::{Context, KeyValue};
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::{
        Sampler, SdkTracer, SdkTracerProvider, Span as SdkSpan, SpanData, SpanProcessor,
    };
    use opentelemetry_sdk::Resource;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Spans that may wait for the worker before new ones are dropped.
    const QUEUE: usize = 2048;
    /// Spans in one export request.
    const BATCH: usize = 256;
    /// How long a partial batch waits before it is sent.
    const SCHEDULE: Duration = Duration::from_secs(5);
    /// One export request's whole deadline (connect included).
    const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
    /// How long shutdown waits for what is queued, whole (a full queue included).
    const SHUTDOWN: Duration = Duration::from_secs(5);

    pub const SPANS_DROPPED: &str = "llmshim_telemetry_spans_dropped_total";
    pub const EXPORT_FAILURES: &str = "llmshim_telemetry_export_failures_total";

    /// Live request contexts an attempt may still be parented to.
    const REGISTRY: usize = 8192;
    /// How long the context of a request that has had no attempt yet stays after
    /// its span ended: a streamed answer may be dispatched after the response
    /// head went out. A request that has had attempts is dropped as soon as the
    /// last one ends.
    const LINGER: Duration = Duration::from_secs(60);

    pub struct Telemetry {
        tracer: SdkTracer,
        provider: SdkTracerProvider,
        requests: Arc<Requests>,
    }

    /// The span contexts of requests in flight, by request id, so an attempt,
    /// which the gateway knows only by that id, joins its request's trace.
    /// Only authenticated callers' requests are registered (an unauthenticated
    /// caller chooses its id and must not be able to take another's), the first
    /// registration of an id wins, and the table is bounded: when full, a new
    /// request is not registered and its attempts start their own traces.
    #[derive(Default)]
    struct Requests(std::sync::Mutex<Registry>);

    #[derive(Default)]
    struct Registry {
        map: std::collections::HashMap<String, Entry>,
        /// Ended requests that had no attempt, oldest first, with the entry's
        /// generation so a reused id is not removed by a stale item.
        lingering: std::collections::VecDeque<(std::time::Instant, String, u64)>,
        generation: u64,
    }

    struct Entry {
        context: opentelemetry::trace::SpanContext,
        generation: u64,
        ended: bool,
        open: usize,
        seen: bool,
    }

    impl Registry {
        fn prune(&mut self) {
            while let Some((at, _, _)) = self.lingering.front() {
                if at.elapsed() < LINGER {
                    break;
                }
                let (_, id, generation) = self.lingering.pop_front().expect("front exists");
                if self
                    .map
                    .get(&id)
                    .is_some_and(|e| e.generation == generation && e.ended && e.open == 0)
                {
                    self.map.remove(&id);
                }
            }
        }
    }

    impl Requests {
        /// Whether this request now owns the id's entry.
        fn register(&self, id: &str, context: opentelemetry::trace::SpanContext) -> bool {
            let Ok(mut reg) = self.0.lock() else {
                return false;
            };
            reg.prune();
            if reg.map.len() >= REGISTRY || reg.map.contains_key(id) {
                return false;
            }
            reg.generation += 1;
            let generation = reg.generation;
            reg.map.insert(
                id.to_owned(),
                Entry {
                    context,
                    generation,
                    ended: false,
                    open: 0,
                    seen: false,
                },
            );
            true
        }

        fn ended(&self, id: &str) {
            let Ok(mut reg) = self.0.lock() else { return };
            let Some(entry) = reg.map.get_mut(id) else {
                return;
            };
            entry.ended = true;
            let (open, seen, generation) = (entry.open, entry.seen, entry.generation);
            if open == 0 {
                if seen {
                    reg.map.remove(id);
                } else {
                    reg.lingering
                        .push_back((std::time::Instant::now(), id.to_owned(), generation));
                }
            }
        }

        /// The request's context, counting an attempt that now holds it.
        fn begin_attempt(&self, id: &str) -> Option<opentelemetry::trace::SpanContext> {
            let mut reg = self.0.lock().ok()?;
            let entry = reg.map.get_mut(id)?;
            entry.open += 1;
            entry.seen = true;
            Some(entry.context.clone())
        }

        fn end_attempt(&self, id: &str) {
            let Ok(mut reg) = self.0.lock() else { return };
            let Some(entry) = reg.map.get_mut(id) else {
                return;
            };
            entry.open = entry.open.saturating_sub(1);
            if entry.ended && entry.open == 0 {
                reg.map.remove(id);
            }
        }
    }

    pub struct RequestSpan {
        span: SdkSpan,
        /// The id this span owns in the registry, if it does.
        id: Option<String>,
        requests: Arc<Requests>,
    }

    impl Telemetry {
        pub(super) fn start(
            config: &TelemetryConfig,
            headers: Vec<(String, String)>,
        ) -> Result<Option<Self>, Issue> {
            let issue = |path: &str, message: String| Issue {
                path: path.into(),
                message,
            };
            let url = format!("{}/v1/traces", config.otlp_endpoint.trim_end_matches('/'));
            let mut map = reqwest::header::HeaderMap::new();
            for (name, value) in headers {
                let at = format!("telemetry.headers.{name}");
                let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| issue(&at, "is not a valid header name".into()))?;
                let mut header_value = reqwest::header::HeaderValue::from_str(&value)
                    .map_err(|_| issue(&at, "its value is not a valid header value".into()))?;
                header_value.set_sensitive(true);
                map.insert(header_name, header_value);
            }
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(EXPORT_TIMEOUT)
                .build()
                .map_err(|_| issue("telemetry", "cannot build the export client".into()))?;
            // No `Resource::builder()`: it reads OTEL_* variables, and the file is
            // the only source of settings.
            let resource = Resource::builder_empty()
                .with_service_name(
                    config
                        .service_name
                        .clone()
                        .unwrap_or_else(|| "llmshim".to_owned()),
                )
                .build();
            let exporter = Exporter {
                client,
                url,
                headers: map,
                resource: (&resource).into(),
            };
            let processor = Queue::start(exporter, QUEUE, BATCH, SCHEDULE)
                .map_err(|message| issue("telemetry", message))?;
            let ratio = config.sample_ratio.unwrap_or(1.0);
            let provider = SdkTracerProvider::builder()
                .with_span_processor(processor)
                .with_resource(resource)
                .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
                    ratio,
                ))))
                .build();
            let tracer = provider.tracer("llmshim");
            Ok(Some(Self {
                tracer,
                provider,
                requests: Arc::default(),
            }))
        }

        /// Start the span of one request. `inbound` is the request's headers when
        /// the caller was authenticated: only then is its `traceparent` honored,
        /// so a stranger cannot choose a trace id or force the sampler.
        pub fn request_span(
            &self,
            method: &str,
            route: &str,
            request_id: &str,
            caller: &SpanCaller,
            inbound: Option<&HeaderMap>,
        ) -> RequestSpan {
            let parent = match inbound {
                Some(headers) => {
                    use opentelemetry::propagation::TextMapPropagator;
                    TraceContextPropagator::new().extract(&HeaderExtractor(headers))
                }
                None => Context::new(),
            };
            let mut attributes = vec![
                KeyValue::new("http.request.method", bounded_text(method)),
                KeyValue::new("url.path", bounded_text(route)),
                KeyValue::new("llmshim.request_id", request_id.to_owned()),
            ];
            if let Some(tenant) = &caller.tenant {
                attributes.push(KeyValue::new("llmshim.tenant", tenant.clone()));
            }
            if let Some(key) = &caller.key {
                attributes.push(KeyValue::new("llmshim.key", key.clone()));
            }
            let span = self
                .tracer
                .span_builder("llmshim.request")
                .with_kind(SpanKind::Server)
                .with_attributes(attributes)
                .start_with_context(&self.tracer, &parent);
            let registered = inbound.is_some()
                && self
                    .requests
                    .register(request_id, span.span_context().clone());
            RequestSpan {
                span,
                id: registered.then(|| request_id.to_owned()),
                requests: self.requests.clone(),
            }
        }

        /// Send what is queued and stop the worker, within a bound. Call once,
        /// after the listeners have stopped.
        pub fn shutdown(&self) {
            let _ = self.provider.shutdown_with_timeout(SHUTDOWN);
        }
    }

    impl RequestSpan {
        /// End the span: with the response's status, or `None` for a request
        /// abandoned before a response existed (an explicit error status).
        pub fn end(mut self, status: Option<u16>) {
            match status {
                Some(code) => {
                    self.span
                        .set_attribute(KeyValue::new("http.response.status_code", i64::from(code)));
                    if code >= 500 {
                        self.span.set_status(Status::error(format!("HTTP {code}")));
                    }
                }
                None => self.span.set_status(Status::error("abandoned")),
            }
            self.span.end();
            if let Some(id) = &self.id {
                self.requests.ended(id);
            }
        }
    }

    /// One `llmshim.attempt` client span per admitted provider attempt, a child
    /// of its request's span when this replica holds it.
    struct Attempt {
        span: SdkSpan,
        /// The request whose registry entry this attempt holds open.
        held: Option<(String, Arc<Requests>)>,
    }

    impl Drop for Attempt {
        fn drop(&mut self) {
            if let Some((id, requests)) = self.held.take() {
                requests.end_attempt(&id);
            }
        }
    }

    impl AttemptSpans for Telemetry {
        fn start(&self, begun: &AttemptBegun<'_>) -> Box<dyn AttemptSpan> {
            let linked = begun
                .request_id
                .and_then(|id| self.requests.begin_attempt(id).map(|sc| (id, sc)));
            let held = linked
                .as_ref()
                .map(|(id, _)| ((*id).to_owned(), self.requests.clone()));
            let parent = linked.map_or_else(Context::new, |(_, sc)| {
                Context::new().with_remote_span_context(sc)
            });
            let mut attributes = vec![
                KeyValue::new("llmshim.provider", bounded_text(begun.provider)),
                KeyValue::new("llmshim.model", bounded_text(begun.model)),
                KeyValue::new("llmshim.attempt.kind", begun.kind),
            ];
            if let Some(id) = begun.request_id {
                attributes.push(KeyValue::new("llmshim.request_id", id.to_owned()));
            }
            let span = self
                .tracer
                .span_builder("llmshim.attempt")
                .with_kind(SpanKind::Client)
                .with_attributes(attributes)
                .start_with_context(&self.tracer, &parent);
            Box::new(Attempt { span, held })
        }
    }

    impl AttemptSpan for Attempt {
        fn end(mut self: Box<Self>, record: &UsageRecord) {
            let span = &mut self.span;
            span.set_attribute(KeyValue::new("llmshim.attempt.outcome", record.outcome));
            if let Some(status) = record.http_status {
                span.set_attribute(KeyValue::new(
                    "llmshim.upstream.status_code",
                    i64::from(status),
                ));
            }
            let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
            if record.usage_reported {
                span.set_attribute(KeyValue::new(
                    "llmshim.usage.input_tokens",
                    count(record.input_tokens),
                ));
                span.set_attribute(KeyValue::new(
                    "llmshim.usage.output_tokens",
                    count(record.output_tokens),
                ));
                if let Some(n) = record.reasoning_tokens {
                    span.set_attribute(KeyValue::new("llmshim.usage.reasoning_tokens", count(n)));
                }
                span.set_attribute(KeyValue::new(
                    "llmshim.usage.cache_read_tokens",
                    count(record.cache_read_tokens),
                ));
                span.set_attribute(KeyValue::new(
                    "llmshim.usage.cache_write_tokens",
                    count(record.cache_write_tokens),
                ));
            }
            // Unknown is absent, never zero.
            if let (Some(usd), Some(source)) = (record.cost_usd, &record.cost_source) {
                span.set_attribute(KeyValue::new("llmshim.cost_usd", usd));
                span.set_attribute(KeyValue::new("llmshim.cost_source", source.clone()));
            }
            if record.outcome != "completed" {
                span.set_status(Status::error(record.outcome));
            }
            span.end();
        }
    }

    /// At most 256 characters of a caller-controlled value.
    fn bounded_text(text: &str) -> String {
        text.chars().take(256).collect()
    }

    struct HeaderExtractor<'a>(&'a HeaderMap);

    impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).and_then(|value| value.to_str().ok())
        }
        fn keys(&self) -> Vec<&str> {
            self.0.keys().map(|name| name.as_str()).collect()
        }
    }

    /// Posts OTLP/HTTP protobuf to the collector.
    struct Exporter {
        client: reqwest::Client,
        url: String,
        headers: reqwest::header::HeaderMap,
        resource: opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema,
    }

    impl Exporter {
        async fn send(&self, batch: Vec<SpanData>) -> Result<(), String> {
            use prost::Message;
            let request = opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest {
                resource_spans: opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope(
                    batch,
                    &self.resource,
                ),
            };
            let response = self
                .client
                .post(&self.url)
                .headers(self.headers.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/x-protobuf")
                .body(request.encode_to_vec())
                .send()
                .await
                .map_err(|error| {
                    // Only the kind: a URL in an error text can carry a credential.
                    if error.is_timeout() {
                        "timed out".to_owned()
                    } else if error.is_connect() {
                        "could not connect".to_owned()
                    } else {
                        "request failed".to_owned()
                    }
                })?;
            if response.status().is_success() {
                Ok(())
            } else {
                Err(format!(
                    "the collector answered {}",
                    response.status().as_u16()
                ))
            }
        }
    }

    enum Message {
        Span(Box<SpanData>),
        Flush(mpsc::SyncSender<()>),
        Shutdown(mpsc::SyncSender<()>),
    }

    /// A bounded queue and the worker thread that drains it. Replaces the SDK's
    /// batch processor so that a drop is counted rather than silent.
    pub struct Queue {
        sender: mpsc::SyncSender<Message>,
        dropped: Arc<AtomicU64>,
        reported_drop: AtomicBool,
    }

    impl std::fmt::Debug for Queue {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Queue")
        }
    }

    impl Queue {
        fn start(
            exporter: Exporter,
            capacity: usize,
            batch: usize,
            schedule: Duration,
        ) -> Result<Self, String> {
            let (sender, receiver) = mpsc::sync_channel::<Message>(capacity);
            std::thread::Builder::new()
                .name("llmshim-otlp".into())
                .spawn(move || worker(exporter, receiver, batch, schedule))
                .map_err(|_| "cannot start the export worker thread".to_owned())?;
            Ok(Self {
                sender,
                dropped: Arc::new(AtomicU64::new(0)),
                reported_drop: AtomicBool::new(false),
            })
        }

        /// Ask the worker to flush or stop, and wait for it, within `wait` in all.
        /// A full queue (a stalled collector) is retried until the deadline and
        /// then given up on: shutdown never waits for a free slot unbounded.
        fn ask(&self, message: impl Fn(mpsc::SyncSender<()>) -> Message, wait: Duration) {
            let deadline = std::time::Instant::now() + wait;
            let (done, wait_for) = mpsc::sync_channel(1);
            loop {
                match self.sender.try_send(message(done.clone())) {
                    Ok(()) => {
                        let _ = wait_for.recv_timeout(
                            deadline.saturating_duration_since(std::time::Instant::now()),
                        );
                        return;
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => return,
                    Err(mpsc::TrySendError::Full(_)) => {
                        if std::time::Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }
    }

    impl SpanProcessor for Queue {
        fn on_start(&self, _span: &mut SdkSpan, _cx: &Context) {}

        fn on_end(&self, span: SpanData) {
            if !span.span_context.is_sampled() {
                return;
            }
            if let Err(error) = self.sender.try_send(Message::Span(Box::new(span))) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                crate::gateway::metrics::incr(SPANS_DROPPED, &[]);
                if !self.reported_drop.swap(true, Ordering::Relaxed) {
                    let why = match error {
                        mpsc::TrySendError::Full(_) => "the span queue is full",
                        mpsc::TrySendError::Disconnected(_) => "the export worker has stopped",
                    };
                    eprintln!(
                        "telemetry: {why}; spans are being dropped (counted in {SPANS_DROPPED})"
                    );
                }
            }
        }

        fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
            self.ask(Message::Flush, SHUTDOWN);
            Ok(())
        }

        fn shutdown_with_timeout(
            &self,
            timeout: Duration,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            self.ask(Message::Shutdown, timeout);
            Ok(())
        }
    }

    fn worker(
        exporter: Exporter,
        receiver: mpsc::Receiver<Message>,
        batch: usize,
        schedule: Duration,
    ) {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => {
                eprintln!("telemetry: cannot start the export runtime; spans will be dropped");
                return;
            }
        };
        let failed = AtomicBool::new(false);
        let mut pending: Vec<SpanData> = Vec::new();
        let export = |pending: &mut Vec<SpanData>| {
            if pending.is_empty() {
                return;
            }
            let spans = std::mem::take(pending);
            if let Err(reason) = runtime.block_on(exporter.send(spans)) {
                crate::gateway::metrics::incr(EXPORT_FAILURES, &[]);
                if !failed.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "telemetry: export failed ({reason}); further failures are counted in \
                         {EXPORT_FAILURES}"
                    );
                }
            }
        };
        loop {
            match receiver.recv_timeout(schedule) {
                Ok(Message::Span(span)) => {
                    pending.push(*span);
                    if pending.len() >= batch {
                        export(&mut pending);
                    }
                }
                Ok(Message::Flush(done)) => {
                    export(&mut pending);
                    let _ = done.send(());
                }
                Ok(Message::Shutdown(done)) => {
                    export(&mut pending);
                    let _ = done.send(());
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => export(&mut pending),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    export(&mut pending);
                    return;
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn context(span: u64) -> opentelemetry::trace::SpanContext {
            use opentelemetry::trace::{SpanId, TraceFlags, TraceId, TraceState};
            opentelemetry::trace::SpanContext::new(
                TraceId::from(1u128),
                SpanId::from(span),
                TraceFlags::SAMPLED,
                false,
                TraceState::default(),
            )
        }

        #[test]
        fn the_first_registration_of_an_id_wins_and_a_loser_cannot_end_it() {
            let requests = Requests::default();
            assert!(requests.register("a", context(1)));
            assert!(
                !requests.register("a", context(2)),
                "a duplicate is refused"
            );
            let held = requests.begin_attempt("a").unwrap();
            assert_eq!(held.span_id(), context(1).span_id());
        }

        #[test]
        fn an_entry_goes_when_its_last_attempt_ends_after_the_request() {
            let requests = Requests::default();
            requests.register("a", context(1));
            requests.begin_attempt("a").unwrap();
            requests.begin_attempt("a").unwrap();
            requests.ended("a");
            requests.end_attempt("a");
            assert!(requests.0.lock().unwrap().map.contains_key("a"));
            requests.end_attempt("a");
            assert!(requests.0.lock().unwrap().map.is_empty());
        }

        #[test]
        fn a_request_that_had_attempts_leaves_when_it_ends_and_one_without_waits() {
            let requests = Requests::default();
            requests.register("served", context(1));
            requests.begin_attempt("served").unwrap();
            requests.end_attempt("served");
            requests.ended("served");
            requests.register("refused", context(2));
            requests.ended("refused");
            let reg = requests.0.lock().unwrap();
            assert!(!reg.map.contains_key("served"));
            assert!(reg.map.contains_key("refused"), "kept for a late stream");
            assert_eq!(reg.lingering.len(), 1);
        }

        #[test]
        fn the_registry_is_bounded() {
            let requests = Requests::default();
            for n in 0..REGISTRY {
                assert!(requests.register(&format!("r{n}"), context(1)));
            }
            assert!(!requests.register("one-more", context(1)));
        }
        /// A collector that accepts connections and never answers.
        fn stalled_collector() -> String {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                let mut held = Vec::new();
                for socket in listener.incoming().flatten() {
                    held.push(socket);
                }
            });
            format!("http://{address}/v1/traces")
        }

        #[test]
        fn a_full_queue_drops_spans_counts_them_and_never_blocks_the_caller() {
            let resource = Resource::builder_empty().with_service_name("t").build();
            let exporter = Exporter {
                client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .unwrap(),
                url: stalled_collector(),
                headers: reqwest::header::HeaderMap::new(),
                resource: (&resource).into(),
            };
            // Capacity 1 and a batch of 1: the worker takes the first span and
            // stalls inside the export, the queue holds one more, the rest drop.
            let queue = Queue::start(exporter, 1, 1, Duration::from_secs(60)).unwrap();
            let dropped = queue.dropped.clone();
            let provider = SdkTracerProvider::builder()
                .with_span_processor(queue)
                .with_resource(resource)
                .build();
            let tracer = provider.tracer("t");
            let started = std::time::Instant::now();
            for _ in 0..200 {
                tracer.start("llmshim.request").end();
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "ending a span never waits for the exporter: {:?}",
                started.elapsed()
            );
            assert!(
                dropped.load(Ordering::Relaxed) >= 190,
                "dropped {}",
                dropped.load(Ordering::Relaxed)
            );
            // Dropping the provider would wait for the stalled export; the
            // process ends the test instead.
            std::mem::forget(provider);
        }
    }
}

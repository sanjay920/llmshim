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
}

#[cfg(feature = "serve-otel")]
mod real {
    use super::*;
    use opentelemetry::trace::{Span as _, SpanKind, Status, Tracer as _, TracerProvider as _};
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
    /// How long shutdown waits for what is queued.
    const SHUTDOWN: Duration = Duration::from_secs(5);

    pub const SPANS_DROPPED: &str = "llmshim_telemetry_spans_dropped_total";
    pub const EXPORT_FAILURES: &str = "llmshim_telemetry_export_failures_total";

    pub struct Telemetry {
        tracer: SdkTracer,
        provider: SdkTracerProvider,
    }

    pub struct RequestSpan(SdkSpan);

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
            let processor = Queue::start(exporter, QUEUE, BATCH, SCHEDULE);
            let ratio = config.sample_ratio.unwrap_or(1.0);
            let provider = SdkTracerProvider::builder()
                .with_span_processor(processor)
                .with_resource(resource)
                .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
                    ratio,
                ))))
                .build();
            let tracer = provider.tracer("llmshim");
            Ok(Some(Self { tracer, provider }))
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
                KeyValue::new("http.request.method", method.to_owned()),
                KeyValue::new("http.route", route.to_owned()),
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
            RequestSpan(span)
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
                    self.0
                        .set_attribute(KeyValue::new("http.response.status_code", i64::from(code)));
                    if code >= 500 {
                        self.0.set_status(Status::error(format!("HTTP {code}")));
                    }
                }
                None => self.0.set_status(Status::error("abandoned")),
            }
            self.0.end();
        }
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
        fn start(exporter: Exporter, capacity: usize, batch: usize, schedule: Duration) -> Self {
            let (sender, receiver) = mpsc::sync_channel::<Message>(capacity);
            std::thread::Builder::new()
                .name("llmshim-otlp".into())
                .spawn(move || worker(exporter, receiver, batch, schedule))
                .expect("the telemetry worker thread starts");
            Self {
                sender,
                dropped: Arc::new(AtomicU64::new(0)),
                reported_drop: AtomicBool::new(false),
            }
        }

        fn ask(&self, message: impl FnOnce(mpsc::SyncSender<()>) -> Message, wait: Duration) {
            let (done, wait_for) = mpsc::sync_channel(1);
            if self.sender.send(message(done)).is_ok() {
                let _ = wait_for.recv_timeout(wait);
            }
        }
    }

    impl SpanProcessor for Queue {
        fn on_start(&self, _span: &mut SdkSpan, _cx: &Context) {}

        fn on_end(&self, span: SpanData) {
            if !span.span_context.is_sampled() {
                return;
            }
            if self.sender.try_send(Message::Span(Box::new(span))).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                crate::gateway::metrics::incr(SPANS_DROPPED, &[]);
                if !self.reported_drop.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "telemetry: the span queue is full; spans are being dropped \
                         (counted in {SPANS_DROPPED})"
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
            Err(_) => return,
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
            let queue = Queue::start(exporter, 1, 1, Duration::from_secs(60));
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

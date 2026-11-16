//! `llmshim serve` exporting traces (0016), against a local OTLP/HTTP receiver.
//!
//! Spans leave in batches, on a timer or at shutdown, so the tests that read them
//! stop the server with SIGTERM first: shutdown flushes what is queued.
#![cfg(all(feature = "serve-otel", unix))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{
    chat, chat_with_request_id, completion, config, exit_status_within, start, terminate,
    ADMIN_TOKEN, CALLER_KEY, UPSTREAM_KEY,
};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value as AnyValue;
use opentelemetry_proto::tonic::trace::v1::Span;
use prost::Message;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OTLP_TOKEN: &str = "otlp-collector-token-0123456789";

/// The only attribute keys a request span may carry (0016, decision 6).
const CLOSED_ATTRIBUTES: &[&str] = &[
    "http.request.method",
    "http.route",
    "http.response.status_code",
    "llmshim.request_id",
    "llmshim.tenant",
    "llmshim.key",
];

/// One request the receiver got: its headers and its body.
type Request = (Vec<(String, String)>, Vec<u8>);

#[derive(Clone, Default)]
struct Received(Arc<Mutex<Vec<Request>>>);

impl Received {
    fn bodies(&self) -> Vec<Vec<u8>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| b.clone())
            .collect()
    }

    fn spans(&self) -> Vec<Span> {
        self.bodies()
            .iter()
            .flat_map(|body| {
                ExportTraceServiceRequest::decode(body.as_slice())
                    .unwrap()
                    .resource_spans
            })
            .flat_map(|resource| resource.scope_spans)
            .flat_map(|scope| scope.spans)
            .collect()
    }

    fn header(&self, name: &str) -> Option<String> {
        self.0.lock().unwrap().iter().find_map(|(headers, _)| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
    }
}

/// An OTLP/HTTP receiver that keeps what it is sent.
async fn receiver() -> (String, Received) {
    use axum::routing::post;
    let received = Received::default();
    let sink = received.clone();
    let app = axum::Router::new().route(
        "/v1/traces",
        post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let sink = sink.clone();
                async move {
                    let headers = headers
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.to_string(),
                                value.to_str().unwrap_or_default().to_owned(),
                            )
                        })
                        .collect();
                    sink.0.lock().unwrap().push((headers, body.to_vec()));
                    axum::http::StatusCode::OK
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), received)
}

fn telemetry_table(endpoint: &str, extra: &str) -> String {
    format!(
        "[telemetry]\notlp_endpoint = \"{endpoint}\"\n{extra}\n\
         [telemetry.headers]\nauthorization = {{ env = \"SERVE_E2E_OTLP_TOKEN\" }}\n"
    )
}

fn attributes(span: &Span) -> Vec<(String, String)> {
    span.attributes
        .iter()
        .map(|kv| {
            let value = match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                Some(AnyValue::StringValue(text)) => text.clone(),
                Some(AnyValue::IntValue(number)) => number.to_string(),
                other => format!("{other:?}"),
            };
            (kv.key.clone(), value)
        })
        .collect()
}

fn attribute(span: &Span, key: &str) -> Option<String> {
    attributes(span)
        .into_iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

async fn stop(mut server: harness::Server) {
    terminate(&server);
    let status = exit_status_within(&mut server, Duration::from_secs(20))
        .await
        .expect("the server exits after SIGTERM");
    assert_eq!(status.code(), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_span_is_exported_with_a_closed_attribute_set_and_no_secret() {
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(1)
        .create_async()
        .await;
    let (endpoint, received) = receiver().await;
    let server = start(
        &config(
            &upstream.url(),
            &telemetry_table(&endpoint, "service_name = \"edge\""),
        ),
        "otel-span",
        &[("SERVE_E2E_OTLP_TOKEN", OTLP_TOKEN)],
    );

    // An authenticated caller joining its own trace, and a stranger who claims one.
    let (trace, parent) = ("0af7651916cd43dd8448eb211c80319c", "b7ad6b7169203331");
    let traceparent = format!("00-{trace}-{parent}-01");
    let client = reqwest::Client::new();
    let authed = client
        .post(format!("{}/v1/chat/completions", server.base))
        .bearer_auth(CALLER_KEY)
        .header("traceparent", &traceparent)
        .header("x-request-id", "run-5.step-1")
        .json(&serde_json::json!({"model": "inference/test-model",
            "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), 200);
    let stranger = client
        .post(format!("{}/v1/chat/completions", server.base))
        .bearer_auth("a-wrong-token-0123456789")
        .header("traceparent", &traceparent)
        .json(&serde_json::json!({"model": "inference/test-model", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(stranger.status(), 401);
    let stranger_id = stranger.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    stop(server).await;

    assert_eq!(
        received.header("authorization").as_deref(),
        Some(OTLP_TOKEN),
        "the configured header reaches the collector"
    );
    let spans = received.spans();
    assert_eq!(spans.len(), 2, "one span per request: {spans:#?}");
    let by_id = |id: &str| {
        spans
            .iter()
            .find(|span| attribute(span, "llmshim.request_id").as_deref() == Some(id))
            .unwrap_or_else(|| panic!("no span for {id}"))
    };

    let ok = by_id("run-5.step-1");
    assert_eq!(ok.name, "llmshim.request");
    assert_eq!(
        hex(&ok.trace_id),
        trace,
        "joins the authenticated caller's trace"
    );
    assert_eq!(hex(&ok.parent_span_id), parent);
    assert_eq!(attribute(ok, "llmshim.tenant").as_deref(), Some("platform"));
    assert_eq!(attribute(ok, "llmshim.key").as_deref(), Some("ci"));
    assert_eq!(
        attribute(ok, "http.response.status_code").as_deref(),
        Some("200")
    );
    assert_eq!(
        attribute(ok, "http.route").as_deref(),
        Some("/v1/chat/completions")
    );

    let refused = by_id(&stranger_id);
    assert_ne!(
        hex(&refused.trace_id),
        trace,
        "a stranger cannot choose a trace id"
    );
    assert!(
        refused.parent_span_id.is_empty(),
        "and has no remote parent"
    );
    assert_eq!(
        attribute(refused, "http.response.status_code").as_deref(),
        Some("401")
    );
    assert!(attribute(refused, "llmshim.tenant").is_none());

    // The attribute set is closed.
    for span in &spans {
        for (key, _) in attributes(span) {
            assert!(
                CLOSED_ATTRIBUTES.contains(&key.as_str()),
                "unexpected attribute {key}"
            );
        }
    }
    // And nothing secret or private is in anything that was sent.
    for body in received.bodies() {
        for forbidden in [
            CALLER_KEY,
            UPSTREAM_KEY,
            OTLP_TOKEN,
            "ping",
            "pong",
            "a-wrong-token",
        ] {
            assert!(
                !body
                    .windows(forbidden.len())
                    .any(|window| window == forbidden.as_bytes()),
                "{forbidden} leaked into a span"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sample_ratio_of_zero_exports_nothing_for_a_new_trace() {
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect(1)
        .create_async()
        .await;
    let (endpoint, received) = receiver().await;
    let server = start(
        &config(
            &upstream.url(),
            &telemetry_table(&endpoint, "sample_ratio = 0.0"),
        ),
        "otel-ratio",
        &[("SERVE_E2E_OTLP_TOKEN", OTLP_TOKEN)],
    );
    assert_eq!(
        chat(&server, Some(CALLER_KEY), "inference/test-model")
            .await
            .status(),
        200
    );
    stop(server).await;
    assert!(received.spans().is_empty(), "{:#?}", received.spans());
}

/// A collector that is not there must not touch a request, and the failure is
/// counted where an operator looks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_collector_that_is_down_never_touches_a_request_and_is_counted() {
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect_at_least(1)
        .create_async()
        .await;
    let server = start(
        &config(
            &upstream.url(),
            &format!(
                "[admin]\nport = 0\ntoken = {{ env = \"SERVE_E2E_ADMIN_TOKEN\" }}\n{}",
                telemetry_table("http://127.0.0.1:1", "")
            ),
        ),
        "otel-down",
        &[("SERVE_E2E_OTLP_TOKEN", OTLP_TOKEN)],
    );
    let admin = server.admin.clone().unwrap();
    let started = std::time::Instant::now();
    for _ in 0..3 {
        let response =
            chat_with_request_id(&server, Some(CALLER_KEY), "inference/test-model", None).await;
        assert_eq!(response.status(), 200);
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "requests do not wait for the exporter"
    );

    // The batch leaves on a timer (5 s); the failure is a /metrics series.
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let text = loop {
        let text = client
            .get(format!("{admin}/metrics"))
            .bearer_auth(ADMIN_TOKEN)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if text.contains("llmshim_telemetry_export_failures_total")
            || std::time::Instant::now() > deadline
        {
            break text;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert!(
        text.contains("llmshim_telemetry_export_failures_total"),
        "{text}"
    );
    assert!(!text.contains(OTLP_TOKEN));
    assert_eq!(
        chat(&server, Some(CALLER_KEY), "inference/test-model")
            .await
            .status(),
        200
    );
    stop(server).await;
}

/// A collector that accepts and never answers must not hold a request, and must
/// not hold shutdown past its bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_collector_that_stalls_never_holds_a_request_or_shutdown() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let held = Arc::new(Mutex::new(Vec::new()));
    let keep = held.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            keep.lock().unwrap().push(socket); // never read, never answered
        }
    });
    let mut upstream = mockito::Server::new_async().await;
    let _mock = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_body(completion("pong"))
        .expect_at_least(1)
        .create_async()
        .await;
    let server = start(
        &config(&upstream.url(), &telemetry_table(&endpoint, "")),
        "otel-stall",
        &[("SERVE_E2E_OTLP_TOKEN", OTLP_TOKEN)],
    );
    let started = std::time::Instant::now();
    for _ in 0..5 {
        assert_eq!(
            chat(&server, Some(CALLER_KEY), "inference/test-model")
                .await
                .status(),
            200
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    // Shutdown flushes, the flush stalls, and the bound ends it: the process still
    // exits cleanly within the shutdown bound (5 s) plus the drain.
    let began = std::time::Instant::now();
    stop(server).await;
    assert!(
        began.elapsed() < Duration::from_secs(15),
        "{:?}",
        began.elapsed()
    );
    drop(held);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

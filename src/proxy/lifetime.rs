use super::ratelimit::Backpressure;
use super::wire::{self, Wire};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::json;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

const DEFAULT_UNARY_LOGICAL_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);
const DEFAULT_STREAM_LOGICAL_LIFETIME: Duration = Duration::from_secs(6 * 60 * 60);
const PUMP_CHUNK_BYTES: usize = 16 * 1024;
const LOGICAL_TIMEOUT_MESSAGE: &str = "Request exceeded the configured logical lifetime";

#[derive(Clone, Copy, Debug)]
pub(crate) struct LogicalRequestDeadlines {
    unary: Duration,
    stream: Duration,
}

impl LogicalRequestDeadlines {
    pub(crate) fn from_env() -> Self {
        Self {
            unary: configured_duration(
                "LLMSHIM_PROXY_UNARY_TIMEOUT_MS",
                DEFAULT_UNARY_LOGICAL_LIFETIME,
            ),
            stream: configured_duration(
                "LLMSHIM_PROXY_STREAM_TIMEOUT_MS",
                DEFAULT_STREAM_LOGICAL_LIFETIME,
            ),
        }
    }

    #[cfg(feature = "gateway")]
    pub(crate) fn gateway_from_env() -> Self {
        Self {
            unary: configured_duration(
                "LLMSHIM_GATEWAY_UNARY_JOB_TIMEOUT_MS",
                DEFAULT_UNARY_LOGICAL_LIFETIME,
            ),
            stream: configured_duration(
                "LLMSHIM_GATEWAY_STREAM_JOB_TIMEOUT_MS",
                DEFAULT_STREAM_LOGICAL_LIFETIME,
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn new(unary: Duration, stream: Duration) -> Self {
        assert!(!unary.is_zero());
        assert!(!stream.is_zero());
        Self { unary, stream }
    }
}

fn configured_duration(name: &str, default: Duration) -> Duration {
    let Some(milliseconds) = std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
    else {
        return default;
    };
    let configured = Duration::from_millis(milliseconds);
    if Instant::now().checked_add(configured).is_some() {
        configured
    } else {
        default
    }
}

#[derive(Clone)]
pub(crate) struct PreparationAdmissionState {
    backpressure: Backpressure,
    deadlines: LogicalRequestDeadlines,
}

impl PreparationAdmissionState {
    pub(crate) fn new(backpressure: Backpressure, deadlines: LogicalRequestDeadlines) -> Self {
        Self {
            backpressure,
            deadlines,
        }
    }
}

#[derive(Clone)]
pub(crate) struct LogicalRequestLifetime {
    inner: Arc<LogicalRequestLifetimeInner>,
}

struct LogicalRequestLifetimeInner {
    stream_deadline: Instant,
    deadline_sender: watch::Sender<Instant>,
}

impl LogicalRequestLifetime {
    pub(crate) fn new(path: &str, deadlines: LogicalRequestDeadlines) -> Self {
        let admitted_at = Instant::now();
        let unary_deadline = admitted_at
            .checked_add(deadlines.unary)
            .unwrap_or_else(|| admitted_at + DEFAULT_UNARY_LOGICAL_LIFETIME);
        let stream_deadline = admitted_at
            .checked_add(deadlines.stream)
            .unwrap_or_else(|| admitted_at + DEFAULT_STREAM_LOGICAL_LIFETIME);
        let initial_deadline = if path == "/v1/chat/stream" {
            stream_deadline
        } else {
            unary_deadline
        };
        let (deadline_sender, _) = watch::channel(initial_deadline);
        Self {
            inner: Arc::new(LogicalRequestLifetimeInner {
                stream_deadline,
                deadline_sender,
            }),
        }
    }

    pub(crate) fn select_streaming(&self, streaming: bool) -> Result<(), ()> {
        let current_deadline = *self.inner.deadline_sender.borrow();
        let selected_deadline = if streaming {
            self.inner.stream_deadline
        } else {
            current_deadline
        };
        let now = Instant::now();
        if now >= current_deadline {
            return Err(());
        }
        if selected_deadline != current_deadline {
            self.inner.deadline_sender.send_replace(selected_deadline);
        }
        if now >= selected_deadline {
            return Err(());
        }
        Ok(())
    }

    pub(crate) fn deadline(&self) -> Instant {
        *self.inner.deadline_sender.borrow()
    }

    pub(crate) fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline()
    }

    pub(crate) async fn expired(&self) {
        let mut deadline_receiver = self.inner.deadline_sender.subscribe();
        loop {
            let deadline = *deadline_receiver.borrow_and_update();
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => return,
                changed = deadline_receiver.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

pub(crate) async fn admit_preparation(
    State(state): State<PreparationAdmissionState>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    let requires_preparation = request.method() == axum::http::Method::POST
        && matches!(
            path.as_str(),
            "/v1/chat" | "/v1/chat/stream" | "/v1/chat/completions" | "/v1/messages"
        );
    if !requires_preparation {
        return next.run(request).await;
    }

    let preparation_permit = match state.backpressure.acquire_preparation().await {
        Ok(permit) => permit,
        Err(()) => {
            return super::preparation_overload_response(&path, state.backpressure.queue_timeout())
        }
    };
    let lifetime = LogicalRequestLifetime::new(&path, state.deadlines);
    request.extensions_mut().insert(lifetime.clone());
    let response_future = next.run(request);
    tokio::pin!(response_future);
    let response = tokio::select! {
        biased;
        _ = lifetime.expired() => return timeout_response(&path),
        response = &mut response_future => response,
    };
    if lifetime.is_expired() {
        drop(response);
        drop(preparation_permit);
        return timeout_response(&path);
    }
    pump_response_with_owner(response, path, lifetime, preparation_permit)
}

pub(crate) fn timeout_response(path: &str) -> Response {
    match path {
        "/v1/chat/completions" => wire::fail(
            Wire::Chat,
            StatusCode::GATEWAY_TIMEOUT,
            LOGICAL_TIMEOUT_MESSAGE,
        ),
        "/v1/messages" => wire::fail(
            Wire::Messages,
            StatusCode::GATEWAY_TIMEOUT,
            LOGICAL_TIMEOUT_MESSAGE,
        ),
        _ => (
            StatusCode::GATEWAY_TIMEOUT,
            axum::Json(json!({
                "error": {
                    "code": "request_timeout",
                    "message": LOGICAL_TIMEOUT_MESSAGE,
                }
            })),
        )
            .into_response(),
    }
}

#[cfg(feature = "gateway")]
pub(crate) fn pump_response(
    response: Response,
    path: String,
    lifetime: LogicalRequestLifetime,
) -> Response {
    pump_response_with_owner(response, path, lifetime, ())
}

fn pump_response_with_owner<Owner>(
    response: Response,
    path: String,
    lifetime: LogicalRequestLifetime,
    owner: Owner,
) -> Response
where
    Owner: Send + 'static,
{
    let is_sse = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let (parts, body) = response.into_parts();
    let (frame_sender, frame_receiver) = mpsc::channel(1);
    let terminal = Arc::new(Mutex::new(None));
    let producer_terminal = terminal.clone();
    let deadline = lifetime.deadline();
    tokio::spawn(async move {
        run_body_pump(body, frame_sender, producer_terminal, deadline, owner).await;
    });
    let pumped = PumpedBody {
        receiver: frame_receiver,
        terminal,
        timeout_frame: is_sse.then(|| timeout_sse_frame(&path)),
        sse_boundary: SseBoundary::default(),
        finished: false,
    };
    Response::from_parts(parts, Body::from_stream(pumped))
}

struct PumpFrame {
    bytes: Bytes,
    acknowledged: oneshot::Sender<()>,
}

enum PumpTerminal {
    Timeout,
    BodyError(String),
}

async fn run_body_pump<Owner>(
    body: Body,
    frame_sender: mpsc::Sender<PumpFrame>,
    terminal: Arc<Mutex<Option<PumpTerminal>>>,
    deadline: Instant,
    _owner: Owner,
) where
    Owner: Send + 'static,
{
    let mut source = body.into_data_stream();
    loop {
        let source_item = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                set_terminal(&terminal, PumpTerminal::Timeout);
                return;
            }
            _ = frame_sender.closed() => return,
            item = source.next() => item,
        };
        let Some(source_item) = source_item else {
            return;
        };
        let source_bytes = match source_item {
            Ok(bytes) => bytes,
            Err(error) => {
                set_terminal(&terminal, PumpTerminal::BodyError(error.to_string()));
                return;
            }
        };
        for source_chunk in source_bytes.chunks(PUMP_CHUNK_BYTES) {
            let owned_chunk = Bytes::copy_from_slice(source_chunk);
            let (acknowledgement_sender, acknowledgement_receiver) = oneshot::channel();
            let frame = PumpFrame {
                bytes: owned_chunk,
                acknowledged: acknowledgement_sender,
            };
            let sent = tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    set_terminal(&terminal, PumpTerminal::Timeout);
                    return;
                }
                _ = frame_sender.closed() => return,
                result = frame_sender.send(frame) => result.is_ok(),
            };
            if !sent {
                return;
            }
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    set_terminal(&terminal, PumpTerminal::Timeout);
                    return;
                }
                _ = frame_sender.closed() => return,
                result = acknowledgement_receiver => {
                    if result.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

fn set_terminal(terminal: &Mutex<Option<PumpTerminal>>, value: PumpTerminal) {
    let mut slot = terminal
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        *slot = Some(value);
    }
}

struct PumpedBody {
    receiver: mpsc::Receiver<PumpFrame>,
    terminal: Arc<Mutex<Option<PumpTerminal>>>,
    timeout_frame: Option<Bytes>,
    sse_boundary: SseBoundary,
    finished: bool,
}

impl Stream for PumpedBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.receiver).poll_recv(context) {
            Poll::Ready(Some(frame)) => {
                self.sse_boundary.observe(&frame.bytes);
                let _ = frame.acknowledged.send(());
                Poll::Ready(Some(Ok(frame.bytes)))
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                self.finished = true;
                let terminal = self
                    .terminal
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                match terminal {
                    Some(PumpTerminal::Timeout)
                        if self.timeout_frame.is_some() && self.sse_boundary.is_safe() =>
                    {
                        Poll::Ready(Some(Ok(self.timeout_frame.take().unwrap())))
                    }
                    Some(PumpTerminal::Timeout) => Poll::Ready(Some(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        LOGICAL_TIMEOUT_MESSAGE,
                    )))),
                    Some(PumpTerminal::BodyError(message)) => {
                        Poll::Ready(Some(Err(std::io::Error::other(message))))
                    }
                    None => Poll::Ready(None),
                }
            }
        }
    }
}

struct SseBoundary {
    at_line_start: bool,
    after_carriage_return: bool,
    safe: bool,
    observed_anything: bool,
}

impl Default for SseBoundary {
    fn default() -> Self {
        Self {
            at_line_start: true,
            after_carriage_return: false,
            safe: true,
            observed_anything: false,
        }
    }
}

impl SseBoundary {
    fn observe(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.observed_anything = true;
            if self.after_carriage_return && *byte == b'\n' {
                self.after_carriage_return = false;
                continue;
            }
            self.after_carriage_return = false;
            if *byte == b'\r' || *byte == b'\n' {
                self.safe = self.at_line_start;
                self.at_line_start = true;
                self.after_carriage_return = *byte == b'\r';
            } else {
                self.safe = false;
                self.at_line_start = false;
            }
        }
    }

    fn is_safe(&self) -> bool {
        !self.observed_anything || self.safe
    }
}

fn timeout_sse_frame(path: &str) -> Bytes {
    let frame = match path {
        "/v1/chat/completions" => format!(
            "data: {}\n\n",
            json!({
                "error": {
                    "message": LOGICAL_TIMEOUT_MESSAGE,
                    "type": "api_error",
                    "param": null,
                    "code": "request_timeout",
                }
            })
        ),
        "/v1/messages" => format!(
            "event: error\ndata: {}\n\n",
            json!({
                "type": "error",
                "error": {
                    "type": "api_error",
                    "message": LOGICAL_TIMEOUT_MESSAGE,
                }
            })
        ),
        _ => format!(
            "event: error\ndata: {}\n\n",
            json!({
                "type": "error",
                "message": LOGICAL_TIMEOUT_MESSAGE,
                "error": {
                    "message": LOGICAL_TIMEOUT_MESSAGE,
                    "code": "request_timeout",
                    "status": StatusCode::GATEWAY_TIMEOUT.as_u16(),
                }
            })
        ),
    };
    Bytes::from(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct PendingBody {
        dropped: Arc<AtomicBool>,
    }

    impl Stream for PendingBody {
        type Item = Result<Bytes, Infallible>;

        fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Drop for PendingBody {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    fn test_lifetime(duration: Duration) -> LogicalRequestLifetime {
        LogicalRequestLifetime::new(
            "/v1/chat/stream",
            LogicalRequestDeadlines::new(duration, duration),
        )
    }

    async fn preparation_permit(backpressure: &Backpressure) -> tokio::sync::OwnedSemaphorePermit {
        backpressure
            .acquire_preparation()
            .await
            .expect("test preparation permit")
    }

    #[tokio::test(start_paused = true)]
    async fn expired_unknown_mode_cannot_be_revived_as_streaming() {
        let admitted_at = Instant::now();
        let lifetime = LogicalRequestLifetime::new(
            "/v1/chat",
            LogicalRequestDeadlines::new(Duration::from_secs(2), Duration::from_secs(6)),
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(lifetime.select_streaming(true).is_err());
        assert_eq!(lifetime.deadline(), admitted_at + Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn expired_shorter_stream_mode_clamps_the_effective_deadline() {
        let admitted_at = Instant::now();
        let lifetime = LogicalRequestLifetime::new(
            "/v1/chat",
            LogicalRequestDeadlines::new(Duration::from_secs(6), Duration::from_secs(2)),
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(lifetime.select_streaming(true).is_err());
        assert_eq!(lifetime.deadline(), admitted_at + Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn live_shorter_stream_mode_selects_its_earlier_deadline() {
        let admitted_at = Instant::now();
        let lifetime = LogicalRequestLifetime::new(
            "/v1/chat",
            LogicalRequestDeadlines::new(Duration::from_secs(6), Duration::from_secs(2)),
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(lifetime.select_streaming(true).is_ok());
        assert_eq!(lifetime.deadline(), admitted_at + Duration::from_secs(2));
    }

    #[test]
    fn sse_boundary_requires_a_complete_blank_line() {
        let mut boundary = SseBoundary::default();
        boundary.observe(b"data: first\n");
        assert!(!boundary.is_safe());
        boundary.observe(b"\n");
        assert!(boundary.is_safe());
        boundary.observe(b"data: partial");
        assert!(!boundary.is_safe());
        boundary.observe(b"\r\n\r\n");
        assert!(boundary.is_safe());
    }

    #[tokio::test(start_paused = true)]
    async fn unpolled_sse_drops_source_and_releases_permit_at_deadline() {
        let dropped = Arc::new(AtomicBool::new(false));
        let source = PendingBody {
            dropped: dropped.clone(),
        };
        let response = (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(source),
        )
            .into_response();
        let backpressure = Backpressure::new(1, Duration::from_secs(1));
        let pumped = pump_response_with_owner(
            response,
            "/v1/chat/stream".into(),
            test_lifetime(Duration::from_secs(5)),
            preparation_permit(&backpressure).await,
        );

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert!(dropped.load(Ordering::Acquire));
        let second_permit = preparation_permit(&backpressure).await;
        drop(second_permit);
        drop(pumped);
    }

    #[tokio::test(start_paused = true)]
    async fn unpolled_unary_drops_source_and_releases_permit_at_deadline() {
        let dropped = Arc::new(AtomicBool::new(false));
        let response = (
            [(header::CONTENT_TYPE, "application/json")],
            Body::from_stream(PendingBody {
                dropped: dropped.clone(),
            }),
        )
            .into_response();
        let backpressure = Backpressure::new(1, Duration::from_secs(1));
        let pumped = pump_response_with_owner(
            response,
            "/v1/chat".into(),
            test_lifetime(Duration::from_secs(5)),
            preparation_permit(&backpressure).await,
        );

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert!(dropped.load(Ordering::Acquire));
        drop(preparation_permit(&backpressure).await);
        drop(pumped);
    }

    #[tokio::test(start_paused = true)]
    async fn normal_body_is_preserved_and_permit_follows_the_last_owned_chunk() {
        let backpressure = Backpressure::new(1, Duration::from_secs(1));
        let response = (
            [(header::CONTENT_TYPE, "application/json")],
            Body::from(r#"{"answer":"ok"}"#),
        )
            .into_response();
        let pumped = pump_response_with_owner(
            response,
            "/v1/chat".into(),
            test_lifetime(Duration::from_secs(30)),
            preparation_permit(&backpressure).await,
        );
        tokio::task::yield_now().await;
        let blocked_backpressure = backpressure.clone();
        let unavailable = tokio::spawn(async move {
            tokio::time::timeout(
                Duration::from_millis(10),
                blocked_backpressure.acquire_preparation(),
            )
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(10)).await;
        assert!(unavailable.await.unwrap().is_err());

        let bytes = axum::body::to_bytes(pumped.into_body(), 1024)
            .await
            .expect("normal body");
        assert_eq!(bytes, r#"{"answer":"ok"}"#);
        drop(preparation_permit(&backpressure).await);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_partial_consumer_resumes_before_deadline() {
        let payload = vec![b'x'; PUMP_CHUNK_BYTES * 2 + 17];
        let expected = payload.clone();
        let response = Body::from(payload).into_response();
        let backpressure = Backpressure::new(1, Duration::from_secs(1));
        let pumped = pump_response_with_owner(
            response,
            "/v1/chat".into(),
            test_lifetime(Duration::from_secs(30)),
            preparation_permit(&backpressure).await,
        );
        let mut body = pumped.into_body().into_data_stream();
        let first = body.next().await.unwrap().unwrap();
        assert_eq!(first.len(), PUMP_CHUNK_BYTES);
        tokio::time::advance(Duration::from_secs(20)).await;
        let mut received = first.to_vec();
        while let Some(chunk) = body.next().await {
            received.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(received, expected);
        drop(preparation_permit(&backpressure).await);
    }

    #[tokio::test(start_paused = true)]
    async fn complete_sse_event_gets_route_correct_timeout_event() {
        for path in ["/v1/chat/stream", "/v1/chat/completions", "/v1/messages"] {
            let source = async_stream::stream! {
                yield Ok::<_, Infallible>(Bytes::from_static(b"event: content\ndata: {\"type\":\"content\",\"text\":\"ok\"}\n\n"));
                std::future::pending::<()>().await;
            };
            let response = (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(source),
            )
                .into_response();
            let backpressure = Backpressure::new(1, Duration::from_secs(1));
            let pumped = pump_response_with_owner(
                response,
                path.into(),
                test_lifetime(Duration::from_secs(5)),
                preparation_permit(&backpressure).await,
            );
            let mut events = crate::sse::data(pumped.into_body().into_data_stream());
            let first = events.next().await.unwrap().unwrap();
            assert!(first.contains("content"), "{path}");
            tokio::time::advance(Duration::from_secs(5)).await;
            let timeout = events.next().await.unwrap().unwrap();
            let timeout: serde_json::Value = serde_json::from_str(&timeout).unwrap();
            match path {
                "/v1/chat/stream" => {
                    assert_eq!(timeout["type"], "error");
                    assert_eq!(timeout["error"]["code"], "request_timeout");
                }
                "/v1/chat/completions" => {
                    assert_eq!(timeout["error"]["type"], "api_error");
                    assert_eq!(timeout["error"]["code"], "request_timeout");
                }
                "/v1/messages" => {
                    assert_eq!(timeout["type"], "error");
                    assert_eq!(timeout["error"]["type"], "api_error");
                }
                _ => unreachable!(),
            }
            assert!(events.next().await.is_none(), "{path}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_between_copied_sse_chunks_uses_transport_error() {
        let mut event = Vec::from(&b"data: \""[..]);
        event.extend(std::iter::repeat_n(b'x', PUMP_CHUNK_BYTES * 3));
        event.extend_from_slice(b"\"\n\n");
        let source = futures::stream::once(async move { Ok::<_, Infallible>(Bytes::from(event)) })
            .chain(futures::stream::pending());
        let response = (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(source),
        )
            .into_response();
        let backpressure = Backpressure::new(1, Duration::from_secs(1));
        let pumped = pump_response_with_owner(
            response,
            "/v1/chat/stream".into(),
            test_lifetime(Duration::from_secs(5)),
            preparation_permit(&backpressure).await,
        );
        let mut body = pumped.into_body().into_data_stream();
        let first = body.next().await.unwrap().unwrap();
        assert_eq!(first.len(), PUMP_CHUNK_BYTES);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        let source = futures::stream::once(async move { Ok::<_, axum::Error>(first) }).chain(body);
        let mut parsed = crate::sse::data(source);
        let error = parsed
            .next()
            .await
            .expect("body error")
            .expect_err("partial SSE must not receive an appended timeout event");
        assert!(error.to_string().contains("could not read upstream SSE"));
    }
}

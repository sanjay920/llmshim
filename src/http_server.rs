use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Sleep};

const DEFAULT_MAX_CONNECTIONS: usize = 1_024;
const DEFAULT_HEADER_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HEADER_BYTES: usize = 64 * 1_024;
const MAX_HEADER_COUNT: usize = 100;
const MAX_HTTP2_STREAMS: u32 = 128;

#[derive(Clone, Copy)]
struct HttpServerLimits {
    maximum_connections: usize,
    header_timeout: Duration,
}

impl HttpServerLimits {
    fn from_env() -> Self {
        let maximum_connections = std::env::var("LLMSHIM_HTTP_MAX_CONNECTIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0 && *value <= Semaphore::MAX_PERMITS)
            .unwrap_or(DEFAULT_MAX_CONNECTIONS);
        let header_timeout = std::env::var("LLMSHIM_HTTP_HEADER_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .map(Duration::from_millis)
            .filter(|duration| Instant::now().checked_add(*duration).is_some())
            .unwrap_or(DEFAULT_HEADER_TIMEOUT);
        Self {
            maximum_connections,
            header_timeout,
        }
    }
}

#[derive(Clone)]
struct ConnectionAddress {
    socket_address: SocketAddr,
    first_headers_received: Arc<AtomicBool>,
}

struct LimitedListener {
    listener: tokio::net::TcpListener,
    connection_permits: Arc<Semaphore>,
    header_timeout: Duration,
}

struct LimitedStream {
    stream: tokio::net::TcpStream,
    _connection_permit: OwnedSemaphorePermit,
    first_headers_received: Arc<AtomicBool>,
    first_headers_deadline: Pin<Box<Sleep>>,
}

impl axum_server::Address for ConnectionAddress {
    type Stream = LimitedStream;
    type Listener = LimitedListener;
}

impl axum_server::AddrListener<LimitedStream, ConnectionAddress> for LimitedListener {
    async fn bind_to(address: ConnectionAddress) -> std::io::Result<Self> {
        Ok(Self {
            listener: tokio::net::TcpListener::bind(address.socket_address).await?,
            connection_permits: Arc::new(Semaphore::new(DEFAULT_MAX_CONNECTIONS)),
            header_timeout: DEFAULT_HEADER_TIMEOUT,
        })
    }

    async fn accept_stream(&self) -> std::io::Result<(LimitedStream, ConnectionAddress)> {
        let connection_permit = self
            .connection_permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| std::io::Error::other("HTTP connection limiter closed"))?;
        let (stream, socket_address) = self.listener.accept().await?;
        let first_headers_received = Arc::new(AtomicBool::new(false));
        Ok((
            LimitedStream {
                stream,
                _connection_permit: connection_permit,
                first_headers_received: first_headers_received.clone(),
                first_headers_deadline: Box::pin(tokio::time::sleep(self.header_timeout)),
            },
            ConnectionAddress {
                socket_address,
                first_headers_received,
            },
        ))
    }

    fn get_local_addr(&self) -> std::io::Result<ConnectionAddress> {
        Ok(ConnectionAddress {
            socket_address: self.listener.local_addr()?,
            first_headers_received: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl LimitedStream {
    fn check_first_headers(&mut self, context: &mut Context<'_>) -> std::io::Result<()> {
        if !self.first_headers_received.load(Ordering::Acquire)
            && self
                .first_headers_deadline
                .as_mut()
                .poll(context)
                .is_ready()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "HTTP request headers timed out",
            ));
        }
        Ok(())
    }
}

impl AsyncRead for LimitedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.check_first_headers(context)?;
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for LimitedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.check_first_headers(context)?;
        Pin::new(&mut self.stream).poll_write(context, bytes)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.check_first_headers(context)?;
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

#[derive(Clone)]
struct FirstHeadersReceived(Arc<AtomicBool>);

impl Connected<ConnectionAddress> for FirstHeadersReceived {
    fn connect_info(address: ConnectionAddress) -> Self {
        Self(address.first_headers_received)
    }
}

async fn mark_first_headers(
    ConnectInfo(first_headers_received): ConnectInfo<FirstHeadersReceived>,
    request: Request,
    next: Next,
) -> Response {
    first_headers_received.0.store(true, Ordering::Release);
    next.run(request).await
}

pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    application: axum::Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    serve_with_limits(
        listener,
        application,
        shutdown,
        HttpServerLimits::from_env(),
    )
    .await
}

async fn serve_with_limits(
    listener: tokio::net::TcpListener,
    application: axum::Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    limits: HttpServerLimits,
) -> Result<(), String> {
    let limited_listener = LimitedListener {
        listener,
        connection_permits: Arc::new(Semaphore::new(limits.maximum_connections)),
        header_timeout: limits.header_timeout,
    };
    let mut server = axum_server::Server::<ConnectionAddress>::from_listener(limited_listener);
    server
        .http_builder()
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Some(limits.header_timeout))
        .max_headers(MAX_HEADER_COUNT)
        .max_buf_size(MAX_HEADER_BYTES);
    server
        .http_builder()
        .http2()
        .timer(hyper_util::rt::TokioTimer::new())
        .max_concurrent_streams(MAX_HTTP2_STREAMS)
        .max_header_list_size(MAX_HEADER_BYTES as u32)
        .keep_alive_interval(Some(Duration::from_secs(30)))
        .keep_alive_timeout(Duration::from_secs(10));
    let shutdown_handle = axum_server::Handle::<ConnectionAddress>::new();
    let application = application.layer(axum::middleware::from_fn(mark_first_headers));
    let server_future = server
        .handle(shutdown_handle.clone())
        .serve(application.into_make_service_with_connect_info::<FirstHeadersReceived>());
    tokio::pin!(server_future);
    let server_result = tokio::select! {
        server_result = &mut server_future => server_result,
        _ = shutdown => {
            shutdown_handle.graceful_shutdown(None);
            server_future.await
        }
    };
    server_result.map_err(|error| format!("server failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use bytes::Bytes;
    use futures::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    struct TestServer {
        address: SocketAddr,
        shutdown: tokio::sync::oneshot::Sender<()>,
        task: tokio::task::JoinHandle<Result<(), String>>,
    }

    impl TestServer {
        async fn start(maximum_connections: usize, header_timeout: Duration) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let application = axum::Router::new()
                .route("/health", axum::routing::get(|| async { "ok" }))
                .route(
                    "/slow",
                    axum::routing::get(|| async {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        "finished"
                    }),
                )
                .route(
                    "/hold",
                    axum::routing::get(|| async {
                        Body::from_stream(
                            futures::stream::once(async {
                                Ok::<_, std::io::Error>(Bytes::from_static(b"start"))
                            })
                            .chain(futures::stream::pending()),
                        )
                    }),
                );
            let (shutdown, shutdown_receiver) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(serve_with_limits(
                listener,
                application,
                async {
                    let _ = shutdown_receiver.await;
                },
                HttpServerLimits {
                    maximum_connections,
                    header_timeout,
                },
            ));
            Self {
                address,
                shutdown,
                task,
            }
        }

        async fn stop(self) {
            let _ = self.shutdown.send(());
            tokio::time::timeout(Duration::from_secs(2), self.task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }

        async fn health(&self) {
            let mut connection = TcpStream::connect(self.address).await.unwrap();
            connection
                .write_all(b"GET /health HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            tokio::time::timeout(
                Duration::from_secs(1),
                connection.read_to_end(&mut response),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(response.starts_with(b"HTTP/1.1 200"));
        }
    }

    #[tokio::test]
    async fn accepted_connection_capacity_is_held_through_response_body() {
        let server = TestServer::start(1, Duration::from_secs(1)).await;
        let mut held_connection = TcpStream::connect(server.address).await.unwrap();
        held_connection
            .write_all(b"GET /hold HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .await
            .unwrap();
        let mut response = [0; 512];
        let received =
            tokio::time::timeout(Duration::from_secs(1), held_connection.read(&mut response))
                .await
                .unwrap()
                .unwrap();
        assert!(response[..received].starts_with(b"HTTP/1.1 200"));
        let mut waiting_connection = TcpStream::connect(server.address).await.unwrap();
        waiting_connection
            .write_all(b"GET /health HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            waiting_connection.read(&mut response)
        )
        .await
        .is_err());
        drop(held_connection);
        let mut completed_response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(1),
            waiting_connection.read_to_end(&mut completed_response),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(completed_response.starts_with(b"HTTP/1.1 200"));
        drop(waiting_connection);
        server.stop().await;
    }

    #[tokio::test]
    async fn silent_and_incomplete_protocol_headers_expire_and_release_capacity() {
        let server = TestServer::start(1, Duration::from_millis(50)).await;
        for prefix in [
            &b""[..],
            &b"G"[..],
            &b"GET /health HTTP/1.1\r\nHost: fixture\r\n"[..],
            &b"PRI "[..],
            &b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"[..],
        ] {
            let mut connection = TcpStream::connect(server.address).await.unwrap();
            connection.write_all(prefix).await.unwrap();
            let mut response = Vec::new();
            let closed = tokio::time::timeout(
                Duration::from_secs(1),
                connection.read_to_end(&mut response),
            )
            .await;
            assert!(
                closed.is_ok(),
                "connection remained open for prefix length {}",
                prefix.len()
            );
            drop(connection);
            server.health().await;
        }
        server.stop().await;
    }

    #[tokio::test]
    async fn http1_keepalive_next_headers_have_a_finite_deadline() {
        let server = TestServer::start(1, Duration::from_millis(50)).await;
        for next_prefix in [&b""[..], &b"G"[..]] {
            let mut connection = TcpStream::connect(server.address).await.unwrap();
            connection
                .write_all(b"GET /health HTTP/1.1\r\nHost: fixture\r\n\r\n")
                .await
                .unwrap();
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(1), async {
                let mut buffer = [0; 512];
                while !response.ends_with(b"\r\n\r\nok") {
                    let received = connection.read(&mut buffer).await.unwrap();
                    assert!(received > 0);
                    response.extend_from_slice(&buffer[..received]);
                }
            })
            .await
            .unwrap();
            connection.write_all(next_prefix).await.unwrap();
            let closed = tokio::time::timeout(
                Duration::from_secs(1),
                connection.read_to_end(&mut response),
            )
            .await;
            assert!(closed.is_ok(), "keepalive did not expire");
            drop(connection);
            server.health().await;
        }
        server.stop().await;
    }

    #[tokio::test]
    async fn http1_and_http2_responses_can_outlast_the_header_deadline() {
        let server = TestServer::start(2, Duration::from_millis(50)).await;
        for http2 in [false, true] {
            let builder = reqwest::Client::builder()
                .no_proxy()
                .pool_max_idle_per_host(0);
            let client = if http2 {
                builder.http2_prior_knowledge()
            } else {
                builder.http1_only()
            }
            .build()
            .unwrap();
            let response = client
                .get(format!("http://{}/slow", server.address))
                .timeout(Duration::from_secs(1))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(
                response.version(),
                if http2 {
                    reqwest::Version::HTTP_2
                } else {
                    reqwest::Version::HTTP_11
                }
            );
            assert_eq!(response.text().await.unwrap(), "finished");
            drop(client);
        }
        server.stop().await;
    }

    #[tokio::test]
    async fn oversized_http1_headers_fail_before_application_dispatch() {
        let server = TestServer::start(1, Duration::from_secs(1)).await;
        let mut connection = TcpStream::connect(server.address).await.unwrap();
        let request = format!(
            "GET /health HTTP/1.1\r\nHost: fixture\r\nX-Synthetic: {}\r\nConnection: close\r\n\r\n",
            "a".repeat(MAX_HEADER_BYTES)
        );
        let _ = connection.write_all(request.as_bytes()).await;
        let mut response = Vec::new();
        let completed = tokio::time::timeout(
            Duration::from_secs(1),
            connection.read_to_end(&mut response),
        )
        .await;
        assert!(completed.is_ok());
        assert!(!response.starts_with(b"HTTP/1.1 200"));
        drop(connection);
        server.health().await;
        server.stop().await;
    }
}

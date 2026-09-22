use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rcgen::{CertificateParams, KeyPair};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::io::AsyncReadExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MANAGED_AUTH_HEADER: &str = "x-llmshim-managed-token";
const MANAGED_PROTOCOL: &str = "llmshim-managed-v1";
const MAX_MANAGED_CONNECTIONS: usize = 32;
const MANAGED_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const MANAGED_HEADER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct LimitedAddress {
    socket_address: SocketAddr,
    connection_permits: Arc<Semaphore>,
}

struct LimitedListener {
    listener: tokio::net::TcpListener,
    connection_permits: Arc<Semaphore>,
}

struct LimitedStream {
    stream: tokio::net::TcpStream,
    _connection_permit: OwnedSemaphorePermit,
}

type ManagedHttpServer =
    axum_server::Server<LimitedAddress, axum_server::tls_rustls::RustlsAcceptor>;

impl axum_server::Address for LimitedAddress {
    type Stream = LimitedStream;
    type Listener = LimitedListener;
}

impl axum_server::AddrListener<LimitedStream, LimitedAddress> for LimitedListener {
    async fn bind_to(address: LimitedAddress) -> std::io::Result<Self> {
        Ok(Self {
            listener: tokio::net::TcpListener::bind(address.socket_address).await?,
            connection_permits: address.connection_permits,
        })
    }

    async fn accept_stream(&self) -> std::io::Result<(LimitedStream, LimitedAddress)> {
        let connection_permit = self
            .connection_permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| std::io::Error::other("managed connection limiter closed"))?;
        let (stream, socket_address) = self.listener.accept().await?;
        Ok((
            LimitedStream {
                stream,
                _connection_permit: connection_permit,
            },
            LimitedAddress {
                socket_address,
                connection_permits: self.connection_permits.clone(),
            },
        ))
    }

    fn get_local_addr(&self) -> std::io::Result<LimitedAddress> {
        Ok(LimitedAddress {
            socket_address: self.listener.local_addr()?,
            connection_permits: self.connection_permits.clone(),
        })
    }
}

impl AsyncRead for LimitedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for LimitedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

fn build_managed_http_server(
    listener: std::net::TcpListener,
    tls_config: axum_server::tls_rustls::RustlsConfig,
    max_connections: usize,
    handshake_timeout: Duration,
    header_timeout: Duration,
) -> Result<ManagedHttpServer, String> {
    let connection_permits = Arc::new(Semaphore::new(max_connections));
    let limited_listener = LimitedListener {
        listener: tokio::net::TcpListener::from_std(listener)
            .map_err(|error| format!("cannot start managed proxy listener: {error}"))?,
        connection_permits,
    };
    let tls_acceptor = axum_server::tls_rustls::RustlsAcceptor::new(tls_config)
        .handshake_timeout(handshake_timeout);
    let mut server = axum_server::Server::<LimitedAddress>::from_listener(limited_listener)
        .acceptor(tls_acceptor)
        .http1_only();
    server
        .http_builder()
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Some(header_timeout))
        .keep_alive(false)
        .max_buf_size(16 * 1024);
    Ok(server)
}

#[derive(Serialize)]
struct ReadinessRecord<'a> {
    protocol: &'static str,
    base_url: String,
    auth_token: &'a str,
    certificate_pem: &'a str,
}

pub(crate) async fn serve(
    router: llmshim::router::Router,
    logger: Option<llmshim::log::Logger>,
) -> Result<(), String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| format!("cannot bind managed proxy: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("cannot configure managed proxy listener: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("cannot read managed proxy address: {error}"))?;

    let signing_key = KeyPair::generate()
        .map_err(|error| format!("cannot generate managed proxy TLS key: {error}"))?;
    let certificate = CertificateParams::new(vec![Ipv4Addr::LOCALHOST.to_string()])
        .map_err(|error| format!("cannot configure managed proxy TLS certificate: {error}"))?
        .self_signed(&signing_key)
        .map_err(|error| format!("cannot generate managed proxy TLS certificate: {error}"))?;
    let certificate_pem = certificate.pem();
    let private_key_pem = signing_key.serialize_pem();
    let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem(
        certificate_pem.as_bytes().to_vec(),
        private_key_pem.into_bytes(),
    )
    .await
    .map_err(|error| format!("cannot configure managed proxy TLS: {error}"))?;

    let auth_token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let expected_token_hash: Arc<[u8; 32]> = Arc::new(Sha256::digest(auth_token.as_bytes()).into());
    let app = llmshim::proxy::app(router, logger)
        .layer(axum::middleware::from_fn(
            move |request: Request, next: Next| {
                let expected_token_hash = expected_token_hash.clone();
                async move { authorize(request, next, &expected_token_hash).await }
            },
        ))
        .layer(tower::limit::GlobalConcurrencyLimitLayer::new(
            MAX_MANAGED_CONNECTIONS,
        ));
    let server = build_managed_http_server(
        listener,
        tls_config,
        MAX_MANAGED_CONNECTIONS,
        MANAGED_HANDSHAKE_TIMEOUT,
        MANAGED_HEADER_TIMEOUT,
    )?;
    let shutdown_handle = axum_server::Handle::<LimitedAddress>::new();
    let parent_liveness_handle = shutdown_handle.clone();
    tokio::spawn(async move {
        // Managed parents retain stdin's write end; EOF ends this exact child even after a crash.
        let mut parent_liveness = tokio::io::stdin();
        let mut buffer = [0_u8; 64];
        loop {
            match parent_liveness.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        parent_liveness_handle.graceful_shutdown(Some(Duration::from_secs(2)));
    });

    let readiness = ReadinessRecord {
        protocol: MANAGED_PROTOCOL,
        base_url: format!("https://{address}"),
        auth_token: &auth_token,
        certificate_pem: &certificate_pem,
    };
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &readiness)
        .map_err(|error| format!("cannot write managed proxy readiness record: {error}"))?;
    stdout
        .write_all(b"\n")
        .and_then(|_| stdout.flush())
        .map_err(|error| format!("cannot flush managed proxy readiness record: {error}"))?;

    eprintln!("llmshim managed proxy starting on https://{address}");
    server
        .handle(shutdown_handle)
        .serve(app.into_make_service())
        .await
        .map_err(|error| format!("managed proxy failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};
    use tokio_rustls::TlsConnector;

    #[tokio::test]
    async fn connection_permit_is_held_until_the_stream_closes() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let limited = LimitedListener {
            listener,
            connection_permits: Arc::new(Semaphore::new(1)),
        };

        let first_client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (first_stream, _) = axum_server::AddrListener::accept_stream(&limited)
            .await
            .unwrap();
        let second_client = tokio::net::TcpStream::connect(address).await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(25),
            axum_server::AddrListener::accept_stream(&limited)
        )
        .await
        .is_err());

        drop(first_stream);
        let second_stream = tokio::time::timeout(
            Duration::from_millis(250),
            axum_server::AddrListener::accept_stream(&limited),
        )
        .await
        .unwrap()
        .unwrap();
        drop((first_client, second_client, second_stream));
    }

    #[tokio::test]
    async fn handshake_and_header_waits_are_bounded() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let signing_key = KeyPair::generate().unwrap();
        let certificate = CertificateParams::new(vec![Ipv4Addr::LOCALHOST.to_string()])
            .unwrap()
            .self_signed(&signing_key)
            .unwrap();
        let tls_config = axum_server::tls_rustls::RustlsConfig::from_der(
            vec![certificate.der().to_vec()],
            signing_key.serialize_der(),
        )
        .await
        .unwrap();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = build_managed_http_server(
            listener,
            tls_config,
            2,
            Duration::from_millis(50),
            Duration::from_millis(50),
        )
        .unwrap();
        let task = tokio::spawn(server.serve(axum::Router::new().into_make_service()));

        let mut idle_tcp = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_millis(500), idle_tcp.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, 0);

        let mut roots = RootCertStore::empty();
        roots.add(certificate.der().clone()).unwrap();
        let client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
        let server_name =
            tokio_rustls::rustls::pki_types::ServerName::IpAddress(Ipv4Addr::LOCALHOST.into());
        let mut tls = TlsConnector::from(Arc::new(client_config))
            .connect(server_name, tcp)
            .await
            .unwrap();
        let read = tokio::time::timeout(Duration::from_millis(500), tls.read(&mut byte))
            .await
            .unwrap();
        match read {
            Ok(bytes_read) => assert_eq!(bytes_read, 0),
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof),
        }
        task.abort();
    }
}

async fn authorize(request: Request, next: Next, expected_token_hash: &[u8; 32]) -> Response {
    let provided_token = request
        .headers()
        .get(MANAGED_AUTH_HEADER)
        .and_then(|value: &HeaderValue| value.to_str().ok());
    let authorized = provided_token.is_some_and(|token| {
        let provided_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        bool::from(provided_hash.ct_eq(expected_token_hash))
    });
    if authorized {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "unauthorized",
                    "message": "managed proxy authentication required"
                }
            })),
        )
            .into_response()
    }
}

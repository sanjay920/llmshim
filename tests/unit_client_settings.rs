//! The upstream HTTP client built from explicit settings (0021): it applies what
//! it is given, and reads nothing from the environment.
//!
//! Its own test binary, so the environment variables it sets as canaries cannot
//! reach another test's client.

use llmshim::client::{AttemptDeadlines, ClientSettings, ShimClient};
use llmshim::providers::openai_compat::OpenAiCompatible;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `ClientSettings` is `#[non_exhaustive]`: start from the default and assign.
fn with(change: impl FnOnce(&mut ClientSettings)) -> ClientSettings {
    let mut settings = ClientSettings::default();
    change(&mut settings);
    settings
}

const BODY: &str = r#"{"id":"c","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

/// An HTTP/1.1 server that keeps connections alive and counts how many it
/// accepted, so the number of connections a client opened is observable.
async fn counting_server() -> (String, Arc<AtomicUsize>) {
    let accepted = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let count = accepted.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    // One request: headers, then a body of Content-Length bytes.
                    let head_end = loop {
                        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break at + 4;
                        }
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buffer[..head_end]).to_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    while buffer.len() < head_end + length {
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    }
                    buffer.drain(..head_end + length);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{BODY}",
                        BODY.len()
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, accepted)
}

/// `waves` rounds of `width` requests at once, then how many connections were
/// accepted in total.
async fn connections_after(settings: ClientSettings, width: usize, waves: usize) -> usize {
    let (url, accepted) = counting_server().await;
    let client = ShimClient::from_settings(settings).unwrap();
    let provider = Arc::new(OpenAiCompatible::new("p", format!("{url}/v1"), None));
    for _ in 0..waves {
        let mut tasks = Vec::new();
        for _ in 0..width {
            let (client, provider) = (client.clone(), provider.clone());
            tasks.push(tokio::spawn(async move {
                client
                    .completion(
                        provider.as_ref(),
                        "m",
                        &json!({"model": "p/m", "messages": [{"role": "user", "content": "x"}]}),
                    )
                    .await
                    .expect("the counting server answers")
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        // Let the connections return to the pool before the next wave.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    accepted.load(Ordering::SeqCst)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pool_setting_decides_how_many_connections_are_kept_between_waves() {
    let width = 12;
    let settings = |idle| with(|s| s.pool_max_idle_per_host = idle);
    // A pool as large as the wave keeps every connection: the second wave opens
    // none. A pool of one keeps one, so each later wave opens the rest again.
    let kept = connections_after(settings(width), width, 3).await;
    let churned = connections_after(settings(1), width, 3).await;
    assert!(
        kept <= width,
        "a pool of {width} reused its connections: {kept}"
    );
    assert!(
        churned >= 2 * (width - 1),
        "a pool of one reopened them each wave: {churned}"
    );
}

#[test]
fn the_settings_a_client_is_built_from_are_the_settings_it_reports() {
    let deadlines = AttemptDeadlines::default()
        .with_connect_timeout(Duration::from_secs(7))
        .unwrap()
        .with_unary_timeouts(Duration::from_secs(11), Duration::from_secs(222))
        .unwrap();
    let settings = with(|s| {
        s.max_retries = 5;
        s.max_backoff = Duration::from_secs(42);
        s.pool_max_idle_per_host = 99;
        s.pool_idle_timeout = Duration::from_secs(33);
        s.ambient_proxy = false;
        s.deadlines = deadlines;
    });
    let client = ShimClient::from_settings(settings).unwrap();
    assert_eq!(client.settings(), settings);
    assert!(!client.ambient_proxy());
}

#[test]
fn settings_outside_the_bounds_are_refused_not_clamped() {
    let bad = [
        with(|s| s.max_retries = 11),
        with(|s| s.max_backoff = Duration::from_secs(0)),
        with(|s| s.max_backoff = Duration::from_secs(301)),
        with(|s| s.pool_max_idle_per_host = 0),
        with(|s| s.pool_max_idle_per_host = 4097),
        with(|s| s.pool_idle_timeout = Duration::from_secs(3601)),
    ];
    for settings in bad {
        assert!(ShimClient::from_settings(settings).is_err(), "{settings:?}");
    }
    assert!(ShimClient::from_settings(ClientSettings::default()).is_ok());
}

/// Run alone in this binary: the canaries are process-wide.
#[test]
fn a_client_from_settings_ignores_every_llmshim_variable_and_the_shared_one_does_not() {
    for (name, value) in [
        ("LLMSHIM_MAX_RETRIES", "9"),
        ("LLMSHIM_MAX_BACKOFF_SECS", "7"),
        ("LLMSHIM_UPSTREAM_CONNECT_TIMEOUT_MS", "1234"),
        ("LLMSHIM_UPSTREAM_HEADER_TIMEOUT_MS", "2345"),
        ("LLMSHIM_ATTEMPT_POLICY_TIMEOUT_MS", "3456"),
        ("LLMSHIM_UPSTREAM_UNARY_ATTEMPT_TIMEOUT_MS", "4567"),
        ("LLMSHIM_UPSTREAM_STREAM_IDLE_TIMEOUT_MS", "5678"),
    ] {
        std::env::set_var(name, value);
    }
    let from_settings = ShimClient::from_settings(ClientSettings::default()).unwrap();
    assert_eq!(
        from_settings.settings(),
        ClientSettings::default(),
        "explicit settings are not touched by the environment"
    );
    // The environment-driven client does read them, so the canaries are live.
    let from_env = ShimClient::new().settings();
    assert_eq!(from_env.max_retries, 9);
    assert_eq!(from_env.max_backoff, Duration::from_secs(7));
    assert_ne!(from_env.deadlines, AttemptDeadlines::default());
}

/// A transport that authorizes nothing and asks for the scoped client, as the cloud
/// transports do (0020, 0021): the path that builds its own connection pool.
struct Scoped;

impl llmshim::transport::RequestTransport for Scoped {
    fn authorize<'a>(
        &'a self,
        _request: &'a mut reqwest::Request,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = llmshim::error::Result<()>> + Send + 'a>>
    {
        Box::pin(async { Ok(()) })
    }

    fn events(
        &self,
        _model: &str,
        _bytes: llmshim::transport::ByteStream,
    ) -> llmshim::transport::EventStream {
        unreachable!("this test sends no stream")
    }

    fn error(
        &self,
        status: u16,
        _headers: &reqwest::header::HeaderMap,
        body: String,
        retry_after: Option<Duration>,
    ) -> llmshim::error::ShimError {
        llmshim::error::ShimError::ProviderError {
            status,
            body,
            retry_after,
        }
    }
}

async fn scoped_connections_after(idle: usize, width: usize, waves: usize) -> usize {
    let (url, accepted) = counting_server().await;
    // An ambient client, so a transport that wants a scoped one gets the lazily
    // built second pool, which must take the same settings.
    let client = ShimClient::from_settings(with(|s| s.pool_max_idle_per_host = idle)).unwrap();
    assert!(client.ambient_proxy());
    for _ in 0..waves {
        let mut tasks = Vec::new();
        for _ in 0..width {
            let (client, url) = (client.clone(), url.clone());
            tasks.push(tokio::spawn(async move {
                let request = llmshim::provider::ProviderRequest {
                    url: format!("{url}/v1/chat/completions"),
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: json!({"x": 1}),
                };
                client
                    .send_with_transport(&request, &Scoped)
                    .await
                    .expect("the counting server answers")
                    .bytes()
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    accepted.load(Ordering::SeqCst)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_scoped_transport_the_cloud_providers_use_takes_the_pool_setting_too() {
    let width = 12;
    let kept = scoped_connections_after(width, width, 3).await;
    let churned = scoped_connections_after(1, width, 3).await;
    assert!(
        kept <= width,
        "a pool of {width} reused its connections: {kept}"
    );
    assert!(
        churned >= 2 * (width - 1),
        "a pool of one reopened them each wave: {churned}"
    );
}

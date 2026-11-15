//! What `llmshim serve` does when something it depends on goes away while it
//! runs: the rows of the failure-mode table (note 0014) that need a real server
//! process.
//!
//! The Redis test needs a Redis instance:
//! `LLMSHIM_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test --features serve-redis
//! --test integration_serve_outage -- --ignored`
#![cfg(feature = "serve")]
// The SIGTERM test and its helpers are for Unix; on Windows the file only has to
// compile.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{completion, config, start, CALLER_KEY};
use serde_json::json;
use std::time::Duration;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(40))
        .build()
        .unwrap()
}

fn chat_request(client: &reqwest::Client, base: &str, model: &str) -> reqwest::RequestBuilder {
    client
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(CALLER_KEY)
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "ping"}]}))
}

/// Wait until the mock has received its request, so the request is known to be
/// with the provider (and not still on its way) when the test breaks something.
async fn until_with_the_provider(mock: &mockito::Mock) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !mock.matched_async().await {
        assert!(
            std::time::Instant::now() < deadline,
            "the request never reached the provider"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// No line the server wrote after it started looks like a crash.
fn assert_no_panic(server: &harness::Server) {
    let lines = server.stderr.lock().unwrap();
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("panicked") || line.contains("RUST_BACKTRACE")),
        "the server panicked: {lines:#?}"
    );
}

/// `SIGTERM` is how an orchestrator stops a replica. A request already in
/// flight finishes, the listener stops taking new connections, and the process
/// exits 0: a rolling deploy does not cut an answer in half.
#[cfg(unix)]
#[tokio::test]
async fn sigterm_lets_an_in_flight_request_finish_then_exits_zero() {
    let mut upstream = mockito::Server::new_async().await;
    let body = completion("slow pong");
    let slow = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_chunked_body(move |writer| {
            std::thread::sleep(Duration::from_millis(1500));
            std::io::Write::write_all(writer, body.as_bytes())
        })
        .expect(1)
        .create_async()
        .await;
    let mut server = start(&config(&upstream.url(), ""), "sigterm", &[]);
    let base = server.base.clone();

    let client = client();
    let in_flight = tokio::spawn({
        let request = chat_request(&client, &base, "inference/test-model");
        async move { request.send().await }
    });
    // The upstream is holding the answer for 1.5 s; the request is in flight.
    until_with_the_provider(&slow).await;
    harness::terminate(&server);

    // The listener stops accepting new connections while the old one drains.
    let address = base.trim_start_matches("http://").to_owned();
    let refused_by = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if tokio::net::TcpStream::connect(&address).await.is_err() {
            break;
        }
        assert!(
            std::time::Instant::now() < refused_by,
            "the listener still accepts connections after SIGTERM"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let response = in_flight
        .await
        .unwrap()
        .expect("the in-flight request finishes");
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"], "slow pong",
        "{body}"
    );

    let code = harness::exit_code_within(&mut server, Duration::from_secs(10)).await;
    assert_eq!(code, Some(Some(0)), "exits cleanly after draining");
    assert_no_panic(&server);
}

#[cfg(feature = "serve-redis")]
mod redis_outage {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};

    /// A TCP forwarder to the real Redis that a test can cut and restore: a
    /// cut closes every open connection and drops new ones at once, which is
    /// what a replica sees when its Redis, or the network to it, goes away.
    struct RedisProxy {
        address: std::net::SocketAddr,
        cut: tokio::sync::watch::Sender<bool>,
    }

    impl RedisProxy {
        async fn start(target: &str) -> Self {
            let target = std::net::ToSocketAddrs::to_socket_addrs(
                target
                    .trim_start_matches("redis://")
                    .split('/')
                    .next()
                    .unwrap(),
            )
            .unwrap()
            .next()
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (cut, watch) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                while let Ok((mut client, _)) = listener.accept().await {
                    if *watch.borrow() {
                        continue; // dropped: the connection closes at once
                    }
                    let mut watch = watch.clone();
                    tokio::spawn(async move {
                        let Ok(mut upstream) = TcpStream::connect(target).await else {
                            return;
                        };
                        tokio::select! {
                            _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {}
                            _ = watch.wait_for(|cut| *cut) => {}
                        }
                    });
                }
            });
            Self { address, cut }
        }

        fn cut(&self) {
            self.cut.send(true).unwrap();
        }

        fn restore(&self) {
            self.cut.send(false).unwrap();
        }
    }

    /// Names no other test sharing this Redis has used (the queue, the buckets
    /// and the spend ledger are keyed by provider name and tenant).
    fn fleet_config(upstream: &str, provider: &str, tenant: &str) -> String {
        config(
            upstream,
            "[fleet]\nredis_url = { env = \"SERVE_E2E_REDIS_URL\" }",
        )
        .replace("providers.inference", &format!("providers.{provider}"))
        .replace("tenant = \"platform\"", &format!("tenant = \"{tenant}\""))
    }

    async fn ready(client: &reqwest::Client, base: &str) -> u16 {
        client
            .get(format!("{base}/ready"))
            .send()
            .await
            .map(|response| response.status().as_u16())
            .unwrap_or(0)
    }

    /// Redis goes away while a request is already with the provider. The caller is
    /// not left waiting for an answer that can no longer be delivered, the
    /// replica stays up, and the next request after Redis returns is served.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
    async fn losing_redis_mid_request_ends_the_call_promptly_and_the_replica_recovers() {
        let target = std::env::var("LLMSHIM_TEST_REDIS_URL").expect("set LLMSHIM_TEST_REDIS_URL");
        let id = uuid::Uuid::new_v4().simple();
        let (provider, tenant) = (format!("midflight-{id}"), format!("tenant-{id}"));
        let model = format!("{provider}/test-model");

        // The first answer takes 2 s, so Redis can be cut while it is pending.
        let mut upstream = mockito::Server::new_async().await;
        let slow = completion("late");
        let slow_mock = upstream
            .mock("POST", "/chat/completions")
            .with_header("content-type", "application/json")
            .with_chunked_body(move |writer| {
                std::thread::sleep(Duration::from_millis(2000));
                std::io::Write::write_all(writer, slow.as_bytes())
            })
            .expect(1)
            .create_async()
            .await;

        let proxy = RedisProxy::start(&target).await;
        let redis_through_proxy = format!("redis://{}", proxy.address);
        let mut server = start(
            &fleet_config(&upstream.url(), &provider, &tenant),
            "midflight",
            &[("SERVE_E2E_REDIS_URL", redis_through_proxy.as_str())],
        );
        let base = server.base.clone();
        let client = client();

        let started = std::time::Instant::now();
        let in_flight = tokio::spawn({
            let request = chat_request(&client, &base, &model);
            async move { request.send().await }
        });
        until_with_the_provider(&slow_mock).await;
        proxy.cut();
        let outcome = in_flight.await.unwrap();
        let waited = started.elapsed();
        let status = outcome.as_ref().map(|r| r.status().as_u16()).unwrap_or(0);
        eprintln!("mid-flight loss: status {status} after {waited:?}");
        assert!(
            waited < Duration::from_secs(30),
            "the caller is not left waiting for an answer that cannot be delivered: {waited:?}"
        );
        assert_ne!(
            status, 0,
            "the connection to the caller is answered, not dropped"
        );
        assert!(
            server.child.try_wait().unwrap().is_none(),
            "the replica stays up"
        );

        // Redis returns. The next request is served (a fresh answer from a
        // second mock, since the first has been used).
        let _fast = upstream
            .mock("POST", "/chat/completions")
            .with_header("content-type", "application/json")
            .with_body(completion("pong"))
            .expect(1)
            .create_async()
            .await;
        proxy.restore();
        let recovering = std::time::Instant::now();
        let mut served = false;
        while recovering.elapsed() < Duration::from_secs(60) {
            if let Ok(response) = chat_request(&client, &base, &model).send().await {
                if response.status() == 200 {
                    served = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(served, "served again within a minute of Redis returning");
        assert_no_panic(&server);
    }

    /// Losing Redis must not turn into admitting work nobody is accounting for,
    /// and getting Redis back must not need a restart.
    // Multi-threaded: `start` blocks this thread until the replica prints its
    // banner, and the replica needs the proxy (a task of this runtime) to reach
    // Redis before it can.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
    async fn losing_redis_refuses_work_instead_of_admitting_it_and_recovery_needs_no_restart() {
        let target = std::env::var("LLMSHIM_TEST_REDIS_URL").expect("set LLMSHIM_TEST_REDIS_URL");
        let id = uuid::Uuid::new_v4().simple();
        let (provider, tenant) = (format!("outage-{id}"), format!("tenant-{id}"));
        let model = format!("{provider}/test-model");

        // Exactly two answers are ever fetched: one before the outage and one
        // after it. A request admitted during the outage would make a third.
        let mut upstream = mockito::Server::new_async().await;
        let mock = upstream
            .mock("POST", "/chat/completions")
            .with_header("content-type", "application/json")
            .with_body(completion("pong"))
            .expect(2)
            .create_async()
            .await;

        let proxy = RedisProxy::start(&target).await;
        let redis_through_proxy = format!("redis://{}", proxy.address);
        let mut server = start(
            &fleet_config(&upstream.url(), &provider, &tenant),
            "outage",
            &[("SERVE_E2E_REDIS_URL", redis_through_proxy.as_str())],
        );
        let base = server.base.clone();
        let client = client();

        // With Redis, a request is served and the replica is ready.
        let before = chat_request(&client, &base, &model).send().await.unwrap();
        assert_eq!(before.status(), 200);
        assert_eq!(ready(&client, &base).await, 200);

        // Redis goes away.
        proxy.cut();
        let started = std::time::Instant::now();
        let during = chat_request(&client, &base, &model).send().await;
        let waited = started.elapsed();
        let status = during.as_ref().map(|r| r.status().as_u16()).unwrap_or(0);
        assert!(
            (500..600).contains(&status),
            "a request with no accounting is refused (5xx), not served: {status}"
        );
        assert!(
            waited < Duration::from_secs(25),
            "and the refusal is prompt: {waited:?}"
        );
        assert_eq!(ready(&client, &base).await, 503, "not ready without Redis");
        assert!(
            server.child.try_wait().unwrap().is_none(),
            "the replica stays up"
        );

        // Redis comes back; no restart.
        proxy.restore();
        let recovering = std::time::Instant::now();
        let mut recovered = None;
        while recovering.elapsed() < Duration::from_secs(60) {
            if let Ok(response) = chat_request(&client, &base, &model).send().await {
                if response.status() == 200 {
                    recovered = Some(recovering.elapsed());
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let recovered =
            recovered.expect("the replica serves again within a minute of Redis returning");
        eprintln!("recovered {recovered:?} after Redis returned; refused in {waited:?}");
        assert_eq!(ready(&client, &base).await, 200);
        assert!(server.child.try_wait().unwrap().is_none());
        assert_no_panic(&server);
        mock.assert_async().await;
    }
}

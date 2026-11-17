//! What `llmshim serve` does when something it depends on goes away while it
//! runs: the rows of the failure-mode table (note 0014) that need a real server
//! process.
//!
//! The Redis tests need a Redis instance:
//! `LLMSHIM_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test --features serve-redis
//! --test integration_serve_outage -- --ignored --test-threads=1`
//! (one at a time: a replica abandoned in the middle of a request leaves a lease
//! that another test's replicas would sweep, 0014).
#![cfg(feature = "serve")]
// The SIGTERM test and its helpers are for Unix; on Windows the file only has to
// compile.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{completion, config, start, CALLER_KEY};
use serde_json::json;
use std::sync::mpsc;
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

/// No line the server wrote looks like a crash.
fn assert_no_panic(lines: &[String]) {
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("panicked") || line.contains("RUST_BACKTRACE")),
        "the server panicked: {lines:#?}"
    );
}

/// A mock body that holds the provider's answer until the test says so, so that
/// "the request is in flight" is a state the test controls and not a sleep it
/// hopes is long enough.
fn held_answer(
    text: &str,
) -> (
    impl Fn(&mut dyn std::io::Write) -> std::io::Result<()>,
    mpsc::Sender<()>,
) {
    let (release, wait) = mpsc::channel::<()>();
    let wait = std::sync::Mutex::new(wait);
    let body = completion(text);
    (
        move |writer: &mut dyn std::io::Write| {
            // Released by the test; the timeout only keeps a failed test from
            // leaving the mock server's thread blocked for good.
            let _ = wait.lock().unwrap().recv_timeout(Duration::from_secs(30));
            writer.write_all(body.as_bytes())
        },
        release,
    )
}

/// `SIGTERM` is how an orchestrator stops a replica. A request already in
/// flight finishes with its answer, both listeners stop accepting at once, and
/// the process exits 0: a rolling deploy does not cut an answer in half.
#[cfg(unix)]
#[tokio::test]
async fn sigterm_lets_an_in_flight_request_finish_then_exits_zero() {
    let (answer, release) = held_answer("slow pong");
    let mut upstream = mockito::Server::new_async().await;
    let slow = upstream
        .mock("POST", "/chat/completions")
        .with_header("content-type", "application/json")
        .with_chunked_body(move |writer| answer(writer))
        .expect(1)
        .create_async()
        .await;
    let mut server = start(
        &config(
            &upstream.url(),
            "[admin]\nport = 0\nallow_unauthenticated = true",
        ),
        "sigterm",
        &[],
    );
    let base = server.base.clone();
    let admin = server.admin.clone().expect("the admin listener's address");
    let bare = |address: &str| address.trim_start_matches("http://").to_owned();

    let client = client();
    let in_flight = tokio::spawn({
        let request = chat_request(&client, &base, "inference/test-model");
        async move { request.send().await }
    });
    // The provider holds the answer until the test releases it, so the request
    // stays in flight for exactly as long as the checks below need.
    until_with_the_provider(&slow).await;
    harness::terminate(&server);

    // Both listeners stop accepting while the old connection drains, and the
    // request is still pending when they have: it is the drain that is waited
    // for, not the end of the process.
    for address in [bare(&base), bare(&admin)] {
        let refused_by = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if tokio::net::TcpStream::connect(&address).await.is_err() {
                break;
            }
            assert!(
                std::time::Instant::now() < refused_by,
                "{address} still accepts connections after SIGTERM"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    assert!(
        !in_flight.is_finished(),
        "the request must still be in flight while the listeners are already closed"
    );
    assert!(
        server.child.try_wait().unwrap().is_none(),
        "the process waits for the request"
    );

    release.send(()).unwrap();
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

    let status = harness::exit_status_within(&mut server, Duration::from_secs(10))
        .await
        .expect("the process exits once the request is done");
    assert_eq!(status.code(), Some(0), "exits cleanly after draining");
    let stderr = server.finished_stderr();
    assert!(
        stderr
            .iter()
            .any(|line| line.contains("shutdown signal received")),
        "{stderr:#?}"
    );
    assert_no_panic(&stderr);
}

#[cfg(feature = "serve-redis")]
mod redis_outage {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};

    /// A TCP forwarder to the real Redis that a test can cut and restore: a
    /// cut closes every open connection and drops new ones at once, which is
    /// what a replica sees when its Redis, or the network to it, goes away. A
    /// generation counter makes a cut kill the connections that exist even if it
    /// is undone before their tasks next run.
    struct RedisProxy {
        address: std::net::SocketAddr,
        state: tokio::sync::watch::Sender<(bool, u64)>,
    }

    impl RedisProxy {
        async fn start(target: &str) -> Self {
            // `redis://[user:password@]host:port[/db]`: the proxy only needs where
            // to connect.
            let authority = target
                .trim_start_matches("redis://")
                .split('/')
                .next()
                .unwrap()
                .rsplit('@')
                .next()
                .unwrap()
                .to_owned();
            let targets: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host(&authority).await.unwrap().collect();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (state, watch) = tokio::sync::watch::channel((false, 0_u64));
            tokio::spawn(async move {
                while let Ok((mut client, _)) = listener.accept().await {
                    let (cut, generation) = *watch.borrow();
                    if cut {
                        continue; // dropped: the connection closes at once
                    }
                    let mut watch = watch.clone();
                    let targets = targets.clone();
                    tokio::spawn(async move {
                        let mut upstream = None;
                        for target in targets {
                            if let Ok(stream) = TcpStream::connect(target).await {
                                upstream = Some(stream);
                                break;
                            }
                        }
                        let Some(mut upstream) = upstream else { return };
                        tokio::select! {
                            _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {}
                            _ = watch.wait_for(|(_, now)| *now != generation) => {}
                        }
                    });
                }
            });
            Self { address, state }
        }

        fn cut(&self) {
            self.state.send_modify(|(cut, generation)| {
                *cut = true;
                *generation += 1;
            });
        }

        fn restore(&self) {
            self.state.send_modify(|(cut, _)| *cut = false);
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

    async fn status(client: &reqwest::Client, base: &str, path: &str) -> u16 {
        client
            .get(format!("{base}{path}"))
            .send()
            .await
            .map(|response| response.status().as_u16())
            .unwrap_or(0)
    }

    // Multi-threaded: `start` blocks this thread until the replica prints its
    // banner, and the replica needs the proxy (a task of this runtime) to reach
    // Redis before it can.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
    async fn losing_redis_refuses_new_work_and_recovery_needs_no_restart() {
        let target = std::env::var("LLMSHIM_TEST_REDIS_URL").expect("set LLMSHIM_TEST_REDIS_URL");
        let id = uuid::Uuid::new_v4().simple();
        let (provider, tenant) = (format!("outage-{id}"), format!("tenant-{id}"));
        let model = format!("{provider}/test-model");

        // One answer before the outage. The provider is asserted to have been
        // called exactly once before Redis comes back, so nothing was admitted
        // while it was gone; a second mock serves the recovery.
        let mut upstream = mockito::Server::new_async().await;
        let before_mock = upstream
            .mock("POST", "/chat/completions")
            .with_header("content-type", "application/json")
            .with_body(completion("pong"))
            .expect(1)
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
        assert_eq!(status(&client, &base, "/ready").await, 200);

        // Redis goes away.
        proxy.cut();
        let started = std::time::Instant::now();
        let during = chat_request(&client, &base, &model).send().await;
        let waited = started.elapsed();
        let refused = during.as_ref().map(|r| r.status().as_u16()).unwrap_or(0);
        assert!(
            (500..600).contains(&refused),
            "a request that cannot be queued is refused (5xx), not served: {refused}"
        );
        assert!(
            waited < Duration::from_secs(3),
            "and the refusal is prompt, well inside the 5 s Redis deadline: {waited:?}"
        );
        assert_eq!(
            status(&client, &base, "/ready").await,
            503,
            "not ready without Redis"
        );
        assert_eq!(
            status(&client, &base, "/health").await,
            200,
            "but alive: the liveness probe does not depend on Redis"
        );
        assert!(
            server.child.try_wait().unwrap().is_none(),
            "the replica stays up"
        );
        // Nothing was admitted while Redis was gone: the provider has still been
        // called exactly once.
        before_mock.assert_async().await;

        // Redis comes back; no restart.
        let _recovery_mock = upstream
            .mock("POST", "/chat/completions")
            .with_header("content-type", "application/json")
            .with_body(completion("pong"))
            .expect_at_least(1)
            .create_async()
            .await;
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
        assert_eq!(status(&client, &base, "/ready").await, 200);
        assert!(server.child.try_wait().unwrap().is_none());
        assert_no_panic(&server.stderr.lock().unwrap());
    }

    /// A Responses API answer, for the hosted `openai` kind pointed at a mock.
    fn responses_body(text: &str) -> String {
        json!({
            "id": "resp_1", "object": "response", "status": "completed", "model": "gpt-6-luna",
            "output": [{"type": "message", "id": "msg_1", "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}]}],
            "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
        })
        .to_string()
    }

    /// With `[fleet.outage_admission]` a budgeted key keeps being served from its
    /// lease while Redis is gone, the replica stays ready, and when Redis returns
    /// the lease is settled and the fleet path serves again (note 0027).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
    async fn a_budgeted_key_is_served_from_its_lease_while_redis_is_gone() {
        let target = std::env::var("LLMSHIM_TEST_REDIS_URL").expect("set LLMSHIM_TEST_REDIS_URL");
        let id = uuid::Uuid::new_v4().simple();
        let tenant = format!("tenant-{id}");
        // A priced model, so an attempt's cost is bounded before it is sent.
        let model = "openai/gpt-6-luna";

        let mut upstream = mockito::Server::new_async().await;
        let mock = upstream
            .mock("POST", "/responses")
            .with_header("content-type", "application/json")
            .with_body(responses_body("pong"))
            .expect_at_least(3)
            .create_async()
            .await;

        let proxy = RedisProxy::start(&target).await;
        let redis_through_proxy = format!("redis://{}", proxy.address);
        let extra = "budget_usd = 100.0\n[fleet]\nredis_url = { env = \"SERVE_E2E_REDIS_URL\" }\n\
                     [fleet.outage_admission]\nspend_lease_fraction = 0.5\nrecover_after_secs = 5";
        let text = config(&upstream.url(), extra)
            .replace("providers.inference", "providers.openai")
            .replace("kind = \"openai-compatible\"", "kind = \"openai\"")
            .replace("tenant = \"platform\"", &format!("tenant = \"{tenant}\""));
        let mut server = start(
            &text,
            "outage-admission",
            &[("SERVE_E2E_REDIS_URL", redis_through_proxy.as_str())],
        );
        let base = server.base.clone();
        let client = client();
        let ask = || async {
            client
                .post(format!("{base}/v1/chat/completions"))
                .bearer_auth(CALLER_KEY)
                .json(&json!({"model": model, "max_tokens": 50,
                    "messages": [{"role": "user", "content": "ping"}]}))
                .send()
                .await
        };

        // Healthy: served through the fleet. That registers the tenant, and a
        // lease follows within a tick or two.
        assert_eq!(ask().await.unwrap().status(), 200);
        tokio::time::sleep(Duration::from_secs(5)).await;

        proxy.cut();
        // Wait for the replica to notice (three failed probes).
        let noticed = std::time::Instant::now();
        let mut served = None;
        let mut last = String::new();
        while noticed.elapsed() < Duration::from_secs(40) {
            if let Ok(response) = ask().await {
                if response.status() == 200 {
                    served = Some(noticed.elapsed());
                    break;
                }
                last = format!(
                    "{} {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        eprintln!("last refusal: {last}");
        let served = served.expect("a budgeted key is served from its lease during the outage");
        eprintln!("served from the lease {served:?} after Redis was cut");
        assert_eq!(
            status(&client, &base, "/ready").await,
            200,
            "a replica serving from its lease stays in the Service"
        );
        assert!(server.child.try_wait().unwrap().is_none());

        proxy.restore();
        let recovering = std::time::Instant::now();
        let settled = |server: &harness::Server| {
            server
                .stderr
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("leases settled"))
        };
        // Requests keep being served (locally, then through the fleet) while
        // the lease is settled; no restart is needed.
        while !settled(&server) && recovering.elapsed() < Duration::from_secs(90) {
            assert_eq!(ask().await.unwrap().status(), 200);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(
            settled(&server),
            "the lease was settled after Redis returned"
        );
        assert_eq!(
            ask().await.unwrap().status(),
            200,
            "served on the fleet path again"
        );
        let log = server.stderr.lock().unwrap().join("\n");
        assert!(log.contains("serving budgeted keys from leases"), "{log}");
        mock.assert_async().await;
        assert_no_panic(&server.stderr.lock().unwrap());
    }

    /// Redis goes away while a request is already with the provider. The caller
    /// is not left waiting for an answer that can no longer be delivered, the
    /// replica stays up, and the next request after Redis returns is served.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs Redis: set LLMSHIM_TEST_REDIS_URL"]
    async fn losing_redis_mid_request_ends_the_call_promptly_and_the_replica_recovers() {
        let target = std::env::var("LLMSHIM_TEST_REDIS_URL").expect("set LLMSHIM_TEST_REDIS_URL");
        let id = uuid::Uuid::new_v4().simple();
        let (provider, tenant) = (format!("midflight-{id}"), format!("tenant-{id}"));
        let model = format!("{provider}/test-model");

        // The provider holds its answer until Redis has been cut, so the cut is
        // known to land while the request is with the provider.
        let (answer, release) = held_answer("late");
        let mut upstream = mockito::Server::new_async().await;
        let slow_mock = upstream
            .mock("POST", "/chat/completions")
            .with_header("content-type", "application/json")
            .with_chunked_body(move |writer| answer(writer))
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
        release.send(()).unwrap(); // the provider answers, into a replica with no Redis
        let outcome = in_flight.await.unwrap();
        let waited = started.elapsed();
        let refused = outcome.as_ref().map(|r| r.status().as_u16()).unwrap_or(0);
        eprintln!("mid-flight loss: status {refused} after {waited:?}");
        assert!(
            (500..600).contains(&refused),
            "the caller is told the request failed (5xx): {refused}"
        );
        assert!(
            waited < Duration::from_secs(30),
            "and is not left waiting for an answer that cannot be delivered: {waited:?}"
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
            .expect_at_least(1)
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
        assert_no_panic(&server.stderr.lock().unwrap());
    }
}

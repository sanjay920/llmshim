//! A selected fake OAuth endpoint must not stall readers of a still-valid bearer.
use super::*;

async fn until_calls(calls: &AtomicUsize, expected: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        if calls.load(Ordering::SeqCst) == expected {
            // Let the exchange result publish before asserting the next burst.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "expected {expected} token calls, saw {}",
        calls.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn hung_margin_refresh_never_delays_valid_token_readers_and_retries_after_expiry() {
    let (provider, calls, _dir) = provider(vec![200, 0, 200]);
    assert_eq!(header(&provider).await.unwrap(), "Bearer fixture-0");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(91)).await;
    let started = tokio::time::Instant::now();
    let (first, second) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(header(&provider), header(&provider))
    })
    .await
    .expect("valid-token readers waited for the hung refresh");
    assert_eq!(first.unwrap(), "Bearer fixture-0");
    assert_eq!(second.unwrap(), "Bearer fixture-0");
    assert_eq!(started.elapsed(), Duration::ZERO);
    until_calls(&calls, 2).await;
    let (first, second) =
        bounded(async { tokio::join!(header(&provider), header(&provider)) }).await;
    assert_eq!(first.unwrap(), "Bearer fixture-0");
    assert_eq!(second.unwrap(), "Bearer fixture-0");
    assert_eq!(started.elapsed(), Duration::ZERO);
    // The refresh runs for its entire bound, reaching expiry without discarding a token.
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(
        bounded(header(&provider)).await.unwrap(),
        "Bearer fixture-2"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn concurrent_fast_failures_make_one_refresh_per_margin_step() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Vertex::gemini(GoogleIdentity::token_source(
        "fixture-project",
        "global",
        "fast-endpoint",
        Arc::new(FastEndpoint(calls.clone())),
    ));
    header(&provider).await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(91)).await;
    let awake = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    for expected in [2, 3] {
        let burst = futures::future::join_all((0..64).map(|_| header(&provider))).await;
        for token in burst {
            assert_eq!(token.unwrap(), "Bearer fixture-0");
        }
        until_calls(&calls, expected).await;
        for token in futures::future::join_all((0..64).map(|_| header(&provider))).await {
            assert_eq!(token.unwrap(), "Bearer fixture-0");
        }
        // Complete any ready fake-source task before advancing the next margin step.
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), expected);
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    // A fast failure is throttled for one second, then a later request can recover.
    assert_eq!(header(&provider).await.unwrap(), "Bearer fixture-0");
    until_calls(&calls, 4).await;
    assert_eq!(header(&provider).await.unwrap(), "Bearer fixture-3");
    awake.abort();
}

async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut future = std::pin::pin!(future);
    loop {
        if let std::task::Poll::Ready(value) = futures::poll!(future.as_mut()) {
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "authorization stalled"
        );
        tokio::task::yield_now().await;
    }
}

struct FastEndpoint(Arc<AtomicUsize>);
impl TokenSource for FastEndpoint {
    fn token(&self) -> TokenFuture<'_> {
        Box::pin(async {
            let call = self.0.fetch_add(1, Ordering::SeqCst);
            if matches!(call, 1 | 2) {
                Err(CredentialError::Unavailable)
            } else {
                AccessToken::new(&format!("fixture-{call}"), Duration::from_secs(120))
            }
        })
    }
}

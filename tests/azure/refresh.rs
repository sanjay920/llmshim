//! Fake token endpoint responses protect cache lifecycle without any network or ambient identity.
use super::*;
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut future = std::pin::pin!(future);
    loop {
        if let std::task::Poll::Ready(value) = futures::poll!(future.as_mut()) {
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "token operation stalled"
        );
        tokio::task::yield_now().await;
    }
}
async fn observed(calls: &AtomicUsize, expected: usize) {
    bounded(async {
        while calls.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), expected);
}
#[tokio::test(start_paused = true)]
async fn hung_margin_refresh_never_stalls_live_readers_and_expired_timeout_recovers() {
    let (provider, calls) = selected(vec![200, 0, 200]);
    assert_eq!(header(&provider).await, "Bearer fixture-0");
    tokio::time::advance(Duration::from_secs(91)).await;
    let started = tokio::time::Instant::now();
    let (a, b) = ready(async { tokio::join!(header(&provider), header(&provider)) }).await;
    assert_eq!(a, "Bearer fixture-0");
    assert_eq!(b, "Bearer fixture-0");
    observed(&calls, 2).await;
    let (a, b) = ready(async { tokio::join!(header(&provider), header(&provider)) }).await;
    assert_eq!(a, "Bearer fixture-0");
    assert_eq!(b, "Bearer fixture-0");
    assert_eq!(started.elapsed(), Duration::ZERO);
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(bounded(header(&provider)).await, "Bearer fixture-2");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}
#[tokio::test(start_paused = true)]
async fn concurrent_failed_margin_refreshes_keep_live_token_and_are_throttled() {
    let (provider, calls) = selected(vec![200, 503, 503, 200]);
    header(&provider).await;
    tokio::time::advance(Duration::from_secs(91)).await;
    for expected in [2, 3] {
        for _ in 0..2 {
            let tokens = bounded(futures::future::join_all(
                (0..64).map(|_| header(&provider)),
            ))
            .await;
            assert!(tokens.iter().all(|token| token == "Bearer fixture-0"));
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), expected);
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    assert_eq!(header(&provider).await, "Bearer fixture-0");
    observed(&calls, 4).await;
    assert_eq!(header(&provider).await, "Bearer fixture-3");
}
#[tokio::test(start_paused = true)]
async fn expired_and_rejected_tokens_cannot_be_reused_after_refresh_failure() {
    for reject in [false, true] {
        let (provider, calls) = selected(vec![200, 503, 200]);
        let old = header(&provider).await;
        if reject {
            provider
                .transport()
                .unwrap()
                .authorization_rejected(401, "", Some(&old))
                .await
                .unwrap();
        } else {
            tokio::time::advance(Duration::from_secs(121)).await;
        }
        let error = bounded(try_header(&provider)).await.unwrap_err();
        assert_eq!(AzureError::from_shim(&error).unwrap().status, 503);
        assert_eq!(bounded(header(&provider)).await, "Bearer fixture-2");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        provider
            .transport()
            .unwrap()
            .authorization_rejected(401, "", Some(&old))
            .await
            .unwrap();
        assert_eq!(header(&provider).await, "Bearer fixture-2");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}

async fn ready<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    match futures::poll!(future.as_mut()) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("live-token authorization waited for refresh"),
    }
}

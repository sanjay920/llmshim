//! Token-service responses and inference responses are entirely in memory.
use super::*;
use crate::providers::vertex::*;
use std::sync::atomic::{AtomicUsize, Ordering};
struct Exchange {
    calls: Arc<AtomicUsize>,
    statuses: Vec<u16>,
}
impl TokenExchange for Exchange {
    fn exchange(&self, _: reqwest::Request) -> ExchangeFuture<'_> {
        Box::pin(async {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.statuses.get(n) == Some(&0) {
                return std::future::pending().await;
            }
            Ok(ExchangeResponse::new(
                *self.statuses.get(n).unwrap_or(&200),
                serde_json::to_vec(&serde_json::json!({"access_token":format!("fixture-{n}"),
                    "token_type":"Bearer","expires_in":120}))
                .unwrap(),
            ))
        })
    }
}
fn provider(statuses: Vec<u16>) -> (Vertex, Arc<AtomicUsize>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selected.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"type":"service_account",
        "client_email":"fixture@example.invalid","private_key_id":"fixture",
        "private_key":include_str!("../../tests/fixtures/vertex/synthetic-key.pem"),
        "token_uri":"https://oauth2.googleapis.com/token"}))
        .unwrap(),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let source = ServiceAccount::new(path).with_exchange(Arc::new(Exchange {
        calls: calls.clone(),
        statuses,
    }));
    (
        Vertex::gemini(GoogleIdentity::token_source(
            "fixture-project",
            "global",
            "chosen",
            Arc::new(source),
        )),
        calls,
        dir,
    )
}
fn prepared(provider: &Vertex) -> ProviderRequest {
    provider
        .transform_request(
            "model",
            &serde_json::json!({
        "messages":[{"role":"user","content":"Hi"}]}),
        )
        .unwrap()
}
async fn header(provider: &Vertex) -> Result<reqwest::header::HeaderValue> {
    let req = prepared(provider);
    let mut request = reqwest::Request::new(reqwest::Method::POST, req.url.parse().unwrap());
    provider
        .transport()
        .unwrap()
        .authorize(&mut request)
        .await?;
    Ok(request.headers()["authorization"].clone())
}
#[tokio::test]
async fn vertex_rejected_tokens_refresh_once_in_current_dispatch_from_fake_endpoint() {
    for (status, expired, repeated, expected_sends) in [
        (401, true, false, 2),
        (403, true, false, 2),
        (401, true, true, 2),
        (403, false, false, 1),
    ] {
        let (provider, calls, _dir) = provider(vec![]);
        let req = prepared(&provider);
        let seen = std::sync::Mutex::new(Vec::new());
        let client = ShimClient::new_scoped().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(60),
            client.send_prepared_using(
                None,
                AttemptKind::Completion,
                "model",
                None,
                &req,
                Some(&provider),
                |builder| Ok(builder.json(&req.body)),
                |_, request| {
                    let mut seen = seen.lock().unwrap();
                    seen.push(request.headers()["authorization"].clone());
                    let first = seen.len() == 1;
                    let rejected = first || repeated;
                    Box::pin(async move {
                        let body = serde_json::json!({"error":{"code":status,
                        "status":if status==401 {"UNAUTHENTICATED"} else {"PERMISSION_DENIED"},
                        "details":if expired {serde_json::json!([{
                            "@type":"type.googleapis.com/google.rpc.ErrorInfo",
                            "domain":"googleapis.com","reason":"ACCESS_TOKEN_EXPIRED"}])}
                            else {serde_json::json!([])}}});
                        Ok(http::Response::builder()
                            .status(if rejected { status } else { 200 })
                            .body(body.to_string().into_bytes())
                            .unwrap()
                            .into())
                    })
                },
            ),
        )
        .await
        .unwrap();
        assert_eq!(result.is_ok(), expired && !repeated);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), expected_sends);
        assert_eq!(calls.load(Ordering::SeqCst), expected_sends);
        assert_eq!(seen[0], "Bearer fixture-0");
        if expected_sends == 2 {
            assert_eq!(seen[1], "Bearer fixture-1");
        }
        assert!(seen.iter().all(reqwest::header::HeaderValue::is_sensitive));
    }
}
#[tokio::test]
async fn vertex_stale_rejection_preserves_new_cached_token() {
    let (provider, calls, _dir) = provider(vec![]);
    let old = header(&provider).await.unwrap();
    let transport = provider.transport().unwrap();
    assert!(transport
        .authorization_rejected(401, "", Some(&old))
        .await
        .unwrap());
    assert_eq!(header(&provider).await.unwrap(), "Bearer fixture-1");
    assert!(transport
        .authorization_rejected(401, "", Some(&old))
        .await
        .unwrap());
    assert_eq!(header(&provider).await.unwrap(), "Bearer fixture-1");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn vertex_failed_refresh_refuses_expired_cached_token() {
    let (provider, calls, _dir) = provider(vec![200, 503]);
    header(&provider).await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(120)).await;
    let awake = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    assert!(header(&provider).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    awake.abort();
}

#[tokio::test]
async fn vertex_rejection_without_retries_invalidates_without_resending() {
    let (provider, calls, _dir) = provider(vec![]);
    let req = prepared(&provider);
    let sends = AtomicUsize::new(0);
    let mut client = ShimClient::new_scoped().unwrap();
    client.retry.max_retries = 0;
    let result = client
        .send_prepared_using(
            None,
            AttemptKind::Completion,
            "model",
            None,
            &req,
            Some(&provider),
            |builder| Ok(builder.json(&req.body)),
            |_, _| {
                sends.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Ok(http::Response::builder()
                        .status(401)
                        .body(b"{}".to_vec())
                        .unwrap()
                        .into())
                })
            },
        )
        .await;
    assert!(result.is_err());
    assert_eq!(sends.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(header(&provider).await.unwrap(), "Bearer fixture-1");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[path = "vertex_auth_tests/refresh_tests.rs"]
mod refresh_tests;

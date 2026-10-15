//! Real dispatch retries use in-memory responses; no socket or cloud client is executed.
use super::*;
use crate::providers::vertex::*;
use std::sync::atomic::{AtomicUsize, Ordering};
struct Endpoint(Arc<AtomicUsize>);
impl TokenSource for Endpoint {
    fn token(&self) -> TokenFuture<'_> {
        Box::pin(async {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            AccessToken::new(&format!("fixture-{n}"), Duration::from_secs(120))
        })
    }
}
#[tokio::test(start_paused = true)]
async fn vertex_http_retry_refreshes_expired_token_before_second_attempt() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Vertex::gemini(GoogleIdentity::token_source(
        "fixture-project",
        "global",
        "chosen",
        Arc::new(Endpoint(calls.clone())),
    ));
    let req = provider
        .transform_request(
            "model",
            &serde_json::json!({
        "messages":[{"role":"user","content":"Hi"}]}),
        )
        .unwrap();
    let seen = std::sync::Mutex::new(Vec::new());
    let client = ShimClient::new_scoped()
        .unwrap()
        .with_attempt_deadlines(AttemptDeadlines {
            response_headers: Duration::from_secs(300),
            unary_attempt_total: Duration::from_secs(600),
            ..Default::default()
        })
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(1200),
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
                Box::pin(async move {
                    if first {
                        tokio::time::sleep(Duration::from_secs(121)).await;
                    }
                    Ok(http::Response::builder()
                        .status(if first { 503 } else { 200 })
                        .body(Vec::<u8>::new())
                        .unwrap()
                        .into())
                })
            },
        ),
    )
    .await
    .unwrap();
    assert!(result.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], "Bearer fixture-0");
    assert_eq!(seen[1], "Bearer fixture-1");
    assert!(seen.iter().all(reqwest::header::HeaderValue::is_sensitive));
}

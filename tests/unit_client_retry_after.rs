//! `Retry-After` used to die at the client: the request-level retry honoured
//! it, but `ShimError::ProviderError` carried a status and a body and no
//! headers, so a caller with its own backoff above this client backed off
//! blind. The parsed header now rides on the error.
//!
//! Each test sets `LLMSHIM_MAX_RETRIES=0` before building its client, so the
//! 429 is returned on the first attempt instead of being retried — the retry
//! path would otherwise sleep the very header these tests assert on.
use llmshim::client::ShimClient;
use llmshim::error::ShimError;
use llmshim::providers::openai_compat::OpenAiCompatible;
use serde_json::json;
use std::time::Duration;

fn client() -> ShimClient {
    std::env::set_var("LLMSHIM_MAX_RETRIES", "0");
    ShimClient::new()
}

async fn limited(server: &mut mockito::ServerGuard, status: usize, retry_after: Option<&str>) {
    let mut mock = server
        .mock("POST", "/chat/completions")
        .with_status(status)
        .with_body("slow down");
    if let Some(value) = retry_after {
        mock = mock.with_header("retry-after", value);
    }
    mock.create_async().await;
}

async fn fail(server: &mockito::ServerGuard) -> ShimError {
    let provider = OpenAiCompatible::new("local", server.url(), None);
    let request = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
    client()
        .completion(&provider, "m", &request)
        .await
        .expect_err("the server refused")
}

#[tokio::test]
async fn a_429_carries_its_retry_after_in_seconds() {
    let mut server = mockito::Server::new_async().await;
    limited(&mut server, 429, Some("7")).await;
    match fail(&server).await {
        ShimError::ProviderError {
            status,
            body,
            retry_after,
        } => {
            assert_eq!(status, 429);
            assert_eq!(body, "slow down", "the existing fields are untouched");
            assert_eq!(retry_after, Some(Duration::from_secs(7)));
        }
        other => panic!("expected a provider error, got {other:?}"),
    }
}

#[tokio::test]
async fn an_http_date_retry_after_becomes_the_time_until_it() {
    let mut server = mockito::Server::new_async().await;
    let when = (chrono::Utc::now() + chrono::Duration::seconds(90)).to_rfc2822();
    limited(&mut server, 503, Some(&when)).await;
    let ShimError::ProviderError { retry_after, .. } = fail(&server).await else {
        panic!("expected a provider error");
    };
    let wait = retry_after.expect("the date parsed");
    assert!(
        (Duration::from_secs(80)..=Duration::from_secs(90)).contains(&wait),
        "a date ninety seconds out is a wait of about ninety seconds, got {wait:?}"
    );
}

#[tokio::test]
async fn no_header_means_no_hint_not_zero() {
    let mut server = mockito::Server::new_async().await;
    limited(&mut server, 429, None).await;
    let ShimError::ProviderError {
        status,
        retry_after,
        ..
    } = fail(&server).await
    else {
        panic!("expected a provider error");
    };
    assert_eq!(status, 429);
    assert_eq!(retry_after, None);
}

#[tokio::test]
async fn garbage_in_the_header_is_no_hint() {
    let mut server = mockito::Server::new_async().await;
    limited(&mut server, 429, Some("soon-ish")).await;
    let ShimError::ProviderError { retry_after, .. } = fail(&server).await else {
        panic!("expected a provider error");
    };
    assert_eq!(retry_after, None);
}

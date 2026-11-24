//! Retry contracts exercise the real dispatch loop with socket-free HTTP responses.
use super::*;
use crate::providers::bedrock::{AwsProfile, Bedrock};
use aws_credential_types::{
    provider::{error::CredentialsError, future, ProvideCredentials},
    Credentials,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
pub(super) struct FailsThenStops {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) timeout: bool,
}
impl ProvideCredentials for FailsThenStops {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(async move {
            let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
            Err(if !first {
                CredentialsError::invalid_configuration("fixture terminal")
            } else if self.timeout {
                CredentialsError::provider_timed_out(Duration::from_secs(30))
            } else {
                CredentialsError::provider_error("fixture transient")
            })
        })
    }
}

#[tokio::test]
async fn credential_failure_retries_but_configuration_failure_stops_before_http() {
    for timeout in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Bedrock::new(AwsProfile::new("fixture", "us-east-1"))
            .with_credential_provider(
                FailsThenStops {
                    calls: calls.clone(),
                    timeout,
                },
                "fixture",
            );
        let req = ProviderRequest {
            url: "https://example.invalid/".into(),
            headers: vec![],
            body: json!({}),
        };
        let client = ShimClient::new_scoped().unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(60),
            client.send_with_transport(&req, &provider),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            crate::providers::bedrock::BedrockError::from_shim(&error)
                .unwrap()
                .status,
            401
        );
    }
}

#[tokio::test]
async fn http_throttle_and_server_retry_resign_each_exact_body() {
    for status in [429, 503] {
        let provider = Bedrock::new(AwsProfile::new("fixture", "us-east-1"))
            .with_credential_provider(
                Credentials::new("FIXTURE", "secret", None, None, "fixture"),
                "fixture",
            );
        let req = ProviderRequest {
            url: "https://example.invalid/".into(),
            headers: vec![],
            body: json!({"content":"é"}),
        };
        let seen = std::sync::Mutex::new(Vec::new());
        let client = ShimClient::new_scoped().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(60),
            client.send_prepared_using(
                None,
                AttemptKind::Completion,
                "",
                None,
                &req,
                Some(&provider),
                |builder| Ok(builder.json(&req.body)),
                |_, request| {
                    let mut seen = seen.lock().unwrap();
                    seen.push((
                        request.headers()["x-amz-date"].clone(),
                        request.headers()["authorization"].clone(),
                        request.body().unwrap().as_bytes().unwrap().to_vec(),
                    ));
                    let first = seen.len() == 1;
                    Box::pin(async move {
                        // SigV4 dates have second resolution; cross one tick before retrying.
                        if first {
                            tokio::time::sleep(Duration::from_millis(1100)).await;
                        }
                        Ok(http::Response::builder()
                            .status(if first { status } else { 200 })
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
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_ne!(seen[0].0, seen[1].0);
        assert_ne!(seen[0].1, seen[1].1);
        assert_eq!(seen[0].2, seen[1].2);
        assert_eq!(
            serde_json::from_slice::<Value>(&seen[0].2).unwrap(),
            req.body
        );
    }
}

#[derive(Debug)]
struct StallsThenStops(Arc<AtomicUsize>);
impl ProvideCredentials for StallsThenStops {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(async move {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                std::future::pending::<()>().await;
            }
            Err(CredentialsError::invalid_configuration("fixture terminal"))
        })
    }
}
#[tokio::test(start_paused = true)]
async fn authorization_deadline_retries_with_typed_timeout_without_http() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Bedrock::new(AwsProfile::new("fixture", "us-east-1"))
        .with_credential_provider(StallsThenStops(calls.clone()), "fixture");
    let client = ShimClient::new_scoped()
        .unwrap()
        .with_attempt_deadlines(
            AttemptDeadlines::default()
                .with_response_header_timeout(Duration::from_secs(2))
                .unwrap(),
        )
        .unwrap();
    let req = ProviderRequest {
        url: "https://example.invalid/".into(),
        headers: vec![],
        body: json!({}),
    };
    let error = tokio::time::timeout(
        Duration::from_secs(60),
        client.send_with_transport(&req, &provider),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        crate::providers::bedrock::BedrockError::from_shim(&error)
            .unwrap()
            .status,
        401
    );
    use crate::transport::RequestTransport;
    let timeout =
        crate::providers::bedrock::BedrockError::from_shim(&provider.authorization_timeout())
            .unwrap();
    assert_eq!(timeout.status, 503);
    assert_eq!(
        timeout.credential_reason,
        Some(crate::providers::bedrock::CredentialReason::Timeout)
    );
}

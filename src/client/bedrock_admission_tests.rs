//! Unsent credential failures do not consume admission capacity or liability.
use super::bedrock_retry_tests::FailsThenStops;
use super::*;
use crate::providers::bedrock::{AwsProfile, Bedrock};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

struct NoReservation(Arc<AtomicUsize>);
impl crate::policy::AttemptPolicy for NoReservation {
    fn acquire<'a>(
        &'a self,
        _: &'a crate::policy::PreparedAttempt<'a>,
    ) -> crate::policy::AttemptPolicyFuture<
        'a,
        std::result::Result<(), crate::policy::AttemptPolicyRefusal>,
    > {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(crate::policy::AttemptPolicyRefusal::new(
                crate::policy::AttemptPolicyRefusalKind::Budget,
                None,
            ))
        })
    }
    fn observe<'a>(
        &'a self,
        _: &'a crate::policy::AttemptIdentity,
        _: crate::policy::AttemptEvent<'a>,
    ) -> crate::policy::AttemptPolicyFuture<
        'a,
        std::result::Result<(), crate::policy::AttemptPolicyError>,
    > {
        Box::pin(async { panic!("no attempt should exist") })
    }
    fn observe_abandoned(
        &self,
        _: &crate::policy::AttemptIdentity,
        _: crate::policy::AttemptOutcome,
    ) -> std::result::Result<(), crate::policy::AttemptPolicyError> {
        panic!("no reservation should exist")
    }
}
#[tokio::test]
async fn failed_credentials_never_acquire_budget_or_rate_tokens() {
    let credentials = Arc::new(AtomicUsize::new(0));
    let reservations = Arc::new(AtomicUsize::new(0));
    let provider = Bedrock::new(AwsProfile::new("fixture", "us-east-1")).with_credential_provider(
        FailsThenStops {
            calls: credentials.clone(),
            timeout: false,
        },
        "fixture",
    );
    let policy = DispatchPolicyContext::new(Arc::new(NoReservation(reservations.clone())));
    let client = ShimClient::new_scoped().unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(60),
        client.completion_with_policy(
            &provider,
            "model",
            &json!({"messages":[{"role":"user","content":"Hi"}]}),
            &policy,
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(reservations.load(Ordering::SeqCst), 0);
    assert_eq!(credentials.load(Ordering::SeqCst), 2);
    assert_eq!(
        crate::providers::bedrock::BedrockError::from_shim(&error)
            .unwrap()
            .status,
        401
    );
}

#[derive(Debug)]
struct SlowCredentials;
impl aws_credential_types::provider::ProvideCredentials for SlowCredentials {
    fn provide_credentials<'a>(
        &'a self,
    ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        aws_credential_types::provider::future::ProvideCredentials::new(async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(aws_credential_types::Credentials::new(
                "FIXTURE", "secret", None, None, "fixture",
            ))
        })
    }
}
#[tokio::test(start_paused = true)]
async fn credential_loading_leaves_a_fresh_response_header_budget() {
    let provider = Bedrock::new(AwsProfile::new("fixture", "us-east-1"))
        .with_credential_provider(SlowCredentials, "fixture");
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
    let calls = AtomicUsize::new(0);
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
                assert!(request.headers().contains_key("authorization"));
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    Ok(http::Response::builder()
                        .status(200)
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
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn malformed_credential_headers_fail_before_policy_admission() {
    for (key, token) in [("BAD\nKEY", None), ("FIXTURE", Some("BAD\nTOKEN".into()))] {
        let reservations = Arc::new(AtomicUsize::new(0));
        let provider = Bedrock::new(AwsProfile::new("fixture", "us-east-1"))
            .with_credential_provider(
                aws_credential_types::Credentials::new(key, "secret", token, None, "fixture"),
                "fixture",
            );
        let policy = DispatchPolicyContext::new(Arc::new(NoReservation(reservations.clone())));
        let error = ShimClient::new_scoped()
            .unwrap()
            .completion_with_policy(
                &provider,
                "model",
                &json!({"messages":[{"role":"user","content":"Hi"}]}),
                &policy,
            )
            .await
            .unwrap_err();
        assert_eq!(reservations.load(Ordering::SeqCst), 0);
        let error = crate::providers::bedrock::BedrockError::from_shim(&error).unwrap();
        assert_eq!(
            error.credential_reason,
            Some(crate::providers::bedrock::CredentialReason::InvalidConfiguration)
        );
    }
}

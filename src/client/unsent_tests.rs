//! The dispatch loop reports exact zero only when execution never started.
use super::super::*;
use crate::policy::{
    AttemptAccounting, AttemptEvent, AttemptIdentity, AttemptPolicy, AttemptPolicyFuture,
    AttemptPolicyRefusal, AttemptUsageObservation, PreparedAttempt,
};
use crate::transport::{
    AuthorizationFuture, ByteStream, EventStream, PreparedAuthorization, RequestTransport,
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

struct FailingSigner;
impl PreparedAuthorization for FailingSigner {
    fn sign(self: Box<Self>, _: &mut reqwest::Request) -> Result<()> {
        Err(crate::error::provider_error(400, "fixture signing failure"))
    }
}
struct LocalFailure;
impl RequestTransport for LocalFailure {
    fn prepare_authorization<'a>(&'a self, _: &'a mut reqwest::Request) -> AuthorizationFuture<'a> {
        Box::pin(async {
            Ok(Some(
                Box::new(FailingSigner) as Box<dyn PreparedAuthorization>
            ))
        })
    }
    fn authorize<'a>(
        &'a self,
        _: &'a mut reqwest::Request,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async { panic!("preparation already supplied the signer") })
    }
    fn events(&self, _: &str, _: ByteStream) -> EventStream {
        panic!("no HTTP response exists")
    }
    fn error(&self, _: u16, _: &HeaderMap, _: String, _: Option<Duration>) -> ShimError {
        panic!("no HTTP response exists")
    }
}
#[derive(Default)]
struct Settlement {
    zero: AtomicBool,
    outcomes: Mutex<Vec<AttemptOutcome>>,
}
impl AttemptPolicy for Settlement {
    fn acquire<'a>(
        &'a self,
        _: &'a PreparedAttempt<'a>,
    ) -> AttemptPolicyFuture<'a, std::result::Result<(), AttemptPolicyRefusal>> {
        Box::pin(async { Ok(()) })
    }
    fn observe_usage<'a>(
        &'a self,
        _: &'a AttemptIdentity,
        observation: AttemptUsageObservation<'a>,
    ) -> AttemptPolicyFuture<'a, std::result::Result<(), AttemptPolicyError>> {
        Box::pin(async move {
            assert!(
                observation.terminal()
                    && observation.counters_complete()
                    && observation.explicit_zero()
            );
            assert_eq!(
                observation.usage(),
                &json!({"prompt_tokens":0,"completion_tokens":0,
                "total_tokens":0,"uncached_input_tokens":0,
                "cache_read_tokens":0,"cache_write_tokens":0})
            );
            assert!(observation.usage().get("cost_usd").is_none());
            self.zero.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
    fn observe<'a>(
        &'a self,
        _: &'a AttemptIdentity,
        event: AttemptEvent<'a>,
    ) -> AttemptPolicyFuture<'a, std::result::Result<(), AttemptPolicyError>> {
        Box::pin(async move {
            if let AttemptEvent::Finished(outcome) = event {
                self.outcomes.lock().unwrap().push(outcome);
            }
            Ok(())
        })
    }
    fn observe_abandoned(
        &self,
        _: &AttemptIdentity,
        outcome: AttemptOutcome,
    ) -> std::result::Result<(), AttemptPolicyError> {
        self.outcomes.lock().unwrap().push(outcome);
        Ok(())
    }
}
#[tokio::test]
async fn local_signing_failure_settles_observed_zero_without_executing_http() {
    let state = Arc::new(Settlement::default());
    let policy = DispatchPolicyContext::new(state.clone());
    let target = ReplayTarget::new("fixture", "model", crate::reasoning::WireFormat::OpenAiChat);
    let req = ProviderRequest {
        url: "https://example.invalid/".into(),
        headers: vec![],
        body: json!({}),
    };
    let client = ShimClient::new_scoped().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        client.send_prepared_using(
            Some(&policy),
            AttemptKind::Completion,
            "model",
            Some(&target),
            &req,
            Some(&LocalFailure),
            |builder| Ok(builder.json(&req.body)),
            |_, _| Box::pin(async { panic!("must not execute") }),
        ),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(state.zero.load(Ordering::SeqCst));
    assert_eq!(
        *state.outcomes.lock().unwrap(),
        vec![AttemptOutcome::InvalidResponse {
            accounting: AttemptAccounting::UsageObserved
        }]
    );
}

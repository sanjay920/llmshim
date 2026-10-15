//! Real authorization and retry dispatch; the fake endpoint and HTTP executor never use a socket.
use super::*;
use crate::providers::azure::*;
use std::sync::atomic::{AtomicUsize, Ordering};
struct Endpoint(Arc<AtomicUsize>);
impl TokenSource for Endpoint {
    fn token(&self, audience: Audience) -> TokenFuture<'_> {
        Box::pin(async move {
            assert_eq!(audience, Audience::Foundry);
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            AccessToken::new(&format!("fixture-{n}"), Duration::from_secs(120))
        })
    }
}
#[tokio::test(start_paused = true)]
async fn azure_dispatch_refreshes_once_on_rejection_and_after_expiry_with_sensitive_headers() {
    for (status, expired, repeated, retries, key, expected) in [
        (401, true, false, 3, false, 2),
        (403, true, false, 3, false, 2),
        (401, true, true, 3, false, 2),
        (403, false, false, 3, false, 1),
        (401, true, false, 0, false, 1),
        (503, true, false, 3, false, 2),
        (401, true, false, 3, true, 1),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let identity = if key {
            AzureIdentity::api_key("selected", "fixture-key").unwrap()
        } else {
            AzureIdentity::entra(
                "selected",
                Audience::Foundry,
                Arc::new(Endpoint(calls.clone())),
            )
            .unwrap()
        };
        let provider = Azure::new("https://fixture.services.ai.azure.com", identity)
            .unwrap()
            .with_deployment("chat-deployment", "gpt-6-sol")
            .unwrap();
        let req = provider
            .transform_request(
                "chat-deployment",
                &serde_json::json!({
            "messages":[{"role":"user","content":"Hi"}]}),
            )
            .unwrap();
        let mut client = ShimClient::new_scoped()
            .unwrap()
            .with_attempt_deadlines(AttemptDeadlines {
                response_headers: Duration::from_secs(300),
                unary_attempt_total: Duration::from_secs(600),
                ..Default::default()
            })
            .unwrap();
        client.retry.max_retries = retries;
        let seen = std::sync::Mutex::new(Vec::new());
        let attempt = client.send_prepared_using(
            None,
            AttemptKind::Completion,
            "chat-deployment",
            None,
            &req,
            Some(&provider),
            |builder| Ok(builder.json(&req.body)),
            |_, request| {
                assert_eq!(request.url().as_str(), req.url);
                let bytes = request.body().unwrap().as_bytes().unwrap();
                let body: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                assert_eq!(body["model"], "chat-deployment");
                assert!(!request.headers().contains_key(if key {
                    "authorization"
                } else {
                    "api-key"
                }));
                let mut seen = seen.lock().unwrap();
                seen.push(request.headers()[if key { "api-key" } else { "authorization" }].clone());
                let first = seen.len() == 1;
                Box::pin(async move {
                    if first && status == 503 {
                        tokio::time::sleep(Duration::from_secs(121)).await;
                    }
                    let code = if expired {
                        "ExpiredAuthenticationToken"
                    } else {
                        "Forbidden"
                    };
                    let body =
                        serde_json::json!({"error":{"code":code,"message":"secret-fixture"}});
                    Ok(http::Response::builder()
                        .status(if first || repeated { status } else { 200 })
                        .body(body.to_string().into_bytes())
                        .unwrap()
                        .into())
                })
            },
        );
        let result = tokio::time::timeout(Duration::from_secs(1200), attempt)
            .await
            .unwrap();
        assert_eq!(result.is_ok(), expected == 2 && !repeated);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), expected);
        assert_eq!(calls.load(Ordering::SeqCst), if key { 0 } else { expected });
        assert!(seen.iter().all(reqwest::header::HeaderValue::is_sensitive));
        if !key {
            assert_eq!(seen[0], "Bearer fixture-0");
            if expected == 2 {
                assert_eq!(seen[1], "Bearer fixture-1");
            }
        }
    }
}

struct PricingObserver {
    model: String,
    expected: f64,
    observed: AtomicUsize,
}
impl crate::policy::AttemptPolicy for PricingObserver {
    fn observe_abandoned(
        &self,
        _: &crate::policy::AttemptIdentity,
        _: AttemptOutcome,
    ) -> std::result::Result<(), AttemptPolicyError> {
        Ok(())
    }
    fn acquire<'a>(
        &'a self,
        attempt: &'a crate::policy::PreparedAttempt<'a>,
    ) -> crate::policy::AttemptPolicyFuture<'a, std::result::Result<(), AttemptPolicyRefusal>> {
        Box::pin(async move {
            assert_eq!(attempt.identity().pricing_model(), self.model);
            assert_ne!(attempt.identity().native_model(), self.model);
            assert!(crate::cost::is_priceable(
                "azure",
                attempt.identity().pricing_model()
            ));
            Ok(())
        })
    }
    fn observe<'a>(
        &'a self,
        _: &'a crate::policy::AttemptIdentity,
        event: crate::policy::AttemptEvent<'a>,
    ) -> crate::policy::AttemptPolicyFuture<'a, std::result::Result<(), AttemptPolicyError>> {
        Box::pin(async move {
            if let crate::policy::AttemptEvent::Usage { usage } = event {
                assert_eq!(usage["cost_usd"], self.expected);
                self.observed.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn azure_client_accounting_and_admission_use_the_mapped_azure_price() {
    for (deployment, model) in [
        ("claude-sonnet-4-6", "claude-opus-4-6"),
        ("claude-opus-4-6", "claude-sonnet-4-6"),
    ] {
        let provider = Azure::new(
            "https://fixture.openai.azure.com",
            AzureIdentity::api_key("selected", "fixture-key").unwrap(),
        )
        .unwrap()
        .with_deployment(deployment, model)
        .unwrap();
        let target = provider.replay_target(deployment);
        let native = serde_json::json!({"usage":{"prompt_tokens":100,"completion_tokens":20}});
        let observer = Arc::new(PricingObserver {
            model: model.into(),
            expected: crate::cost::cost_usd("azure", model, &native["usage"]).unwrap(),
            observed: AtomicUsize::new(0),
        });
        let context = DispatchPolicyContext::new(observer.clone());
        let mut tracker = Some(
            context
                .acquire(
                    AttemptKind::Completion,
                    deployment,
                    &target,
                    "https://fixture.openai.azure.com/openai/v1/chat/completions",
                    &serde_json::json!({"model":deployment,"max_tokens":20}),
                )
                .await
                .unwrap(),
        );
        assert!(observe_native_response_usage(
            &target,
            &native,
            &mut tracker,
            Duration::from_secs(30),
            tokio::time::Instant::now() + Duration::from_secs(300),
        )
        .await
        .is_ok());
        assert_eq!(observer.observed.load(Ordering::SeqCst), 1);
    }
}

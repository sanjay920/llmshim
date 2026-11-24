use super::{body, AttemptKind, DispatchFailure, ShimClient};
use crate::{error::Result, images::ImageResponse, provider::Provider};
use serde_json::Value;

/// The usage's `cost_source`, or null when the image response wrote `"unknown"` for a price it could
/// not apply: the documented sources are `provider`, `provider_floor` and `catalog`, and an unknown
/// price is a null, never a fourth source (note 0036).
pub(crate) fn known_cost_source(usage: &Value) -> Value {
    match usage.get("cost_source") {
        Some(Value::String(source)) if source != "unknown" => Value::String(source.clone()),
        _ => Value::Null,
    }
}

impl ShimClient {
    /// [`Self::images`] under a trusted attempt policy (note 0036): the attempt is admitted by the
    /// policy (rate, concurrency, spend) before it is sent and reported to it as it ends, as a
    /// completion is.
    pub async fn images_with_policy(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &Value,
        policy_context: &crate::policy::DispatchPolicyContext,
    ) -> Result<ImageResponse> {
        crate::images::validate(request)?;
        let prepared = provider.image_request(model, request)?;
        let target = provider.request_replay_target(model, &prepared);
        let result = async {
            let super::AttemptResponse {
                response,
                mut tracker,
                attempt_deadline,
            } = self
                .send_prepared(
                    Some(policy_context),
                    AttemptKind::Completion,
                    model,
                    Some(&target),
                    &prepared,
                    provider.transport().as_deref(),
                )
                .await?;
            let mut body = match body::read_json(
                response,
                self.response_body_limits.success_bytes,
                self.deadlines.unary_body_idle,
                attempt_deadline,
            )
            .await
            {
                Ok(body) => body,
                Err(error) => {
                    super::finish_invalid_response(
                        &mut tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    return Err(error.into_dispatch_failure());
                }
            };
            if provider.name() == "openai"
                && body.is_object()
                && body.get("output_format").is_none()
            {
                if let Some(format) = prepared.body.get("output_format") {
                    body["output_format"] = format.clone();
                }
            }
            // The counters come from the native body, normalized for its wire; read before the
            // adapter takes the body (an image answer can be tens of megabytes: no copy).
            let observation =
                crate::usage::normalize_native_response_usage_observation(&target, &body);
            let images = match provider.image_response(model, body) {
                Ok(images) => images,
                Err(error) => {
                    super::finish_invalid_response(
                        &mut tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    // A content outcome (a refused prompt, a text-only answer, a safety stop) is
                    // the caller's request being declined, not the provider failing: it must not
                    // count toward the provider's breaker, which every tenant shares.
                    return Err(DispatchFailure::Local(error));
                }
            };
            // The counters come from the native body, normalized for its wire; the price is the
            // image response's own, which applies a rate only to fully classified usage and is
            // null when unknown. The generic per-direction stamp would price image tokens at the
            // text rate.
            if let Some(tracker) = tracker.as_mut() {
                if let Some(mut observation) = observation {
                    observation.usage["cost_usd"] =
                        images.usage.get("cost_usd").cloned().unwrap_or(Value::Null);
                    observation.usage["cost_source"] = known_cost_source(&images.usage);
                    let usage = tracker.usage(
                        &observation.usage,
                        observation.terminal,
                        observation.counters_complete,
                        observation.explicit_zero,
                    );
                    super::bounded_policy(usage, self.deadlines.policy_callback, attempt_deadline)
                        .await
                        .map_err(|_| super::coordinator_timeout_observation())?
                        .map_err(DispatchFailure::PolicyObservation)?;
                }
            }
            super::finish_completed_response(
                &mut tracker,
                self.deadlines.policy_callback,
                attempt_deadline,
            )
            .await?;
            Ok(images)
        }
        .await;
        if let Some(outcome) = Self::breaker_outcome(&result) {
            self.observe(provider, outcome).await;
        }
        result.map_err(DispatchFailure::into_public)
    }

    pub async fn images(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &Value,
    ) -> Result<ImageResponse> {
        crate::images::validate(request)?;
        let prepared = provider.image_request(model, request)?;
        let result = async {
            let attempt = self
                .send_prepared(None, AttemptKind::Completion, model, None, &prepared, None)
                .await?;
            let mut response = body::read_json(
                attempt.response,
                self.response_body_limits.success_bytes,
                self.deadlines.unary_body_idle,
                attempt.attempt_deadline,
            )
            .await
            .map_err(body::BodyReadError::into_dispatch_failure)?;
            if provider.name() == "openai"
                && response.is_object()
                && response.get("output_format").is_none()
            {
                if let Some(format) = prepared.body.get("output_format") {
                    response["output_format"] = format.clone();
                }
            }
            provider
                .image_response(model, response)
                .map_err(DispatchFailure::Upstream)
        }
        .await;
        if let Some(outcome) = Self::breaker_outcome(&result) {
            self.observe(provider, outcome).await;
        }
        result.map_err(DispatchFailure::into_public)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        breaker::{BreakerConfig, ProviderBreaker},
        error::ShimError,
        providers::openai::OpenAi,
    };
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn image_body_limits_preserve_health_but_upstream_failures_trip_breaker() {
        let mut server = mockito::Server::new_async().await;
        let provider = OpenAi::new("key".into()).with_base_url(server.url());
        for (status, body, expected_error, unhealthy) in [
            (
                200,
                " ".repeat(256 * 1024 + 1),
                Some((502, "upstream response body exceeds size limit")),
                false,
            ),
            (
                200,
                format!("[{}0]", "0,".repeat(65_536)),
                Some((502, "upstream JSON exceeds complexity limit")),
                false,
            ),
            (
                200,
                json!({"data":[{"b64_json":"AQ=="}]}).to_string(),
                None,
                false,
            ),
            (
                200,
                json!({"data":[]}).to_string(),
                Some((502, "provider returned no generated images")),
                true,
            ),
            (503, "unavailable".into(), Some((503, "unavailable")), true),
        ] {
            let breaker = Arc::new(ProviderBreaker::with_config(BreakerConfig {
                trip_threshold: 1,
                ..BreakerConfig::default()
            }));
            let client = ShimClient {
                response_body_limits: body::ResponseBodyLimits {
                    success_bytes: 256 * 1024,
                    ..body::ResponseBodyLimits::default()
                },
                retry: super::super::RetryConfig {
                    max_retries: 0,
                    ..super::super::RetryConfig::default()
                },
                ..ShimClient::new().with_breaker(breaker.clone())
            };
            let mock = server
                .mock("POST", "/images/generations")
                .with_status(status)
                .with_body(body)
                .expect(1)
                .create_async()
                .await;
            let result = client
                .images(&provider, "gpt-image-1", &json!({"prompt":"cat"}))
                .await;
            if let Some((status, message)) = expected_error {
                assert!(
                    matches!(result, Err(ShimError::ProviderError { status: actual, body, .. }) if actual == status && body == message)
                );
            } else {
                assert_eq!(result.unwrap().images[0].bytes, [1]);
            }
            assert_eq!(breaker.should_skip("openai"), unhealthy);
            mock.assert_async().await;
            mock.remove_async().await;
        }
    }

    /// Admits everything and observes nothing: the policy's own behaviour is not under test.
    struct Open;
    impl crate::policy::AttemptPolicy for Open {
        fn acquire<'a>(
            &'a self,
            _: &'a crate::policy::PreparedAttempt<'a>,
        ) -> crate::policy::AttemptPolicyFuture<
            'a,
            std::result::Result<(), crate::policy::AttemptPolicyRefusal>,
        > {
            Box::pin(async { Ok(()) })
        }

        fn observe<'a>(
            &'a self,
            _: &'a crate::policy::AttemptIdentity,
            _: crate::policy::AttemptEvent<'a>,
        ) -> crate::policy::AttemptPolicyFuture<
            'a,
            std::result::Result<(), crate::policy::AttemptPolicyError>,
        > {
            Box::pin(async { Ok(()) })
        }

        fn observe_abandoned(
            &self,
            _: &crate::policy::AttemptIdentity,
            _: crate::policy::AttemptOutcome,
        ) -> std::result::Result<(), crate::policy::AttemptPolicyError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_declined_prompt_under_a_policy_is_an_error_but_not_a_provider_failure() {
        // A 200 with no image in it (a refusal, a safety stop, a text-only answer) is the caller's
        // request being declined. The breaker is shared by every tenant of the provider, so three
        // of those must not open it; a real 503 still does.
        let mut server = mockito::Server::new_async().await;
        let provider = OpenAi::new("key".into()).with_base_url(server.url());
        let context = crate::policy::DispatchPolicyContext::new(Arc::new(Open));
        for (status, body, unhealthy) in [
            (200, json!({"data": []}).to_string(), false),
            (503, "unavailable".into(), true),
        ] {
            let breaker = Arc::new(ProviderBreaker::with_config(BreakerConfig {
                trip_threshold: 1,
                ..BreakerConfig::default()
            }));
            let client = ShimClient {
                retry: super::super::RetryConfig {
                    max_retries: 0,
                    ..super::super::RetryConfig::default()
                },
                ..ShimClient::new().with_breaker(breaker.clone())
            };
            let mock = server
                .mock("POST", "/images/generations")
                .with_status(status)
                .with_body(body)
                .expect(1)
                .create_async()
                .await;
            let result = client
                .images_with_policy(
                    &provider,
                    "gpt-image-1",
                    &json!({"prompt": "cat"}),
                    &context,
                )
                .await;
            assert!(result.is_err(), "{status}");
            assert_eq!(breaker.should_skip("openai"), unhealthy, "{status}");
            mock.assert_async().await;
            mock.remove_async().await;
        }
    }

    #[test]
    fn an_unknown_price_is_a_null_source_never_a_fourth_one() {
        for (usage, expected) in [
            (json!({"cost_source": "unknown"}), Value::Null),
            (json!({}), Value::Null),
            (json!({"cost_source": "catalog"}), json!("catalog")),
            (json!({"cost_source": "provider"}), json!("provider")),
        ] {
            assert_eq!(known_cost_source(&usage), expected, "{usage}");
        }
    }
}

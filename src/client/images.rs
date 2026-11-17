use super::{body, AttemptKind, DispatchFailure, ShimClient};
use crate::{error::Result, images::ImageResponse, provider::Provider};
use serde_json::Value;

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
            let images = match provider.image_response(model, body.clone()) {
                Ok(images) => images,
                Err(error) => {
                    super::finish_invalid_response(
                        &mut tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    return Err(DispatchFailure::Upstream(error));
                }
            };
            // The counters come from the native body, normalized for its wire; the price is the
            // image response's own, which applies a rate only to fully classified usage and is
            // null when unknown. The generic per-direction stamp would price image tokens at the
            // text rate.
            if let Some(tracker) = tracker.as_mut() {
                if let Some(mut observation) =
                    crate::usage::normalize_native_response_usage_observation(&target, &body)
                {
                    for key in ["cost_usd", "cost_source"] {
                        observation.usage[key] =
                            images.usage.get(key).cloned().unwrap_or(Value::Null);
                    }
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
}

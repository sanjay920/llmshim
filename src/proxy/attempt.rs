use super::ratelimit::{penalty_duration, Backpressure, RateKey, RateLimiter};
use crate::policy::{
    AttemptEvent, AttemptIdentity, AttemptOutcome, AttemptPolicy, AttemptPolicyError,
    AttemptPolicyFuture, AttemptPolicyRefusal, AttemptPolicyRefusalKind, DispatchPolicyContext,
    PreparedAttempt,
};
use crate::reasoning::WireFormat;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::OwnedSemaphorePermit;

struct ProxyAttemptPolicy {
    limiter: Arc<dyn RateLimiter>,
    backpressure: Backpressure,
    active_permits: Mutex<HashMap<uuid::Uuid, OwnedSemaphorePermit>>,
}

pub(crate) fn context(
    limiter: Arc<dyn RateLimiter>,
    backpressure: Backpressure,
) -> DispatchPolicyContext {
    DispatchPolicyContext::new(Arc::new(ProxyAttemptPolicy {
        limiter,
        backpressure,
        active_permits: Mutex::new(HashMap::new()),
    }))
}

impl AttemptPolicy for ProxyAttemptPolicy {
    fn acquire<'a>(
        &'a self,
        attempt: &'a PreparedAttempt<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
        Box::pin(async move {
            let permit = self.backpressure.acquire().await.map_err(|_| {
                AttemptPolicyRefusal::new(AttemptPolicyRefusalKind::CoordinatorUnavailable, None)
            })?;
            let key = RateKey::provider(attempt.identity().provider_name());
            self.limiter
                .acquire(&key, estimate_prepared_attempt_tokens(attempt))
                .await
                .map_err(|retry| {
                    AttemptPolicyRefusal::new(
                        AttemptPolicyRefusalKind::ProviderLimit,
                        Some(retry.0),
                    )
                })?;
            self.active_permits
                .lock()
                .unwrap()
                .insert(attempt.identity().id(), permit);
            Ok(())
        })
    }

    fn observe<'a>(
        &'a self,
        attempt: &'a AttemptIdentity,
        event: AttemptEvent<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
        Box::pin(async move {
            match event {
                AttemptEvent::ResponseHeaders { status: 429 } => {
                    self.limiter
                        .penalize(
                            &RateKey::provider(attempt.provider_name()),
                            penalty_duration(),
                        )
                        .await;
                    Ok(())
                }
                AttemptEvent::Finished(_) => {
                    self.active_permits.lock().unwrap().remove(&attempt.id());
                    Ok(())
                }
                AttemptEvent::ResponseHeaders { .. } | AttemptEvent::Usage { .. } => Ok(()),
            }
        })
    }

    fn observe_abandoned(
        &self,
        attempt: &AttemptIdentity,
        _outcome: AttemptOutcome,
    ) -> Result<(), AttemptPolicyError> {
        self.active_permits.lock().unwrap().remove(&attempt.id());
        Ok(())
    }
}

fn estimate_prepared_attempt_tokens(attempt: &PreparedAttempt<'_>) -> u32 {
    let native_body = attempt.native_body();
    let prompt_tokens = serde_json::to_string(native_body)
        .map(|serialized| serialized.len() as u64 / 4)
        .unwrap_or(u64::MAX);
    let explicit_output = match attempt.identity().wire() {
        WireFormat::AnthropicMessages => native_body.get("max_tokens").and_then(|v| v.as_u64()),
        WireFormat::OpenAiResponses => native_body
            .get("max_output_tokens")
            .and_then(|v| v.as_u64()),
        WireFormat::OpenAiChat => native_body
            .get("max_completion_tokens")
            .or_else(|| native_body.get("max_tokens"))
            .and_then(|v| v.as_u64()),
        WireFormat::GoogleGenerateContent => native_body
            .pointer("/generationConfig/maxOutputTokens")
            .and_then(|v| v.as_u64()),
    };
    let reasoning_output = match attempt.identity().wire() {
        WireFormat::AnthropicMessages => native_body
            .pointer("/thinking/budget_tokens")
            .and_then(|v| v.as_u64()),
        WireFormat::GoogleGenerateContent => native_body
            .pointer("/generationConfig/thinkingConfig/thinkingBudget")
            .and_then(|v| v.as_u64()),
        WireFormat::OpenAiResponses | WireFormat::OpenAiChat => native_body
            .pointer("/reasoning/max_tokens")
            .and_then(|v| v.as_u64()),
    }
    .unwrap_or_default();
    let default_output = crate::catalog::resolve(&format!(
        "{}/{}",
        attempt.identity().provider_name(),
        attempt.identity().native_model()
    ))
    .or_else(|| crate::catalog::resolve(attempt.identity().native_model()))
    .and_then(|model| model.max_output_tokens)
    .map(u64::from)
    .unwrap_or_else(|| match attempt.identity().provider_name() {
        "anthropic" => 8_192,
        "gemini" => 65_536,
        "openai" | "chatgpt" | "xai" => 128_000,
        "openrouter" => 1_048_576,
        _ => 1_024,
    });
    prompt_tokens
        .saturating_add(
            explicit_output
                .unwrap_or(default_output)
                .max(reasoning_output),
        )
        .clamp(1, u32::MAX as u64) as u32
}

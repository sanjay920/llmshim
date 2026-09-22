use super::ratelimit::{penalty_duration, Backpressure, RateKey, RateLimiter};
use crate::policy::{
    AttemptEvent, AttemptIdentity, AttemptOutcome, AttemptPolicy, AttemptPolicyError,
    AttemptPolicyFuture, AttemptPolicyRefusal, AttemptPolicyRefusalKind, DispatchPolicyContext,
    PreparedAttempt,
};
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
    logical_deadline: Option<tokio::time::Instant>,
) -> DispatchPolicyContext {
    let context = DispatchPolicyContext::new(Arc::new(ProxyAttemptPolicy {
        limiter,
        backpressure,
        active_permits: Mutex::new(HashMap::new()),
    }));
    match logical_deadline {
        Some(deadline) => context.with_logical_deadline(deadline),
        None => context,
    }
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
                .acquire(&key, super::ratelimit::estimate_attempt_tokens(attempt))
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

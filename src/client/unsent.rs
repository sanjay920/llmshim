//! A local signing failure precedes HTTP execution, so its usage is independently zero.
use super::*;

pub(super) async fn finish_unsent_request(
    tracker: &mut Option<AttemptTracker>,
    callback_timeout: Duration,
    attempt_deadline: tokio::time::Instant,
) -> DispatchResult<()> {
    if let Some(tracker) = tracker.as_mut() {
        let zero = serde_json::json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0,
            "uncached_input_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0});
        bounded_policy(
            tracker.usage(&zero, true, true, true),
            callback_timeout,
            attempt_deadline,
        )
        .await
        .map_err(|_| coordinator_timeout_observation())?
        .map_err(DispatchFailure::PolicyObservation)?;
    }
    // Keep the error outcome (rather than a completed request) and the existing
    // usage-observed settlement contract. Unpriced fee uncertainty stays conservative.
    finish_invalid_response(tracker, callback_timeout, attempt_deadline).await
}

#[cfg(all(test, feature = "bedrock"))]
#[path = "unsent_tests.rs"]
mod tests;

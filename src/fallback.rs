use crate::error::{Result, ShimError};
use crate::log::{LogEntry, Logger, RequestTimer};
use crate::policy::DispatchPolicyContext;
use crate::router::Router;
use serde_json::Value;
use std::time::Duration;

/// Configuration for retry and fallback behavior.
#[derive(Debug, Clone)]
pub struct FallbackConfig {
    /// Ordered list of model strings to try. The first is the primary.
    pub models: Vec<String>,
    /// Maximum number of retries per model before moving to the next.
    pub max_retries: u32,
    /// Initial backoff duration (doubles on each retry).
    pub initial_backoff: Duration,
    /// HTTP status codes that trigger a retry/fallback (e.g., 429, 500, 502, 503).
    pub retryable_statuses: Vec<u16>,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            models: Vec::new(),
            max_retries: 2,
            initial_backoff: Duration::from_millis(500),
            retryable_statuses: vec![429, 500, 502, 503, 529],
        }
    }
}

impl FallbackConfig {
    pub fn new(models: Vec<String>) -> Self {
        Self {
            models,
            ..Default::default()
        }
    }

    pub fn max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    pub fn initial_backoff(mut self, d: Duration) -> Self {
        self.initial_backoff = d;
        self
    }
}

fn is_retryable(err: &ShimError, retryable_statuses: &[u16]) -> bool {
    match err {
        ShimError::ProviderError { status, .. } => retryable_statuses.contains(status),
        ShimError::Http(_) => true, // network errors are always retryable
        _ => false,
    }
}

/// Run a completion with retry + fallback across multiple models.
pub async fn completion_with_fallback(
    router: &Router,
    request: &Value,
    config: &FallbackConfig,
    logger: Option<&Logger>,
) -> Result<Value> {
    completion_with_fallback_inner(router, request, config, logger, None).await
}

pub async fn completion_with_fallback_and_policy(
    router: &Router,
    request: &Value,
    config: &FallbackConfig,
    logger: Option<&Logger>,
    policy_context: &DispatchPolicyContext,
) -> Result<Value> {
    completion_with_fallback_inner(router, request, config, logger, Some(policy_context)).await
}

async fn completion_with_fallback_inner(
    router: &Router,
    request: &Value,
    config: &FallbackConfig,
    logger: Option<&Logger>,
    policy_context: Option<&DispatchPolicyContext>,
) -> Result<Value> {
    let models = if config.models.is_empty() {
        // No fallback configured — just use the model from the request
        vec![request
            .get("model")
            .and_then(|m| m.as_str())
            .ok_or(ShimError::MissingModel)?
            .to_string()]
    } else {
        config.models.clone()
    };

    let mut errors: Vec<String> = Vec::new();
    // Every attempt below is counted by the client against this router's
    // breaker; the loop only asks `admit` before dialling.
    let client = crate::bound_client(router);

    for model_str in &models {
        // Build request with this model. A named route expands to its model and
        // settings here, so a chain entry may itself be a `route/<name>`.
        let mut req = request.clone();
        req["model"] = Value::String(model_str.clone());
        let req = match router.expand_route(&req) {
            Ok(expanded) => expanded.into_owned(),
            Err(e) => {
                errors.push(format!("{}: {}", model_str, e));
                continue;
            }
        };

        let (provider, model) = match router.resolve(model_str) {
            Ok(r) => r,
            Err(e) => {
                errors.push(format!("{}: {}", model_str, e));
                continue;
            }
        };

        let mut backoff = config.initial_backoff;

        for attempt in 0..=config.max_retries {
            // Provider health, not rate-limit backoff. Checked per attempt, not
            // once per chain entry: the attempt that opens a circuit is usually
            // this loop's own, and continuing to retry past it is exactly the
            // "retrying into a known-dead target" the breaker exists to stop.
            if !router.breaker().admit(provider.name()).await {
                errors.push(format!(
                    "{}: circuit open for provider {}",
                    model_str,
                    provider.name()
                ));
                break; // move to next model
            }

            let timer = RequestTimer::start();
            // Keep OAuth preparation, SSE-only providers, reasoning provenance,
            // and tool normalization identical to an ordinary completion.
            let outcome = match policy_context {
                Some(context) => {
                    client
                        .completion_with_policy(provider, &model, &req, context)
                        .await
                }
                None => client.completion(provider, &model, &req).await,
            };
            match outcome {
                Ok(result) => {
                    if let Some(logger) = logger {
                        logger.log(&LogEntry::from_response(
                            provider.name(),
                            model_str,
                            &result,
                            timer.elapsed(),
                        ));
                    }
                    return Ok(result);
                }
                Err(e) => {
                    if policy_context.is_some() && crate::policy::terminates_fallback(&e) {
                        if let Some(logger) = logger {
                            logger.log(&LogEntry::from_error(
                                provider.name(),
                                model_str,
                                &e.to_string(),
                                timer.elapsed(),
                            ));
                        }
                        return Err(e);
                    }
                    if is_retryable(&e, &config.retryable_statuses) && attempt < config.max_retries
                    {
                        errors.push(format!("{} (attempt {}): {}", model_str, attempt + 1, e));
                        tokio::time::sleep(backoff).await;
                        backoff *= 2;
                        continue;
                    }
                    if let Some(logger) = logger {
                        logger.log(&LogEntry::from_error(
                            provider.name(),
                            model_str,
                            &e.to_string(),
                            timer.elapsed(),
                        ));
                    }
                    errors.push(format!("{}: {}", model_str, e));
                    break; // move to next model
                }
            }
        }
    }

    Err(ShimError::AllFailed(errors))
}

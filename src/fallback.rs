use crate::client::ShimClient;
use crate::error::{Result, ShimError};
use crate::log::{LogEntry, Logger, RequestTimer};
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
    let client = ShimClient::new();

    for model_str in &models {
        // Build request with this model
        let mut req = request.clone();
        req["model"] = Value::String(model_str.clone());

        let (provider, model) = match router.resolve(model_str) {
            Ok(r) => r,
            Err(e) => {
                errors.push(format!("{}: {}", model_str, e));
                continue;
            }
        };

        let mut backoff = config.initial_backoff;

        for attempt in 0..=config.max_retries {
            let timer = RequestTimer::start();
            let provider_req = match provider.transform_request(&model, &req) {
                Ok(r) => r,
                Err(e) => {
                    errors.push(format!("{}: transform error: {}", model_str, e));
                    break; // don't retry transform errors
                }
            };

            match client.send(&provider_req).await {
                Ok(resp) => {
                    let body: Value = match resp.json().await {
                        Ok(b) => b,
                        Err(e) => {
                            errors.push(format!("{}: json parse error: {}", model_str, e));
                            break;
                        }
                    };
                    match provider.transform_response(&model, body) {
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
                            if is_retryable(&e, &config.retryable_statuses)
                                && attempt < config.max_retries
                            {
                                errors.push(format!(
                                    "{} (attempt {}): {}",
                                    model_str,
                                    attempt + 1,
                                    e
                                ));
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
                Err(e) => {
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

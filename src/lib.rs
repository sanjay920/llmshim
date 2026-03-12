pub mod client;
pub mod config;
pub mod env;
pub mod error;
pub mod fallback;
pub mod log;
pub mod models;
pub mod provider;
pub mod providers;
pub mod router;
pub mod vision;

#[cfg(feature = "proxy")]
pub mod proxy;

use client::ShimClient;
use error::Result;
pub use fallback::{completion_with_fallback, FallbackConfig};
use log::{LogEntry, Logger, RequestTimer};
use router::Router;
use serde_json::Value;

use futures::Stream;
use std::pin::Pin;
use std::sync::LazyLock;

/// Shared HTTP client — reuses connection pool across all requests.
/// Shared HTTP client with connection pooling.
pub static SHARED_CLIENT: LazyLock<ShimClient> = LazyLock::new(ShimClient::new);

/// Pre-establish TCP+TLS connections to all configured provider endpoints.
/// Call once after creating the Router to eliminate cold-start latency on first request.
pub async fn warmup(router: &Router) {
    let urls: Vec<&str> = router
        .provider_keys()
        .iter()
        .filter_map(|name| match *name {
            "openai" => Some("https://api.openai.com"),
            "anthropic" => Some("https://api.anthropic.com"),
            "gemini" => Some("https://generativelanguage.googleapis.com"),
            "xai" => Some("https://api.x.ai"),
            _ => None,
        })
        .collect();
    SHARED_CLIENT.warmup(&urls).await;
}

/// Top-level entry point. Resolves the provider from the model string and fires the request.
pub async fn completion(router: &Router, request: &Value) -> Result<Value> {
    completion_with_logger(router, request, None).await
}

/// Completion with optional logging.
pub async fn completion_with_logger(
    router: &Router,
    request: &Value,
    logger: Option<&Logger>,
) -> Result<Value> {
    let model_str = request
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or(error::ShimError::MissingModel)?;

    let (provider, model) = router.resolve(model_str)?;
    let client = &*SHARED_CLIENT;
    let timer = RequestTimer::start();

    match client.completion(provider, &model, request).await {
        Ok(resp) => {
            if let Some(logger) = logger {
                logger.log(&LogEntry::from_response(
                    provider.name(),
                    model_str,
                    &resp,
                    timer.elapsed(),
                ));
            }
            Ok(resp)
        }
        Err(e) => {
            if let Some(logger) = logger {
                logger.log(&LogEntry::from_error(
                    provider.name(),
                    model_str,
                    &e.to_string(),
                    timer.elapsed(),
                ));
            }
            Err(e)
        }
    }
}

/// Streaming entry point. Returns an SSE stream of OpenAI-format chunks.
pub async fn stream(
    router: &Router,
    request: &Value,
) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
    let model_str = request
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or(error::ShimError::MissingModel)?;

    let (provider, model) = router.resolve(model_str)?;
    let client = &*SHARED_CLIENT;
    client.stream(provider, &model, request).await
}

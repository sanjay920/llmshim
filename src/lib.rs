pub mod client;
pub mod error;
pub mod fallback;
pub mod log;
pub mod models;
pub mod provider;
pub mod providers;
pub mod router;

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
    let client = ShimClient::new();
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
    let client = ShimClient::new();
    client.stream(provider, &model, request).await
}

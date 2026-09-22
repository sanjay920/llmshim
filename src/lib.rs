pub mod breaker;
pub mod cache;
pub mod client;
pub mod schema;
pub mod shim;
/// Offline-first model catalog, also available as the standalone `llmshim-catalog` crate.
pub use llmshim_catalog as catalog;
pub mod config;
pub mod cost;
pub mod credentials;
pub mod env;
pub mod error;
pub mod fallback;
pub mod log;
pub mod models;
pub mod policy;
pub mod provider;
pub mod providers;
pub mod reasoning;
pub mod router;
mod sse;
pub mod streaming;
pub mod toolcall;
pub mod usage;
pub mod vision;

#[cfg(feature = "proxy")]
pub mod proxy;

#[cfg(feature = "gateway")]
pub mod gateway;

use client::ShimClient;
use error::Result;
pub use fallback::{completion_with_fallback, completion_with_fallback_and_policy, FallbackConfig};
use log::{LogEntry, Logger, RequestTimer};
use policy::DispatchPolicyContext;
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

pub async fn completion_with_policy(
    router: &Router,
    request: &Value,
    policy_context: &DispatchPolicyContext,
) -> Result<Value> {
    completion_with_logger_and_policy(router, request, None, policy_context).await
}

/// Completion with optional logging.
pub async fn completion_with_logger(
    router: &Router,
    request: &Value,
    logger: Option<&Logger>,
) -> Result<Value> {
    completion_inner(router, request, logger, None).await
}

pub async fn completion_with_logger_and_policy(
    router: &Router,
    request: &Value,
    logger: Option<&Logger>,
    policy_context: &DispatchPolicyContext,
) -> Result<Value> {
    completion_inner(router, request, logger, Some(policy_context)).await
}

async fn completion_inner(
    router: &Router,
    request: &Value,
    logger: Option<&Logger>,
    policy_context: Option<&DispatchPolicyContext>,
) -> Result<Value> {
    // A named route resolves to its model and settings before dispatch.
    let request = router.expand_route(request)?;
    let request = request.as_ref();
    let model_str = request
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or(error::ShimError::MissingModel)?;

    let (provider, model) = router.resolve(model_str)?;
    let client = bound_client(router);
    let timer = RequestTimer::start();

    // Ordinary traffic feeds provider health too, so a chain's first fallback
    // decision is not the first thing that ever noticed a provider is down.
    // The client does the counting; see `ShimClient::with_breaker`.
    let result = match policy_context {
        Some(context) => {
            client
                .completion_with_policy(provider, &model, request, context)
                .await
        }
        None => client.completion(provider, &model, request).await,
    };

    match result {
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
    stream_inner(router, request, None).await
}

pub async fn stream_with_policy(
    router: &Router,
    request: &Value,
    policy_context: &DispatchPolicyContext,
) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
    stream_inner(router, request, Some(policy_context)).await
}

async fn stream_inner(
    router: &Router,
    request: &Value,
    policy_context: Option<&DispatchPolicyContext>,
) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
    let request = router.expand_route(request)?;
    let request = request.as_ref();
    let model_str = request
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or(error::ShimError::MissingModel)?;

    let (provider, model) = router.resolve_owned(model_str)?;
    // The breaker observes a single target without refusing it. An explicit
    // dispatch policy still gates every actual stream-open attempt below.
    match policy_context {
        Some(context) => {
            bound_client(router)
                .stream_owned_with_policy(provider, &model, request, context)
                .await
        }
        None => {
            bound_client(router)
                .stream_owned(provider, &model, request)
                .await
        }
    }
}

/// The shared HTTP client, reporting to this router's breaker. The pool is
/// shared by clone; only the breaker handle is per call.
pub(crate) fn bound_client(router: &Router) -> ShimClient {
    SHARED_CLIENT.clone().with_breaker(router.breaker().clone())
}

pub mod breaker;
pub mod cache;
pub mod client;
pub mod schema;
pub mod shim;
/// Offline-first model catalog, also available as the standalone `llmshim-catalog` crate.
pub use llmshim_catalog as catalog;
pub mod audio;
pub mod config;
pub mod cost;
pub mod credentials;
mod default_secret_file;
pub mod deployment;
mod derived_response;
pub mod embeddings;
pub mod env;
pub mod error;
pub mod fallback;
pub mod images;
mod json_bounds;
pub mod log;
pub mod models;
pub mod policy;
pub mod provider;
pub mod providers;
pub mod reasoning;
#[cfg(feature = "redis-coordination")]
mod redis_operation;
mod responses_tools;
pub mod router;
mod sse;
mod stream_retention;
pub mod streaming;
pub mod toolcall;
pub mod transport;
pub mod usage;
pub mod vision;

#[cfg(feature = "proxy")]
pub mod proxy;

#[cfg(feature = "gateway")]
pub mod gateway;

#[cfg(feature = "serve")]
pub mod serve;

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
    if let Ok(client) = bound_client(router) {
        client.warmup(&urls).await;
    }
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
    let client = bound_client(router)?;
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
                    provider.instance(),
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
                    provider.instance(),
                    model_str,
                    &e.to_string(),
                    timer.elapsed(),
                ));
            }
            Err(e)
        }
    }
}

/// Embedding entry point. Resolves the model id, refuses before dispatch when
/// the provider has no embeddings API or the catalog does not mark the model as
/// an embedding model, and returns one vector per input text in input order.
///
/// Nothing is chunked: a batch over the provider's bounds is refused, because a
/// second request the caller did not ask for is a second charge and a different
/// rate-limit footprint.
pub async fn embeddings(
    router: &Router,
    request: &embeddings::EmbeddingRequest,
) -> Result<embeddings::Embeddings> {
    embeddings_with_client(router, request, &bound_client(router)?).await
}

/// Embedding dispatch through an embedder's scoped transport, with the same model and batch
/// admission.
pub async fn embeddings_with_client(
    router: &Router,
    request: &embeddings::EmbeddingRequest,
    client: &ShimClient,
) -> Result<embeddings::Embeddings> {
    let (provider, model) = router.resolve(&request.model)?;
    let response = client
        .embeddings(provider, &model, &embeddings::payload(request))
        .await?;
    embeddings::parse(provider.instance(), &model, request.input.len(), response)
}

/// An admitted embeddings call (note 0035): `payload` is the OpenAI-shaped body with the caller's
/// `model`, and the answer is the provider-normalized body (`data`, `usage`) that
/// [`embeddings::parse`] reads. The attempt goes through the same policy as a completion.
#[cfg(feature = "gateway")]
pub(crate) async fn embeddings_value_with_policy(
    router: &Router,
    payload: &Value,
    policy_context: &DispatchPolicyContext,
) -> Result<Value> {
    let model_str = payload
        .get("model")
        .and_then(Value::as_str)
        .ok_or(error::ShimError::MissingModel)?;
    let (provider, model) = router.resolve(model_str)?;
    let client = bound_client(router)?;
    client
        .embeddings_with_policy(provider, &model, payload, policy_context)
        .await
}

/// An admitted image generation (note 0036): `payload` is the OpenAI-shaped body with the caller's
/// `model`. The answer is carried through the dispatcher as a JSON value (`images[]` with base64
/// bytes, media type and revised prompt, and the stamped `usage`) and rendered by the route.
#[cfg(feature = "gateway")]
pub(crate) async fn images_value_with_policy(
    router: &Router,
    payload: &Value,
    policy_context: &DispatchPolicyContext,
) -> Result<Value> {
    use base64::Engine as _;
    let model_str = payload
        .get("model")
        .and_then(Value::as_str)
        .ok_or(error::ShimError::MissingModel)?;
    let (provider, model) = router.resolve(model_str)?;
    let client = bound_client(router)?;
    let response = client
        .images_with_policy(provider, &model, payload, policy_context)
        .await?;
    let images: Vec<Value> = response
        .images
        .iter()
        .map(|image| {
            serde_json::json!({
                "b64_json": base64::engine::general_purpose::STANDARD.encode(&image.bytes),
                "media_type": image.media_type,
                "revised_prompt": image.revised_prompt,
            })
        })
        .collect();
    Ok(serde_json::json!({"images": images, "usage": response.usage}))
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

/// Streaming with usage logging. Exactly one `LogEntry` is written per stream:
/// a success entry when the upstream terminal chunk carries a usage object, or
/// one error entry if the stream fails first. A stream that ends without a
/// usage object logs nothing.
pub async fn stream_with_logger(
    router: &Router,
    request: &Value,
    logger: Option<&Logger>,
) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
    stream_with_logger_inner(router, request, logger, None).await
}

pub async fn stream_with_logger_and_policy(
    router: &Router,
    request: &Value,
    logger: Option<&Logger>,
    policy_context: &DispatchPolicyContext,
) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
    stream_with_logger_inner(router, request, logger, Some(policy_context)).await
}

async fn stream_with_logger_inner(
    router: &Router,
    request: &Value,
    logger: Option<&Logger>,
    policy_context: Option<&DispatchPolicyContext>,
) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
    let timer = RequestTimer::start();
    let (provider, model, upstream) = stream_resolved(router, request, policy_context).await?;
    let Some(logger) = logger.cloned() else {
        return Ok(upstream);
    };
    Ok(logger.wrap_stream(&provider, &model, timer, upstream))
}

async fn stream_inner(
    router: &Router,
    request: &Value,
    policy_context: Option<&DispatchPolicyContext>,
) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
    let (_, _, stream) = stream_resolved(router, request, policy_context).await?;
    Ok(stream)
}

/// Resolve the dispatch target and open the upstream stream. Returns the
/// provider name and requested model string alongside the stream so a logger
/// can attribute it without resolving twice.
async fn stream_resolved(
    router: &Router,
    request: &Value,
    policy_context: Option<&DispatchPolicyContext>,
) -> Result<(
    String,
    String,
    Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
)> {
    let request = router.expand_route(request)?;
    let request = request.as_ref();
    let model_str = request
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or(error::ShimError::MissingModel)?;

    let (provider, model) = router.resolve_owned(model_str)?;
    let provider_name = provider.instance().to_string();
    // The breaker observes a single target without refusing it. An explicit
    // dispatch policy still gates every actual stream-open attempt below.
    let stream = match policy_context {
        Some(context) => {
            bound_client(router)?
                .stream_owned_with_policy(provider, &model, request, context)
                .await
        }
        None => {
            bound_client(router)?
                .stream_owned(provider, &model, request)
                .await
        }
    }?;
    Ok((provider_name, model_str.to_string(), stream))
}

/// A client reporting to this router's breaker. Ambient routers share the pool
/// by clone; scoped routers build a fallible transport without host proxies.
pub(crate) fn bound_client(router: &Router) -> Result<ShimClient> {
    let client = match router.http_client() {
        // The router's own client. A router that forbids ambient proxies never
        // forwards its credentials through one, so a client that honours them is
        // an error here, not a quiet downgrade.
        Some(client) if !router.ambient_proxy() && client.ambient_proxy() => {
            return Err(error::provider_error(
                400,
                "this router refuses ambient proxies but its HTTP client honours them",
            ));
        }
        Some(client) => client.clone(),
        None if router.ambient_proxy() => SHARED_CLIENT.clone(),
        None => ShimClient::new_scoped()?,
    };
    Ok(client.with_breaker(router.breaker().clone()))
}

/// Generate images through the router's configured provider and aliases.
pub async fn images(router: &Router, request: &Value) -> Result<images::ImageResponse> {
    let request = router.expand_route(request)?;
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .ok_or(error::ShimError::MissingModel)?;
    let (provider, model) = router.resolve(model)?;
    bound_client(router)?
        .images(provider, &model, &request)
        .await
}

/// Generate speech through the router's configured provider and aliases.
pub async fn speech(router: &Router, request: &Value) -> Result<audio::SpeechResponse> {
    let request = router.expand_route(request)?;
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .ok_or(error::ShimError::MissingModel)?;
    let (provider, model) = router.resolve(model)?;
    bound_client(router)?
        .speech(provider, &model, &request)
        .await
}

/// The transcription models of every provider configured on this router,
/// ordered by provider key and then by each provider's own list. Each `id`
/// carries the key the provider is registered under, so it routes back to it.
///
/// ```no_run
/// # async fn example(router: &llmshim::router::Router, bytes: Vec<u8>) -> llmshim::error::Result<()> {
/// for model in llmshim::transcription_models(router) {
///     println!("{}  {}", model.id, model.label);
/// }
/// let request = llmshim::audio::TranscriptionRequest::new(
///     "openrouter/openai/whisper-1", bytes, "recording.wav", "audio/wav",
/// );
/// println!("{}", llmshim::transcription(router, &request).await?.text);
/// # Ok(()) }
/// ```
pub fn transcription_models(router: &Router) -> Vec<audio::TranscriptionModelInfo> {
    let mut keys = router.provider_keys();
    keys.sort_unstable();
    keys.into_iter()
        .filter_map(|key| Some((key, router.get(key).ok()?.transcription_models())))
        .flat_map(|(key, models)| {
            models
                .into_iter()
                .map(move |m| audio::TranscriptionModelInfo::new(key, m.model, m.label))
        })
        .collect()
}

/// Transcribe caller-owned audio through the router's configured provider and aliases.
pub async fn transcription(
    router: &Router,
    request: &audio::TranscriptionRequest,
) -> Result<audio::TranscriptionResponse> {
    let (provider, model) = router.resolve(&request.model)?;
    bound_client(router)?
        .transcription(provider, &model, request)
        .await
}

#[cfg(test)]
mod bound_client_tests {
    use super::*;
    use crate::client::ClientSettings;

    fn client(ambient: bool, retries: u32) -> ShimClient {
        ShimClient::from_settings(ClientSettings {
            ambient_proxy: ambient,
            max_retries: retries,
            ..ClientSettings::default()
        })
        .unwrap()
    }

    #[test]
    fn a_router_without_a_client_uses_the_shared_one_as_before() {
        let bound = bound_client(&Router::new()).unwrap();
        assert_eq!(bound.settings(), SHARED_CLIENT.settings());
        // Scoped, without a client of its own: a scoped client, as before.
        let scoped = bound_client(&Router::new().without_ambient_proxy()).unwrap();
        assert!(!scoped.ambient_proxy());
    }

    #[test]
    fn a_routers_own_client_is_the_one_its_calls_use() {
        let router = Router::new().with_http_client(client(true, 7));
        assert_eq!(bound_client(&router).unwrap().settings().max_retries, 7);
        let scoped = Router::new()
            .without_ambient_proxy()
            .with_http_client(client(false, 5));
        let bound = bound_client(&scoped).unwrap();
        assert_eq!(bound.settings().max_retries, 5);
        assert!(!bound.ambient_proxy());
    }

    #[test]
    fn a_scoped_router_refuses_a_client_that_honours_ambient_proxies_in_either_order() {
        let ambient = client(true, 3);
        let one = Router::new()
            .without_ambient_proxy()
            .with_http_client(ambient.clone());
        let other = Router::new()
            .with_http_client(ambient)
            .without_ambient_proxy();
        for router in [one, other] {
            let error = bound_client(&router)
                .err()
                .expect("refused, not downgraded");
            assert!(error.to_string().contains("ambient"), "{error}");
        }
    }
}

use crate::error::Result;
use serde_json::Value;
use std::{future::Future, pin::Pin};

pub struct ProviderRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl ProviderRequest {
    /// Check endpoint, credentials, settings and prefix for a continuation.
    /// Dispatch still sends a full stateless request.
    pub fn can_continue_from(&self, previous: &Self) -> bool {
        self.url == previous.url
            && self.headers == previous.headers
            && crate::cache::continuation_matches(&previous.body, &self.body)
    }
}

/// Core trait every provider implements.
/// Takes OpenAI-format JSON in, emits provider-native JSON out, and back again.
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    /// Native wire and issuer used by shared reasoning replay. Custom
    /// Chat-Completions adapters inherit a conservative unbound account.
    fn replay_target(&self, model: &str) -> crate::reasoning::ReplayTarget {
        crate::reasoning::ReplayTarget::new(
            self.name(),
            model,
            crate::reasoning::WireFormat::OpenAiChat,
        )
    }

    /// Capture the identity of the request actually sent, including native
    /// model overrides and refreshed OAuth account headers.
    fn request_replay_target(
        &self,
        model: &str,
        request: &ProviderRequest,
    ) -> crate::reasoning::ReplayTarget {
        self.replay_target(request.body["model"].as_str().unwrap_or(model))
    }

    /// Transform an OpenAI-format request into the provider's native format.
    /// `model` is the raw model string (after prefix stripping).
    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest>;

    /// Prepare credentials asynchronously before dispatch. API-key providers
    /// use the synchronous transform; OAuth providers can refresh here.
    fn prepare_request<'a>(
        &'a self,
        model: &'a str,
        request: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderRequest>> + Send + 'a>> {
        Box::pin(async move { self.transform_request(model, request) })
    }

    /// Transform the provider's native response back into OpenAI format.
    fn transform_response(&self, model: &str, response: Value) -> Result<Value>;

    /// Create per-response streaming state. Use this when manually consuming
    /// SSE; tool ids, JSON arguments and trailing signatures require state.
    fn stream_normalizer(&self, model: &str) -> crate::streaming::StreamNormalizer {
        crate::streaming::StreamNormalizer::new(self.replay_target(model))
    }

    /// Low-level stateless content/reasoning parser. Tool events are consumed by
    /// `stream_normalizer`, not emitted as incomplete callable tool records.
    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>>;
}

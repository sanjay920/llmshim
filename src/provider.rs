use crate::error::Result;
use serde_json::Value;
use std::{future::Future, pin::Pin};

pub struct ProviderRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

/// Core trait every provider implements.
/// Takes OpenAI-format JSON in, emits provider-native JSON out, and back again.
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

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

    /// Transform a single SSE chunk from the provider's stream into OpenAI format.
    /// Returns None if the chunk should be skipped (e.g. provider-specific keepalives).
    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>>;
}

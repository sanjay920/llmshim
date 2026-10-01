use crate::error::{Result, ShimError};
use serde_json::Value;
use std::{future::Future, pin::Pin};

pub struct ProviderRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestAdmissionPolicy {
    native_namespace: Option<String>,
    protected_native_fields: &'static [&'static str],
    native_prompt_fields: &'static [&'static str],
    native_output_limit_fields: &'static [&'static str],
}

impl RequestAdmissionPolicy {
    pub fn namespaced(
        native_namespace: impl Into<String>,
        protected_native_fields: &'static [&'static str],
        native_prompt_fields: &'static [&'static str],
        native_output_limit_fields: &'static [&'static str],
    ) -> Self {
        Self {
            native_namespace: Some(native_namespace.into()),
            protected_native_fields,
            native_prompt_fields,
            native_output_limit_fields,
        }
    }

    pub fn native_namespace(&self) -> Option<&str> {
        self.native_namespace.as_deref()
    }

    pub fn protected_native_fields(&self) -> &'static [&'static str] {
        self.protected_native_fields
    }

    pub fn native_prompt_fields(&self) -> &'static [&'static str] {
        self.native_prompt_fields
    }

    pub fn native_output_limit_fields(&self) -> &'static [&'static str] {
        self.native_output_limit_fields
    }
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

    /// Describe native request fields that affect proxy admission. Providers
    /// without a merged native namespace keep the empty default.
    fn request_admission_policy(&self) -> RequestAdmissionPolicy {
        RequestAdmissionPolicy::default()
    }

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

    /// Batch limits for this provider's embeddings wire. The default is
    /// llmshim's own ceiling, and a provider whose API publishes a cap states
    /// it here so the caller is refused locally instead of by the server.
    fn embedding_bounds(&self) -> crate::embeddings::EmbeddingBounds {
        crate::embeddings::EmbeddingBounds::default()
    }

    /// Transform an OpenAI-shaped embedding request (`input`, `dimensions`)
    /// into the provider's native form.
    ///
    /// The default refuses by name. A provider with no embeddings API must say
    /// so here rather than be answered by another vendor's model: a vector from
    /// a model the caller did not name is the wrong answer, silently.
    fn transform_embedding_request(
        &self,
        _model: &str,
        _request: &Value,
    ) -> Result<ProviderRequest> {
        Err(crate::embeddings::absent(self.name()))
    }

    /// Normalize a native embeddings response to one vector per input text, in
    /// input order. Only a provider that implements
    /// [`Provider::transform_embedding_request`] reaches this.
    fn transform_embedding_response(&self, _model: &str, _response: Value) -> Result<Value> {
        Err(ShimError::ProviderError {
            status: 502,
            body: format!(
                "{} returned an embeddings response but has no embeddings wire",
                self.name()
            ),
            retry_after: None,
        })
    }

    /// Prepare a non-streaming image-generation request. Unsupported providers
    /// fail locally rather than sending a prompt to a text endpoint.
    fn image_request(&self, _model: &str, _request: &Value) -> Result<ProviderRequest> {
        Err(crate::error::provider_error(
            400,
            "provider does not support image generation",
        ))
    }

    /// Prepare a replayable upload. Providers without transcription refuse locally.
    fn transcription_request(
        &self,
        _model: &str,
        _request: &crate::audio::TranscriptionRequest,
    ) -> Result<crate::audio::TranscriptionUpload> {
        Err(crate::error::provider_error(
            400,
            "provider does not support transcription",
        ))
    }

    /// Turn the provider answer into transcript text and usage with cost provenance.
    fn transcription_response(
        &self,
        _model: &str,
        _request: &crate::audio::TranscriptionRequest,
        _response: Value,
    ) -> Result<crate::audio::TranscriptionResponse> {
        Err(crate::error::provider_error(
            400,
            "provider does not support transcription",
        ))
    }

    /// Prepare speech synthesis. Providers without a speech endpoint refuse locally.
    fn speech_request(&self, _model: &str, _request: &Value) -> Result<ProviderRequest> {
        Err(crate::error::provider_error(
            400,
            "provider does not support speech synthesis",
        ))
    }

    /// Turn the provider answer into audio bytes, their media type, and usage.
    fn speech_response(
        &self,
        _model: &str,
        _request: &Value,
        _bytes: Vec<u8>,
        _media_type: &str,
    ) -> Result<crate::audio::SpeechResponse> {
        Err(crate::error::provider_error(
            400,
            "provider does not support speech synthesis",
        ))
    }

    fn image_response(
        &self,
        _model: &str,
        _response: Value,
    ) -> Result<crate::images::ImageResponse> {
        Err(crate::error::provider_error(
            400,
            "provider does not support image generation",
        ))
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

//! Cerebras Chat Completions, with its own credentials and replay identity.
use crate::error::Result;
use crate::provider::{Provider, ProviderRequest, RequestAdmissionPolicy};
use crate::providers::openai_compat::OpenAiCompatible;
use crate::reasoning::ReplayTarget;
use serde_json::Value;

pub struct Cerebras {
    inner: OpenAiCompatible,
}

impl Cerebras {
    pub fn new(api_key: String) -> Self {
        Self {
            inner: OpenAiCompatible::new("cerebras", "https://api.cerebras.ai/v1", Some(api_key)),
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.inner.base_url = base_url;
        self
    }
}

impl Provider for Cerebras {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn request_admission_policy(&self) -> RequestAdmissionPolicy {
        self.inner.request_admission_policy()
    }

    fn replay_target(&self, model: &str) -> ReplayTarget {
        self.inner.replay_target(model)
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        self.inner.transform_request(model, request)
    }

    fn transform_response(&self, model: &str, response: Value) -> Result<Value> {
        self.inner.transform_response(model, response)
    }

    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        self.inner.transform_stream_chunk(model, chunk)
    }
}

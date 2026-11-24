//! Google Vertex transports with explicit identity selection and shared native conversion.
mod credentials;
mod endpoint;
mod error;
mod replay;
mod response;
pub(crate) use replay::{canonical_model, restriction as replay_restriction};
mod service_account;
mod stream;

use super::{anthropic::Anthropic, gemini::Gemini};
use crate::{
    error::Result,
    provider::{Provider, ProviderRequest},
    reasoning::{ReplayTarget, WireFormat},
    transport::{ByteStream, EventStream, RequestTransport},
};
pub use credentials::{AccessToken, CredentialError, GoogleIdentity, TokenFuture, TokenSource};
pub use error::{VertexError, VertexErrorKind};
use serde_json::Value;
pub use service_account::{ExchangeFuture, ExchangeResponse, ServiceAccount, TokenExchange};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Copy)]
enum Publisher {
    Google,
    Anthropic,
}
/// Register each publisher separately in a router. Construction performs no credential I/O.
#[derive(Clone)]
#[non_exhaustive]
pub struct Vertex {
    identity: GoogleIdentity,
    publisher: Publisher,
    ambient_proxy: bool,
}
impl Vertex {
    pub fn gemini(identity: GoogleIdentity) -> Self {
        Self {
            identity,
            publisher: Publisher::Google,
            ambient_proxy: false,
        }
    }
    pub fn anthropic(identity: GoogleIdentity) -> Self {
        Self {
            identity,
            publisher: Publisher::Anthropic,
            ambient_proxy: false,
        }
    }
    /// Opt into the caller's ambient proxy for inference only; token exchange remains scoped.
    pub fn with_ambient_proxy(mut self, enabled: bool) -> Self {
        self.ambient_proxy = enabled;
        self
    }
    fn root(&self) -> Result<String> {
        endpoint::root(
            &self.identity,
            match self.publisher {
                Publisher::Google => "google",
                Publisher::Anthropic => "anthropic",
            },
        )
    }
    fn wire(&self) -> WireFormat {
        match self.publisher {
            Publisher::Google => WireFormat::GoogleGenerateContent,
            Publisher::Anthropic => WireFormat::AnthropicMessages,
        }
    }
    fn conversion_model<'a>(&self, model: &'a str) -> &'a str {
        match self.publisher {
            Publisher::Google => model,
            Publisher::Anthropic => model.split('@').next().unwrap_or(model),
        }
    }
}
impl Provider for Vertex {
    fn name(&self) -> &str {
        match self.publisher {
            Publisher::Google => "vertex-gemini",
            Publisher::Anthropic => "vertex-anthropic",
        }
    }
    fn request_admission_policy(&self) -> crate::provider::RequestAdmissionPolicy {
        match self.publisher {
            Publisher::Google => Gemini::new(String::new()).request_admission_policy(),
            Publisher::Anthropic => Anthropic::new(String::new()).request_admission_policy(),
        }
    }
    fn transport(&self) -> Option<Arc<dyn RequestTransport>> {
        Some(Arc::new(self.clone()))
    }
    fn replay_target(&self, model: &str) -> ReplayTarget {
        let mut target = ReplayTarget::new(self.name(), model, self.wire());
        let direct = match self.publisher {
            Publisher::Google => "google",
            Publisher::Anthropic => "anthropic",
        };
        target.family =
            crate::catalog::lookup_id(&format!("{direct}/{}", self.conversion_model(model)))
                .and_then(|m| m.family);
        target.bind_account(
            &self.root().unwrap_or_default(),
            Some(&self.identity.binding),
        )
    }
    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        let root = self.root()?;
        let segment = endpoint::model_segment(model)?;
        let target = self.replay_target(model);
        let mut prepared = match self.publisher {
            Publisher::Google => {
                Gemini::new(String::new()).request_for_target(model, request, &target)?
            }
            Publisher::Anthropic => Anthropic::new(String::new()).request_for_target(
                self.conversion_model(model),
                request,
                &target,
            )?,
        };
        let stream = request["stream"] == true;
        let operation = match (self.publisher, stream) {
            (Publisher::Google, false) => "generateContent",
            (Publisher::Google, true) => "streamGenerateContent?alt=sse",
            (Publisher::Anthropic, false) => "rawPredict",
            (Publisher::Anthropic, true) => "streamRawPredict",
        };
        if let Publisher::Anthropic = self.publisher {
            let body = prepared
                .body
                .as_object_mut()
                .ok_or_else(|| VertexError::service(400, "InvalidRequest").into_shim(None))?;
            body.remove("model");
            body.insert("anthropic_version".into(), Value::from("vertex-2023-10-16"));
        }
        prepared.url = format!("{root}{segment}:{operation}");
        prepared.headers.retain(|(name, _)| {
            !name.eq_ignore_ascii_case("x-api-key")
                && !name.eq_ignore_ascii_case("anthropic-version")
        });
        Ok(prepared)
    }
    fn stream_normalizer(&self, model: &str) -> crate::streaming::StreamNormalizer {
        crate::streaming::StreamNormalizer::new(self.replay_target(model))
            .with_conversion_model(self.conversion_model(model).to_owned())
            .with_output_projection(response::project_usage)
    }
    fn transform_response(&self, model: &str, native: Value) -> Result<Value> {
        let native = response::prepare(native);
        if let Some(error) = error::native_error(&native, None) {
            return Err(error.into_shim(None));
        }
        let mut result = match self.publisher {
            Publisher::Google => {
                Gemini::new(String::new()).transform_response_native(model, native.clone())?
            }
            Publisher::Anthropic => Anthropic::new(String::new())
                .transform_response_native(self.conversion_model(model), native.clone())?,
        };
        result["model"] = Value::from(model);
        crate::derived_response::capture_unary(&self.replay_target(model), &native, &mut result)?;
        response::project_usage(&native, &mut result);
        Ok(result)
    }
    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        crate::json_bounds::enforce_sse_complexity(chunk)?;
        let native = response::prepare(serde_json::from_str(chunk)?);
        let chunk = &native.to_string();
        if let Some(error) = error::native_error(&native, None) {
            return Err(error.into_shim(None));
        }
        let result = match self.publisher {
            Publisher::Google => {
                Gemini::new(String::new()).transform_stream_chunk_native(model, chunk)?
            }
            Publisher::Anthropic => Anthropic::new(String::new())
                .transform_stream_chunk_native(self.conversion_model(model), chunk)?,
        };
        let result = result
            .map(|chunk| -> Result<String> {
                let mut value: Value = serde_json::from_str(&chunk)?;
                value["model"] = Value::from(model);
                Ok(value.to_string())
            })
            .transpose()?;
        crate::reasoning::capture_stream(&self.replay_target(model), &native, result)
    }
}
impl RequestTransport for Vertex {
    fn authorize<'a>(
        &'a self,
        request: &'a mut reqwest::Request,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let root = self.root()?;
            if request.method() != reqwest::Method::POST
                || !request.url().as_str().starts_with(&root)
            {
                return Err(CredentialError::InvalidBinding.into_shim());
            }
            self.identity.authorize(request).await
        })
    }
    fn stream_normalizer(
        &self,
        target: ReplayTarget,
        limits: crate::streaming::StreamRetentionLimits,
    ) -> Result<crate::streaming::StreamNormalizer> {
        let model = self.conversion_model(&target.model).to_owned();
        Ok(
            crate::streaming::StreamNormalizer::with_retention_limits(target, limits)?
                .with_conversion_model(model)
                .with_output_projection(response::project_usage),
        )
    }
    fn authorization_rejected<'a>(
        &'a self,
        status: u16,
        body: &'a str,
        sent: Option<&'a reqwest::header::HeaderValue>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool>> + Send + 'a>> {
        Box::pin(async move {
            if !error::token_rejected(status, body) {
                return Ok(false);
            }
            let Some(sent) = sent else {
                return Ok(false);
            };
            self.identity.reject(sent).await;
            Ok(true)
        })
    }
    fn authorization_timeout(&self) -> crate::error::ShimError {
        CredentialError::Timeout.into_shim()
    }
    fn events(&self, _model: &str, bytes: ByteStream) -> EventStream {
        stream::events(bytes)
    }
    fn error(
        &self,
        status: u16,
        _headers: &reqwest::header::HeaderMap,
        body: String,
        retry_after: Option<Duration>,
    ) -> crate::error::ShimError {
        let native: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        error::native_error(&native, Some(status))
            .unwrap_or_else(|| VertexError::service(status, "Unknown"))
            .into_shim(retry_after)
    }
    fn requires_scoped_client(&self) -> bool {
        !self.ambient_proxy
    }
}

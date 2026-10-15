//! Azure OpenAI and Foundry v1: deployment mapping with explicit key or Entra authorization.
mod credentials;
mod error;
mod stream;
use crate::{
    error::Result,
    provider::{Provider, ProviderRequest, RequestAdmissionPolicy},
    providers::openai_compat::OpenAiCompatible,
    reasoning::{ReplayTarget, WireFormat},
    transport::{ByteStream, EventStream, RequestTransport},
};
pub use credentials::{
    AccessToken, Audience, AzureIdentity, CredentialError, TokenFuture, TokenSource,
};
pub use error::{AzureError, AzureErrorKind};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Clone)]
#[non_exhaustive]
pub struct Azure {
    root: String,
    identity: AzureIdentity,
    deployments: Arc<BTreeMap<String, String>>,
    ambient_proxy: bool,
}
impl Azure {
    /// Accept an HTTPS resource origin or its `/openai/v1` base. No credentials are resolved.
    pub fn new(endpoint: &str, identity: AzureIdentity) -> Result<Self> {
        let url = reqwest::Url::parse(endpoint)
            .map_err(|_| AzureError::service(400, "InvalidEndpoint").into_shim(None))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path().trim_end_matches('/'), "" | "/openai/v1")
        {
            return Err(AzureError::service(400, "InvalidEndpoint").into_shim(None));
        }
        let root = format!(
            "{}/openai/v1",
            url.as_str()
                .trim_end_matches('/')
                .trim_end_matches("/openai/v1")
        );
        Ok(Self {
            root,
            identity,
            deployments: Arc::new(BTreeMap::new()),
            ambient_proxy: false,
        })
    }
    /// Deployment is the wire model. The mapped model supplies catalog family metadata
    /// and Azure pricing information.
    pub fn with_deployment(mut self, deployment: &str, model: &str) -> Result<Self> {
        if deployment.is_empty()
            || deployment.len() > 256
            || model.trim().is_empty()
            || !deployment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(AzureError::service(400, "InvalidDeployment").into_shim(None));
        }
        Arc::make_mut(&mut self.deployments).insert(deployment.into(), model.into());
        Ok(self)
    }
    pub fn with_ambient_proxy(mut self, enabled: bool) -> Self {
        self.ambient_proxy = enabled;
        self
    }
    fn adapter(&self) -> OpenAiCompatible {
        OpenAiCompatible::new("azure", &self.root, None)
    }
    fn validate(&self, deployment: &str) -> Result<()> {
        if !self.deployments.contains_key(deployment) {
            return Err(AzureError::service(400, "DeploymentNotConfigured").into_shim(None));
        }
        Ok(())
    }
}
impl Provider for Azure {
    fn name(&self) -> &str {
        "azure"
    }
    fn transport(&self) -> Option<Arc<dyn RequestTransport>> {
        Some(Arc::new(self.clone()))
    }
    fn request_admission_policy(&self) -> RequestAdmissionPolicy {
        self.adapter().request_admission_policy()
    }
    fn replay_target(&self, model: &str) -> ReplayTarget {
        let mut target = ReplayTarget::new(self.name(), model, WireFormat::OpenAiChat);
        target.pricing_model = self.deployments.get(model).cloned();
        target.family = self
            .deployments
            .get(model)
            .and_then(|model| crate::catalog::lookup_id(model))
            .and_then(|model| model.family);
        target.bind_account(&self.root, Some(&self.identity.binding()))
    }
    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        self.validate(model)?;
        for control in ["reasoning_mode", "reasoning_summary", "top_k"] {
            if request.get(control).is_some_and(|value| !value.is_null()) {
                return Err(crate::error::provider_error(
                    400,
                    &format!("unsupported Azure control: {control}"),
                ));
            }
        }
        for field in ["model", "messages", "stream", "stream_options"] {
            if request["x-azure"].get(field).is_some() {
                return Err(
                    AzureError::service(400, format!("UnsupportedOverride:{field}"))
                        .into_shim(None),
                );
            }
        }
        let mut sent =
            self.adapter()
                .request_for_target(model, request, &self.replay_target(model))?;
        if sent.body["stream"] == true {
            let options = sent
                .body
                .as_object_mut()
                .ok_or_else(|| AzureError::service(400, "InvalidRequest").into_shim(None))?
                .entry("stream_options")
                .or_insert_with(|| serde_json::json!({}));
            let options = options
                .as_object_mut()
                .ok_or_else(|| AzureError::service(400, "InvalidRequest").into_shim(None))?;
            options.insert("include_usage".into(), Value::Bool(true));
        }
        Ok(sent)
    }
    fn transform_response(&self, model: &str, native: Value) -> Result<Value> {
        self.validate(model)?;
        if let Some(error) = error::native_error(&native, None) {
            return Err(error.into_shim(None));
        }
        if !native["choices"].is_array() {
            return Err(AzureError::service(502, "InvalidResponse").into_shim(None));
        }
        let mut result = self
            .adapter()
            .transform_response_native(model, native.clone())?;
        result["model"] = Value::from(model);
        crate::derived_response::capture_unary(&self.replay_target(model), &native, &mut result)?;
        Ok(result)
    }
    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        if chunk == "[DONE]" {
            return Ok(None);
        }
        let native = crate::json_bounds::parse_str(chunk, crate::json_bounds::Limits::SSE)
            .map_err(|_| AzureError::service(502, "InvalidEvent").into_shim(None))?;
        if let Some(error) = error::native_error(&native, None) {
            return Err(error.into_shim(None));
        }
        let result = self.adapter().transform_stream_chunk_native(model, chunk)?;
        let result = result
            .map(|chunk| -> Result<_> {
                let mut parsed: Value = serde_json::from_str(&chunk)?;
                parsed
                    .as_object_mut()
                    .ok_or_else(|| AzureError::service(502, "InvalidEvent").into_shim(None))?
                    .insert("model".into(), Value::from(model));
                Ok(parsed.to_string())
            })
            .transpose()?;
        crate::reasoning::capture_stream(&self.replay_target(model), &native, result)
    }
}
impl RequestTransport for Azure {
    fn authorize<'a>(
        &'a self,
        request: &'a mut reqwest::Request,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if request.method() != reqwest::Method::POST
                || request.url().as_str() != format!("{}/chat/completions", self.root)
            {
                return Err(AzureError::service(400, "InvalidEndpoint").into_shim(None));
            }
            self.identity.authorize(request).await
        })
    }
    fn authorization_timeout(&self) -> crate::error::ShimError {
        CredentialError::Timeout.into_shim()
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
            match sent {
                Some(sent) => Ok(self.identity.reject(sent).await),
                None => Ok(false),
            }
        })
    }
    fn requires_scoped_client(&self) -> bool {
        !self.ambient_proxy
    }
    fn events(&self, model: &str, bytes: ByteStream) -> EventStream {
        stream::events(model, bytes)
    }
    fn error(
        &self,
        status: u16,
        _: &reqwest::header::HeaderMap,
        body: String,
        retry_after: Option<Duration>,
    ) -> crate::error::ShimError {
        let native = crate::json_bounds::parse_str(&body, crate::json_bounds::Limits::SSE)
            .unwrap_or(Value::Null);
        error::native_error(&native, Some(status))
            .unwrap_or_else(|| AzureError::service(status, "Unknown"))
            .into_shim(retry_after)
    }
}

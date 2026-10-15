//! Amazon Bedrock Converse, with call-time profile credentials and binary EventStream.
mod content;
mod credential_error;
mod credentials;
mod endpoint;
mod error;
mod events;
mod reasoning;
mod request;
mod response;
mod signing;
mod stream;

use crate::{
    error::Result,
    provider::{Provider, ProviderRequest},
    reasoning::{ReplayTarget, WireFormat},
    transport::{ByteStream, EventStream, RequestTransport},
};
use aws_credential_types::provider::ProvideCredentials;
pub use credentials::AwsProfile;
pub use error::{BedrockError, BedrockErrorKind, CredentialReason};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};
pub use stream::EventStreamDecoder;

#[derive(Clone)]
#[non_exhaustive]
pub struct Bedrock {
    authorization: credentials::Authorization,
    endpoint: std::result::Result<String, &'static str>,
    ambient_proxy: bool,
}

impl Bedrock {
    pub fn new(profile: AwsProfile) -> Self {
        let endpoint = endpoint::url(&profile.region);
        Self {
            endpoint,
            ambient_proxy: false,
            authorization: credentials::Authorization {
                profile,
                files: None,
                credentials: None,
                identity_binding: None,
                cached: Default::default(),
            },
        }
    }

    /// Select profile files explicitly, for isolated applications as well as SDK installations.
    pub fn with_profile_files(mut self, config: PathBuf, credentials: PathBuf) -> Self {
        self.authorization.files = Some((config, credentials));
        self.authorization.cached = Default::default();
        self
    }

    /// Use a caller-owned SDK provider and a stable, non-secret identity reference.
    /// Reuse the binding across restarts/replicas; change it when the AWS identity changes.
    pub fn with_credential_provider(
        mut self,
        provider: impl ProvideCredentials + 'static,
        binding: &str,
    ) -> Self {
        self.authorization.credentials = Some(Arc::new(provider));
        self.authorization.cached = Default::default();
        self.authorization.identity_binding = Some(binding.to_owned());
        self
    }

    /// Sign a fully serialized request. Each invocation checks the shared identity cache;
    /// the named provider is consulted lazily and refreshed before credentials expire.
    pub async fn authorize(
        &self,
        request: &mut reqwest::Request,
    ) -> std::result::Result<(), BedrockError> {
        self.authorization.authorize(request).await
    }

    /// Allow Converse traffic to use the client's ambient proxy configuration.
    pub fn with_ambient_proxy(mut self, enabled: bool) -> Self {
        self.ambient_proxy = enabled;
        self
    }

    fn endpoint(&self) -> Result<String> {
        self.endpoint.clone().map_err(|reason| {
            crate::error::provider_error(
                400,
                &format!("Unsupported Bedrock region or partition: {reason}"),
            )
        })
    }
}

impl Provider for Bedrock {
    fn name(&self) -> &str {
        "bedrock"
    }

    fn transport(&self) -> Option<Arc<dyn RequestTransport>> {
        Some(Arc::new(self.clone()))
    }

    fn replay_target(&self, model: &str) -> ReplayTarget {
        // The decoder projects to Messages semantics; the account remains transport-specific.
        let mut target = ReplayTarget::new("bedrock", model, WireFormat::AnthropicMessages);
        target.family = crate::catalog::lookup_id(&format!("amazon-bedrock/{model}"))
            .and_then(|metadata| metadata.family)
            .or(target.family);
        let binding = match &self.authorization.identity_binding {
            Some(binding) => format!("caller:{binding}"),
            None => format!(
                "profile:{}:{:?}",
                self.authorization.profile.name, self.authorization.files
            ),
        };
        target.bind_account(&self.endpoint().unwrap_or_default(), Some(&binding))
    }

    fn stream_normalizer(&self, model: &str) -> crate::streaming::StreamNormalizer {
        crate::streaming::StreamNormalizer::new(self.replay_target(model))
            .with_output_projection(response::project_usage)
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        request::prepare(self, model, request)
    }

    fn response_for_usage<'a>(&self, native: &'a Value) -> std::borrow::Cow<'a, Value> {
        std::borrow::Cow::Owned(serde_json::json!({"usage":response::usage(&native["usage"])}))
    }

    fn transform_response(&self, model: &str, native: Value) -> Result<Value> {
        response::normalize(&self.replay_target(model), native)
    }

    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        crate::providers::anthropic::Anthropic::new(String::new())
            .transform_stream_chunk(model, chunk)
    }
}

impl RequestTransport for Bedrock {
    fn prepare_authorization<'a>(
        &'a self,
        request: &'a mut reqwest::Request,
    ) -> crate::transport::AuthorizationFuture<'a> {
        Box::pin(async move {
            let identity = self
                .authorization
                .resolve()
                .await
                .map_err(|e| e.into_shim(None))?;
            // The request stays unchanged during policy acquisition. Validate every
            // fallible signing/header input now, then re-sign with a fresh clock afterward.
            identity
                .validate(request)
                .map_err(|error| error.into_shim(None))?;
            Ok(Some(
                Box::new(identity) as Box<dyn crate::transport::PreparedAuthorization>
            ))
        })
    }

    fn authorization_timeout(&self) -> crate::error::ShimError {
        self.authorization
            .failure(CredentialReason::Timeout, 503)
            .into_shim(None)
    }

    fn response_id(&self, headers: &reqwest::header::HeaderMap) -> Option<String> {
        headers
            .get("x-amzn-requestid")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    fn requires_scoped_client(&self) -> bool {
        !self.ambient_proxy
    }

    fn authorize<'a>(
        &'a self,
        request: &'a mut reqwest::Request,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.authorize(request)
                .await
                .map_err(|error| error.into_shim(None))
        })
    }

    fn stream_normalizer(
        &self,
        target: ReplayTarget,
        limits: crate::streaming::StreamRetentionLimits,
    ) -> Result<crate::streaming::StreamNormalizer> {
        Ok(
            crate::streaming::StreamNormalizer::with_retention_limits(target, limits)?
                .with_output_projection(response::project_usage),
        )
    }

    fn events(&self, model: &str, bytes: ByteStream) -> EventStream {
        stream::events(model, None, bytes)
    }

    fn events_with_response_id(
        &self,
        model: &str,
        id: Option<&str>,
        bytes: ByteStream,
    ) -> EventStream {
        stream::events(model, id, bytes)
    }

    fn error(
        &self,
        status: u16,
        headers: &reqwest::header::HeaderMap,
        body: String,
        retry_after: Option<Duration>,
    ) -> crate::error::ShimError {
        let parsed = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
        let code = headers
            .get("x-amzn-errortype")
            .and_then(|value| value.to_str().ok())
            .or_else(|| parsed["__type"].as_str())
            .or_else(|| parsed["code"].as_str())
            .unwrap_or("");
        let error = BedrockError::service(status, code);
        error.with_native_error(parsed, retry_after)
    }
}

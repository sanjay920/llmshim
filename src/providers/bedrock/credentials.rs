//! A named profile is a closed credential source, not the default provider chain.
use super::error::CredentialReason;
use super::{signing, BedrockError};
use aws_config::{profile::ProfileFileCredentialsProvider, provider_config::ProviderConfig};
use aws_credential_types::{
    provider::{ProvideCredentials, SharedCredentialsProvider},
    Credentials,
};
use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
use aws_smithy_runtime_api::client::auth::static_resolver::StaticAuthSchemeOptionResolver;
use aws_smithy_runtime_api::client::{
    identity::{ResolveCachedIdentity, SharedIdentityCache, SharedIdentityResolver},
    runtime_components::RuntimeComponents,
};
use aws_smithy_types::config_bag::ConfigBag;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};

/// An identity reference. Credentials are loaded only when a request is dispatched.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AwsProfile {
    pub name: String,
    pub region: String,
}

impl AwsProfile {
    pub fn new(name: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            region: region.into(),
        }
    }
}

#[derive(Clone)]
pub(super) struct Authorization {
    pub profile: AwsProfile,
    pub files: Option<(PathBuf, PathBuf)>,
    pub credentials: Option<Arc<dyn ProvideCredentials>>,
    pub identity_binding: Option<String>,
    pub cached: Arc<tokio::sync::OnceCell<CachedIdentity>>,
}

pub(super) struct CachedIdentity {
    cache: SharedIdentityCache,
    resolver: SharedIdentityResolver,
    runtime: RuntimeComponents,
}

impl Authorization {
    pub(super) fn failure(&self, reason: CredentialReason, status: u16) -> BedrockError {
        self.describe(BedrockError::credentials(
            &self.profile.name,
            reason,
            status,
        ))
    }
    fn describe(&self, mut error: BedrockError) -> BedrockError {
        if let Some(binding) = &self.identity_binding {
            error.profile = None;
            error.binding = Some(binding.clone());
        }
        error
    }

    pub async fn authorize(&self, request: &mut reqwest::Request) -> Result<(), BedrockError> {
        let identity = self.resolve().await?;
        identity.sign_request(request)
    }

    pub(super) async fn resolve(&self) -> Result<SigningIdentity, BedrockError> {
        let failure = |reason, status| self.failure(reason, status);
        let endpoint = super::endpoint::url(&self.profile.region)
            .map_err(|_| failure(CredentialReason::InvalidConfiguration, 401))?;
        if self.profile.name.trim().is_empty() || self.profile.region.trim().is_empty() {
            return Err(failure(CredentialReason::InvalidConfiguration, 401));
        }
        let cached = self
            .cached
            .get_or_try_init(|| async {
                let provider = match &self.credentials {
                    Some(provider) => SharedCredentialsProvider::new(provider.clone()),
                    None => {
                        let http = aws_smithy_http_client::Builder::new()
                            .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
                                aws_smithy_http_client::tls::rustls_provider::CryptoMode::Ring,
                            ))
                            .build_https();
                        let config = ProviderConfig::without_region()
                            .with_http_client(http)
                            .with_region(Some(aws_types::region::Region::new(
                                self.profile.region.clone(),
                            )));
                        let mut builder = ProfileFileCredentialsProvider::builder()
                            .configure(&config)
                            .profile_name(&self.profile.name);
                        if let Some((config, credentials)) = &self.files {
                            let files = EnvConfigFiles::builder()
                                .include_default_config_file(false)
                                .include_default_credentials_file(false)
                                .with_file(EnvConfigFileKind::Config, config)
                                .with_file(EnvConfigFileKind::Credentials, credentials)
                                .build();
                            builder = builder.profile_files(files);
                        }
                        // This is only the named provider, never the default credential chain.
                        SharedCredentialsProvider::new(builder.build())
                    }
                };
                // Our outer 30s deadline retains a typed reason; the SDK cache otherwise
                // erases its internal timeout into an opaque boxed error at the same instant.
                let cache = aws_config::identity::IdentityCache::lazy()
                    .load_timeout(Duration::from_secs(31))
                    .build();
                let resolver = SharedIdentityResolver::new(provider);
                let runtime = RuntimeComponents::builder("bedrock-credentials")
                .with_auth_scheme_option_resolver(Some(
                    StaticAuthSchemeOptionResolver::new(vec![aws_runtime::auth::sigv4::SCHEME_ID])))
                .with_auth_scheme(aws_runtime::auth::sigv4::SigV4AuthScheme::new())
                .with_endpoint_resolver(Some(aws_smithy_runtime::client::orchestrator::endpoints
                    ::StaticUriEndpointResolver::uri(endpoint)))
                .with_identity_cache(Some(cache.clone()))
                .with_identity_resolver(aws_runtime::auth::sigv4::SCHEME_ID, resolver.clone())
                .with_retry_strategy(Some(aws_smithy_runtime::client::retries::strategy
                    ::StandardRetryStrategy::new()))
                .with_time_source(Some(aws_smithy_async::time::SystemTimeSource::new()))
                .with_sleep_impl(Some(aws_smithy_async::rt::sleep::TokioSleep::new()))
                .build().map_err(|_| failure(CredentialReason::InvalidConfiguration, 401))?;
                Ok::<_, BedrockError>(CachedIdentity {
                    cache,
                    resolver,
                    runtime,
                })
            })
            .await?;
        let identity = tokio::time::timeout(
            Duration::from_secs(30),
            cached.cache.resolve_cached_identity(
                cached.resolver.clone(),
                &cached.runtime,
                &ConfigBag::base(),
            ),
        )
        .await
        .map_err(|_| failure(CredentialReason::Timeout, 503))?
        .map_err(|error| {
            self.describe(super::credential_error::classify(
                &self.profile.name,
                error.as_ref(),
            ))
        })?;
        let credentials = identity
            .data::<Credentials>()
            .ok_or_else(|| failure(CredentialReason::ProviderFailed, 503))?
            .clone();
        if credentials.access_key_id().is_empty() || credentials.secret_access_key().is_empty() {
            return Err(failure(CredentialReason::InvalidConfiguration, 401));
        }
        if credentials
            .expiry()
            .is_some_and(|expiry| expiry <= SystemTime::now())
        {
            return Err(failure(CredentialReason::ExpiredCredentials, 401));
        }
        for value in [
            Some(credentials.access_key_id()),
            credentials.session_token(),
        ]
        .into_iter()
        .flatten()
        {
            if !reqwest::header::HeaderValue::from_str(value)
                .is_ok_and(|header| header.to_str().is_ok())
            {
                return Err(failure(CredentialReason::InvalidConfiguration, 401));
            }
        }
        Ok(SigningIdentity {
            credentials,
            region: self.profile.region.clone(),
        })
    }
}

pub(super) struct SigningIdentity {
    credentials: Credentials,
    region: String,
}
impl SigningIdentity {
    pub(super) fn validate(&self, request: &reqwest::Request) -> Result<(), BedrockError> {
        let mut request = request.try_clone().ok_or_else(BedrockError::protocol)?;
        signing::sign_request(
            &mut request,
            self.credentials.clone(),
            &self.region,
            "bedrock",
            SystemTime::now(),
            aws_sigv4::http_request::SigningSettings::default(),
        )
    }

    fn sign_request(self, request: &mut reqwest::Request) -> Result<(), BedrockError> {
        signing::sign_request(
            request,
            self.credentials,
            &self.region,
            "bedrock",
            SystemTime::now(),
            aws_sigv4::http_request::SigningSettings::default(),
        )
    }
}

impl crate::transport::PreparedAuthorization for SigningIdentity {
    fn sign(self: Box<Self>, request: &mut reqwest::Request) -> crate::error::Result<()> {
        self.sign_request(request)
            .map_err(|error| error.into_shim(None))
    }
}

//! The AWS credential chain for the `s3` backend (note 0050, slice 3a): environment variables, the
//! web identity token file of EKS IAM roles for service accounts (IRSA), EKS Pod Identity and ECS task
//! roles (the container credentials endpoint), the EC2 instance profile (IMDS), SSO and profile files.
//! The SDK's provider does the work; this keeps the result for as long as it is valid and gives the
//! store `(access key id, secret, session token)` to sign with. Built only with `serve-cloud`.
//!
//! Unlike the provider transports (Bedrock, Vertex, Azure), which refuse an ambient identity on
//! purpose because a gateway serves many callers, the object store is the operator's own
//! infrastructure: the pod's own role is exactly what it should use. It is still explicit: a file that
//! says `credentials = "aws"` uses the chain, and one that does not never consults it.

use super::BlobError;
use aws_config::default_provider::credentials::DefaultCredentialsChain;
use aws_config::provider_config::ProviderConfig;
use aws_credential_types::provider::{error::CredentialsError, ProvideCredentials};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{Mutex, OnceCell};

/// How long credentials with no expiry (environment keys, a profile's static keys) are kept before the
/// chain is asked again, so a rotated profile file is seen.
const NO_EXPIRY_KEEP: Duration = Duration::from_secs(60);
/// How long before expiry a temporary credential is replaced.
const RENEW_BEFORE: Duration = Duration::from_secs(300);
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a failure to get credentials is remembered: a brown-out of STS or the metadata service is
/// one shared wait, not one 30-second wait per queued request.
const FAILURE_KEEP: Duration = Duration::from_secs(5);
/// The soonest a refusal may force the credentials to be fetched again.
const FORCE_EVERY: Duration = Duration::from_secs(5);

struct Cached {
    id: String,
    secret: String,
    token: Option<String>,
    renew_at: SystemTime,
    fetched: Instant,
}

#[derive(Default)]
struct State {
    held: Option<Cached>,
    failed: Option<(Instant, BlobError)>,
}

pub struct AwsChain {
    config: ProviderConfig,
    provider: OnceCell<DefaultCredentialsChain>,
    cached: Mutex<State>,
}

impl AwsChain {
    /// The chain for `region` (which web identity and container roles need to reach STS). No
    /// credential I/O happens here: the chain is built and asked at the first request.
    pub fn new(region: &str) -> Self {
        let http = aws_smithy_http_client::Builder::new()
            .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
                aws_smithy_http_client::tls::rustls_provider::CryptoMode::Ring,
            ))
            .build_https();
        Self::with_config(
            ProviderConfig::default()
                .with_http_client(http)
                .with_region(Some(aws_types::region::Region::new(region.to_owned()))),
        )
    }

    /// With a prepared provider configuration (an environment, a file system and an HTTP client of the
    /// caller's): how the tests give the chain a hermetic world.
    pub fn with_config(config: ProviderConfig) -> Self {
        Self {
            config,
            provider: OnceCell::new(),
            cached: Mutex::new(State::default()),
        }
    }

    /// `(access key id, secret, session token)`. `Denied` when the chain finds no credentials at all,
    /// `Unavailable` when a source failed or timed out; neither carries the SDK's message.
    pub async fn get(&self) -> Result<(String, String, Option<String>), BlobError> {
        let mut state = self.cached.lock().await;
        if let Some(held) = state.held.as_ref() {
            if SystemTime::now() < held.renew_at {
                return Ok((held.id.clone(), held.secret.clone(), held.token.clone()));
            }
        }
        if let Some((at, error)) = state.failed {
            if at.elapsed() < FAILURE_KEEP {
                return Err(error);
            }
        }
        let provider = self
            .provider
            .get_or_init(|| async {
                DefaultCredentialsChain::builder()
                    .configure(self.config.clone())
                    .build()
                    .await
            })
            .await;
        let loaded = tokio::time::timeout(LOAD_TIMEOUT, provider.provide_credentials())
            .await
            .map_err(|_| BlobError::Unavailable)
            .and_then(|result| {
                result.map_err(|error| match error {
                    CredentialsError::CredentialsNotLoaded(_) => BlobError::Denied,
                    _ => BlobError::Unavailable,
                })
            });
        let credentials = match loaded {
            Ok(credentials)
                if !credentials.access_key_id().is_empty()
                    && !credentials.secret_access_key().is_empty() =>
            {
                credentials
            }
            Ok(_) => {
                state.failed = Some((Instant::now(), BlobError::Denied));
                return Err(BlobError::Denied);
            }
            Err(error) => {
                state.failed = Some((Instant::now(), error));
                return Err(error);
            }
        };
        let now = SystemTime::now();
        let renew_at = match credentials.expiry() {
            Some(expiry) => expiry
                .checked_sub(RENEW_BEFORE)
                // A credential that lives less than the margin is used until it is nearly gone.
                .filter(|at| *at > now)
                .unwrap_or_else(|| now + Duration::from_secs(5).min(NO_EXPIRY_KEEP)),
            None => now + NO_EXPIRY_KEEP,
        };
        let held = Cached {
            id: credentials.access_key_id().to_owned(),
            secret: credentials.secret_access_key().to_owned(),
            token: credentials.session_token().map(str::to_owned),
            renew_at,
            fetched: Instant::now(),
        };
        let triple = (held.id.clone(), held.secret.clone(), held.token.clone());
        state.failed = None;
        state.held = Some(held);
        Ok(triple)
    }

    /// Forget what is held, because the store refused a request signed with the credentials whose key
    /// id is `refused`: only if those are still the ones held (another request may have refreshed
    /// them already) and were fetched at least five seconds ago (a role that merely lacks a permission
    /// must not turn every denied request into a call to STS).
    pub async fn invalidate(&self, refused: &str) {
        let mut state = self.cached.lock().await;
        if state
            .held
            .as_ref()
            .is_some_and(|held| held.id == refused && held.fetched.elapsed() >= FORCE_EVERY)
        {
            state.held = None;
        }
    }
}

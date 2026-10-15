use super::*;
use aws_credential_types::{
    provider::{error::CredentialsError, future, ProvideCredentials},
    Credentials,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Debug)]
struct Counting(Arc<AtomicUsize>);
impl ProvideCredentials for Counting {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Credentials::new("FIXTURE", "secret", None, None, "fixture"))
        })
    }
}
fn request() -> reqwest::Request {
    let mut request = reqwest::Request::new(
        reqwest::Method::POST,
        "https://bedrock-runtime.us-east-1.amazonaws.com/model/model/converse"
            .parse()
            .unwrap(),
    );
    *request.body_mut() = Some("{}".into());
    request
}
#[tokio::test]
async fn credential_cache_and_stable_binding_survive_clones_and_reconstruction() {
    let count = Arc::new(AtomicUsize::new(0));
    let first = provider().with_credential_provider(Counting(count.clone()), "tenant-fixture");
    let second = provider().with_credential_provider(Counting(count.clone()), "tenant-fixture");
    assert_eq!(
        first.replay_target("model").account,
        second.replay_target("model").account
    );
    first.authorize(&mut request()).await.unwrap();
    first.clone().authorize(&mut request()).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    use llmshim::transport::RequestTransport;
    assert!(first.requires_scoped_client());
    assert!(!first.with_ambient_proxy(true).requires_scoped_client());
}

#[derive(Debug)]
struct Failing(bool);
impl ProvideCredentials for Failing {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(async move {
            Err(if self.0 {
                CredentialsError::provider_timed_out(std::time::Duration::from_secs(30))
            } else {
                CredentialsError::provider_error("private detail must not escape")
            })
        })
    }
}
#[tokio::test]
async fn transient_credentials_and_timeouts_are_retryable_and_explain_profile() {
    for timeout in [false, true] {
        let error = provider()
            .with_credential_provider(Failing(timeout), "fixture")
            .authorize(&mut request())
            .await
            .unwrap_err();
        assert_eq!(error.status, 503);
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["binding"], "fixture");
        assert!(value.get("profile").is_none());
        assert_eq!(
            value["credential_reason"],
            if timeout {
                "timeout"
            } else {
                "provider_failed"
            }
        );
        assert!(!error.to_string().contains("private detail"));
    }
}

#[derive(Debug)]
struct Pending;
impl ProvideCredentials for Pending {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(std::future::pending())
    }
}
#[tokio::test(start_paused = true)]
async fn stalled_provider_has_typed_timeout_without_real_time_or_network() {
    let error = provider()
        .with_credential_provider(Pending, "fixture")
        .authorize(&mut request())
        .await
        .unwrap_err();
    assert_eq!(error.status, 503);
    assert_eq!(
        serde_json::to_value(error).unwrap()["credential_reason"],
        "timeout"
    );
}

#[derive(Debug)]
struct ExpiredThenFresh(Arc<AtomicUsize>);
impl ProvideCredentials for ExpiredThenFresh {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(async move {
            let expiry =
                (self.0.fetch_add(1, Ordering::SeqCst) == 0).then_some(std::time::UNIX_EPOCH);
            Ok(Credentials::new(
                "FIXTURE", "secret", None, expiry, "fixture",
            ))
        })
    }
}
#[tokio::test]
async fn expired_identity_is_rejected_then_refreshed() {
    let count = Arc::new(AtomicUsize::new(0));
    let provider = provider().with_credential_provider(ExpiredThenFresh(count.clone()), "fixture");
    let error = provider.authorize(&mut request()).await.unwrap_err();
    assert_eq!(
        serde_json::to_value(error).unwrap()["credential_reason"],
        "expired_credentials"
    );
    provider.authorize(&mut request()).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[test]
fn caller_binding_is_authoritative_across_unused_profile_settings() {
    let first = provider().with_credential_provider(Counting(Arc::default()), "identity-a");
    let other = Bedrock::new(AwsProfile::new("another-profile", "us-east-1"))
        .with_profile_files("config-fixture".into(), "credentials-fixture".into())
        .with_credential_provider(Counting(Arc::default()), "identity-a");
    assert_eq!(
        first.replay_target("model").account,
        other.replay_target("model").account
    );
    let different = provider().with_credential_provider(Counting(Arc::default()), "identity-b");
    assert_ne!(
        first.replay_target("model").account,
        different.replay_target("model").account
    );
    assert_ne!(
        first.replay_target("model").account,
        provider().replay_target("model").account
    );
}

#[tokio::test]
async fn caller_diagnostic_names_binding_and_uses_plain_reason() {
    let error = Bedrock::new(AwsProfile::new("unused-profile", "us-east-1"))
        .with_credential_provider(Failing(false), "identity-binding")
        .authorize(&mut request())
        .await
        .unwrap_err();
    let text = error.to_string();
    assert!(text.contains("identity-binding"));
    assert!(text.contains("credentials provider failed"));
    assert!(!text.contains("unused-profile"));
    assert!(!text.contains("ProviderFailed"));
}

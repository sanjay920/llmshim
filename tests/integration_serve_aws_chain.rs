//! The AWS credential chain for the media store's `s3` backend (note 0050, slice 3a), in a process of
//! its own because the chain reads the real environment: each test clears every `AWS_*` variable,
//! sets what it needs, and holds a lock so no two tests run at once. The sources are mock servers
//! (the container credentials endpoint of EKS Pod Identity and ECS, the EC2 metadata service) and
//! files, so nothing leaves the machine.
#![cfg(feature = "serve-cloud")]

use llmshim::serve::blobstore::aws_chain::AwsChain;
use llmshim::serve::blobstore::{self, BlobError};
use llmshim::serve::config::MediaStore;
use serde_json::json;
use std::time::Duration;

static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A clean AWS environment holding only `vars`, under the lock.
async fn world(vars: &[(&str, &str)]) -> tokio::sync::MutexGuard<'static, ()> {
    let guard = ONE_AT_A_TIME.lock().await;
    for (name, _) in std::env::vars() {
        if name.starts_with("AWS_") {
            std::env::remove_var(name);
        }
    }
    // Nothing reaches the real metadata service, and no profile file of this machine is read.
    std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");
    std::env::set_var("AWS_CONFIG_FILE", "/nonexistent/aws-config");
    std::env::set_var(
        "AWS_SHARED_CREDENTIALS_FILE",
        "/nonexistent/aws-credentials",
    );
    for (name, value) in vars {
        std::env::set_var(name, value);
    }
    guard
}

fn creds_json(id: &str, expiry: &str) -> String {
    format!(
        r#"{{"AccessKeyId": "{id}", "SecretAccessKey": "secret-{id}", "Token": "session-{id}",
            "Expiration": "{expiry}"}}"#
    )
}

#[tokio::test]
async fn environment_keys_are_the_first_link() {
    let _world = world(&[
        ("AWS_ACCESS_KEY_ID", "AKIDENV"),
        ("AWS_SECRET_ACCESS_KEY", "secret-env"),
        ("AWS_SESSION_TOKEN", "token-env"),
    ])
    .await;
    let chain = AwsChain::new("us-east-1");
    assert_eq!(
        chain.get().await.unwrap(),
        (
            "AKIDENV".into(),
            "secret-env".into(),
            Some("token-env".into())
        )
    );
}

#[tokio::test]
async fn nothing_to_find_is_denied() {
    let _world = world(&[]).await;
    assert_eq!(
        AwsChain::new("us-east-1").get().await.unwrap_err(),
        BlobError::Denied
    );
}

#[tokio::test]
async fn pod_identity_and_ecs_roles_read_the_container_credentials_endpoint() {
    let mut server = mockito::Server::new_async().await;
    let dir = std::env::temp_dir().join(format!("llmshim-pod-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let token_file = dir.join("token");
    std::fs::write(&token_file, "pod-identity-token").unwrap();
    let endpoint = server
        .mock("GET", "/creds")
        .match_header("authorization", "pod-identity-token")
        .with_header("content-type", "application/json")
        .with_body(creds_json("AKIDPOD", "2099-01-01T00:00:00Z"))
        .expect(2)
        .create_async()
        .await;
    let uri = format!("{}/creds", server.url());
    let _world = world(&[
        ("AWS_CONTAINER_CREDENTIALS_FULL_URI", uri.as_str()),
        (
            "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
            token_file.to_str().unwrap(),
        ),
    ])
    .await;
    let chain = AwsChain::new("us-east-1");
    let first = chain.get().await.unwrap();
    assert_eq!(
        first,
        (
            "AKIDPOD".into(),
            "secret-AKIDPOD".into(),
            Some("session-AKIDPOD".into())
        )
    );
    // Valid for decades: the second call is the first's, not a second request.
    assert_eq!(chain.get().await.unwrap(), first);
    // A refused signature drops what is held, so the next call asks again: the endpoint's second hit.
    chain.invalidate().await;
    assert_eq!(chain.get().await.unwrap(), first);
    endpoint.assert_async().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn an_instance_profile_is_read_from_the_metadata_service() {
    let mut imds = mockito::Server::new_async().await;
    let _token = imds
        .mock("PUT", "/latest/api/token")
        .with_header("x-aws-ec2-metadata-token-ttl-seconds", "21600")
        .with_body("imds-session-token")
        .create_async()
        .await;
    let _role = imds
        .mock("GET", "/latest/meta-data/iam/security-credentials/")
        .match_header("x-aws-ec2-metadata-token", "imds-session-token")
        .with_body("node-role")
        .create_async()
        .await;
    let _creds = imds
        .mock(
            "GET",
            "/latest/meta-data/iam/security-credentials/node-role",
        )
        .match_header("x-aws-ec2-metadata-token", "imds-session-token")
        .with_body(
            r#"{"Code": "Success", "Type": "AWS-HMAC", "AccessKeyId": "AKIDIMDS",
                "SecretAccessKey": "secret-imds", "Token": "session-imds",
                "LastUpdated": "2026-01-01T00:00:00Z", "Expiration": "2099-01-01T00:00:00Z"}"#,
        )
        .create_async()
        .await;
    let endpoint = imds.url();
    let _world = world(&[("AWS_EC2_METADATA_SERVICE_ENDPOINT", endpoint.as_str())]).await;
    std::env::remove_var("AWS_EC2_METADATA_DISABLED");
    let chain = AwsChain::new("us-east-1");
    let got = chain.get().await.unwrap();
    assert_eq!(got.0, "AKIDIMDS");
    assert_eq!(got.2.as_deref(), Some("session-imds"));
}

#[tokio::test]
async fn an_eks_service_account_role_is_assumed_with_its_projected_token() {
    // IRSA: the kubelet projects a service account token; the chain trades it at STS for the role's
    // temporary credentials (`AssumeRoleWithWebIdentity`).
    let mut sts = mockito::Server::new_async().await;
    let assume = sts
        .mock("POST", "/")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::Regex("Action=AssumeRoleWithWebIdentity".into()),
            mockito::Matcher::Regex("RoleArn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fllmshim-media".into()),
            mockito::Matcher::Regex("WebIdentityToken=projected-service-account-token".into()),
        ]))
        .with_header("content-type", "text/xml")
        .with_body(
            r#"<AssumeRoleWithWebIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleWithWebIdentityResult>
    <Credentials>
      <AccessKeyId>AKIDIRSA</AccessKeyId>
      <SecretAccessKey>secret-irsa</SecretAccessKey>
      <SessionToken>session-irsa</SessionToken>
      <Expiration>2099-01-01T00:00:00Z</Expiration>
    </Credentials>
    <AssumedRoleUser><Arn>arn:aws:sts::123456789012:assumed-role/llmshim-media/s</Arn><AssumedRoleId>AROAX:s</AssumedRoleId></AssumedRoleUser>
  </AssumeRoleWithWebIdentityResult>
  <ResponseMetadata><RequestId>r</RequestId></ResponseMetadata>
</AssumeRoleWithWebIdentityResponse>"#,
        )
        .expect(1)
        .create_async()
        .await;
    let dir = std::env::temp_dir().join(format!("llmshim-irsa-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let token_file = dir.join("token");
    std::fs::write(&token_file, "projected-service-account-token").unwrap();
    let endpoint = sts.url();
    let _world = world(&[
        ("AWS_WEB_IDENTITY_TOKEN_FILE", token_file.to_str().unwrap()),
        (
            "AWS_ROLE_ARN",
            "arn:aws:iam::123456789012:role/llmshim-media",
        ),
        ("AWS_ROLE_SESSION_NAME", "llmshim"),
        ("AWS_ENDPOINT_URL_STS", endpoint.as_str()),
    ])
    .await;
    let chain = AwsChain::new("us-east-1");
    assert_eq!(
        chain.get().await.unwrap(),
        (
            "AKIDIRSA".into(),
            "secret-irsa".into(),
            Some("session-irsa".into())
        )
    );
    assume.assert_async().await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_credential_inside_its_renewal_margin_is_asked_for_again() {
    let mut server = mockito::Server::new_async().await;
    // Expires in two minutes: inside the five-minute margin, so it is never kept for long.
    let soon = chrono::Utc::now() + chrono::Duration::seconds(120);
    let endpoint = server
        .mock("GET", "/creds")
        .with_header("content-type", "application/json")
        .with_body(creds_json(
            "AKIDSOON",
            &soon.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ))
        .expect_at_least(2)
        .create_async()
        .await;
    let uri = format!("{}/creds", server.url());
    let _world = world(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", uri.as_str())]).await;
    let chain = AwsChain::new("us-east-1");
    chain.get().await.unwrap();
    // Kept for five seconds at most, so a wait past that asks again.
    tokio::time::sleep(Duration::from_secs(6)).await;
    chain.get().await.unwrap();
    endpoint.assert_async().await;
}

#[tokio::test]
async fn a_source_that_fails_is_unavailable_not_denied() {
    let mut server = mockito::Server::new_async().await;
    let _broken = server
        .mock("GET", "/creds")
        .with_status(500)
        .create_async()
        .await;
    let uri = format!("{}/creds", server.url());
    let _world = world(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", uri.as_str())]).await;
    assert_eq!(
        AwsChain::new("us-east-1").get().await.unwrap_err(),
        BlobError::Unavailable
    );
}

#[tokio::test]
async fn the_store_signs_with_the_chains_credentials_and_signs_again_after_a_refusal() {
    // The pod's role is rotated: the first request is refused (the credentials were revoked), the
    // chain is asked again, and the retry is signed with the new ones.
    let mut creds = mockito::Server::new_async().await;
    let old = creds
        .mock("GET", "/creds")
        .with_header("content-type", "application/json")
        .with_body(creds_json("AKIDOLD", "2099-01-01T00:00:00Z"))
        .expect(1)
        .create_async()
        .await;
    let new = creds
        .mock("GET", "/creds")
        .with_header("content-type", "application/json")
        .with_body(creds_json("AKIDNEW", "2099-01-01T00:00:00Z"))
        .expect(1)
        .create_async()
        .await;
    let mut s3 = mockito::Server::new_async().await;
    let refused = s3
        .mock("GET", "/media/a/b")
        .match_header(
            "authorization",
            mockito::Matcher::Regex("Credential=AKIDOLD/".into()),
        )
        .match_header("x-amz-security-token", "session-AKIDOLD")
        .with_status(403)
        .expect(1)
        .create_async()
        .await;
    let accepted = s3
        .mock("GET", "/media/a/b")
        .match_header(
            "authorization",
            mockito::Matcher::Regex("Credential=AKIDNEW/".into()),
        )
        .match_header("x-amz-security-token", "session-AKIDNEW")
        .with_status(200)
        .with_body("fine")
        .expect(1)
        .create_async()
        .await;
    let uri = format!("{}/creds", creds.url());
    let _world = world(&[("AWS_CONTAINER_CREDENTIALS_FULL_URI", uri.as_str())]).await;
    let config: MediaStore = serde_json::from_value(json!({
        "kind": "s3", "bucket": "media", "region": "us-east-1", "credentials": "aws",
        "endpoint": s3.url(), "allow_insecure_http": true,
    }))
    .unwrap();
    let handle = blobstore::from_config(&config, &|_| None).unwrap();
    // Constructing the store did no credential I/O.
    assert!(!old.matched_async().await);
    assert_eq!(handle.store.get("a/b", 100).await.unwrap(), b"fine");
    old.assert_async().await;
    new.assert_async().await;
    refused.assert_async().await;
    accepted.assert_async().await;
}

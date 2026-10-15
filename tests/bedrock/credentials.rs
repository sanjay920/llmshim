use super::*;
use llmshim::providers::bedrock::BedrockErrorKind;

#[tokio::test]
async fn missing_named_profile_never_uses_other_profile_credentials() {
    // A child isolates the deliberately hostile environment from parallel tests.
    if std::env::var_os("LLMSHIM_PROFILE_FIXTURE_CHILD").is_none() {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "credentials::missing_named_profile_never_uses_other_profile_credentials",
                ])
                .env("LLMSHIM_PROFILE_FIXTURE_CHILD", "1")
                .env("AWS_ACCESS_KEY_ID", "EXPORTED_SYNTHETIC")
                .env("AWS_SECRET_ACCESS_KEY", "wrong-environment-secret")
                .env("AWS_PROFILE", "default")
                .env("AWS_REGION", "eu-west-1")
                .env("AWS_EC2_METADATA_DISABLED", "true")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    } else {
        missing_profile().await;
    }
}

async fn missing_profile() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    let credentials = dir.path().join("credentials");
    std::fs::write(&config, "").unwrap();
    std::fs::write(
        &credentials,
        "[default]\naws_access_key_id = AMBIENT\naws_secret_access_key = wrong\n",
    )
    .unwrap();
    let provider = provider().with_profile_files(config, credentials);
    let mut request = reqwest::Request::new(
        reqwest::Method::POST,
        "https://bedrock-runtime.us-east-1.amazonaws.com/model/model/converse"
            .parse()
            .unwrap(),
    );
    *request.body_mut() = Some("{}".into());
    let error = provider.authorize(&mut request).await.unwrap_err();
    assert_eq!(error.kind, BedrockErrorKind::Credentials);
    assert!(request.headers().get("authorization").is_none());
    assert!(!format!("{error:?}").contains("AMBIENT"));
    let error = llmshim::client::ShimClient::new_scoped()
        .unwrap()
        .completion(
            &provider,
            "model",
            &serde_json::json!({
                "messages":[{"role":"user","content":"Hello"}]
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(
        llmshim::providers::bedrock::BedrockError::from_shim(&error)
            .unwrap()
            .kind,
        BedrockErrorKind::Credentials
    );
}

#[tokio::test]
async fn named_profile_cache_keeps_identity_and_explicit_region() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    let credentials = dir.path().join("credentials");
    std::fs::write(&config, "[profile fixture]\nregion = eu-west-1\n").unwrap();
    let provider = provider().with_profile_files(config, credentials.clone());
    for key in ["FIRST", "SECOND"] {
        std::fs::write(
            &credentials,
            format!("[fixture]\naws_access_key_id = {key}\naws_secret_access_key = synthetic\n"),
        )
        .unwrap();
        let mut request = reqwest::Request::new(
            reqwest::Method::POST,
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/model/converse"
                .parse()
                .unwrap(),
        );
        *request.body_mut() = Some("{\"message\":\"é\"}".into());
        let body = request.body().unwrap().as_bytes().unwrap().to_vec();
        provider.authorize(&mut request).await.unwrap();
        let auth = request.headers()["authorization"].to_str().unwrap();
        assert!(auth.contains("Credential=FIRST/"));
        assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
        assert_eq!(request.body().unwrap().as_bytes().unwrap(), body);
    }
}

#[tokio::test]
async fn temporary_credentials_sign_exact_json_bytes_and_replace_old_session_tokens() {
    use aws_credential_types::Credentials;
    let provider = provider().with_credential_provider(
        Credentials::new(
            "SYNTHETIC",
            "synthetic-secret",
            Some("synthetic-session".into()),
            None,
            "fixture",
        ),
        "fixture",
    );
    let mut prepared = reqwest::Client::new()
        .post(
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/arn%3Aexample%2Fmodel/converse",
        )
        .json(&serde_json::json!({"messages":[{"role":"user","content":[{"text":"é"}]}]}))
        .build()
        .unwrap();
    let bytes = prepared.body().unwrap().as_bytes().unwrap().to_vec();
    provider.authorize(&mut prepared).await.unwrap();
    assert_eq!(
        prepared.headers()["x-amz-security-token"],
        "synthetic-session"
    );
    assert!(prepared.headers()["authorization"]
        .to_str()
        .unwrap()
        .contains("x-amz-security-token"));
    assert_eq!(prepared.body().unwrap().as_bytes().unwrap(), bytes);
    let provider = super::provider().with_credential_provider(
        Credentials::new("SYNTHETIC", "synthetic-secret", None, None, "fixture"),
        "fixture",
    );
    provider.authorize(&mut prepared).await.unwrap();
    assert!(prepared.headers().get("x-amz-security-token").is_none());
    assert_eq!(prepared.body().unwrap().as_bytes().unwrap(), bytes);
}

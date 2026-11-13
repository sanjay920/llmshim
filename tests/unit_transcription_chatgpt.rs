//! Subscription dictation uses the selected OAuth account and an in-memory file upload.
use llmshim::{
    audio::TranscriptionRequest,
    providers::chatgpt::{ChatGpt, ChatGptAuth},
    router::Router,
};
use mockito::{Matcher, Server};
use serde_json::json;

#[tokio::test]
async fn subscription_dictation_uses_selected_account_and_file_only_upload() {
    let mut server = Server::new_async().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.json");
    std::fs::write(
        &path,
        json!({
            "access_token": "voice-test-access", "refresh_token": "voice-test-refresh",
            "account_id": "voice-test-account",
            "expires_at": chrono::Utc::now().timestamp() + 3600,
        })
        .to_string(),
    )
    .unwrap();
    let original = std::fs::read(&path).unwrap();
    let upload = server.mock("POST", "/backend-api/transcribe")
        .match_header("authorization", "Bearer voice-test-access")
        .match_header("chatgpt-account-id", "voice-test-account")
        .match_header("user-agent", Matcher::Regex("llmshim".into()))
        .match_request(|request| {
            let bytes = request.body().unwrap();
            bytes.windows(b"voice-pcm-canary".len()).any(|part| part == b"voice-pcm-canary")
                && !bytes.windows(b"name=\"model\"".len()).any(|part| part == b"name=\"model\"")
                && !bytes.windows(b"name=\"prompt\"".len()).any(|part| part == b"name=\"prompt\"")
        })
        .with_header("content-type", "application/json")
        .with_body(r#"{"text":"Approved 412, draft 309.","asset_ttl":"30d","asset_format":"wav","asset_pointer":"private-asset"}"#)
        .expect(1).create_async().await;
    let router = Router::new().register(
        "chatgpt",
        Box::new(
            ChatGpt::new(ChatGptAuth::new(path.clone()))
                .with_base_url(format!("{}/backend-api/codex", server.url())),
        ),
    );
    let models = llmshim::transcription_models(&router);
    assert!(models.iter().any(|model| model.id == "chatgpt/transcribe"));
    let request = TranscriptionRequest::new(
        "chatgpt/transcribe",
        b"voice-pcm-canary".to_vec(),
        "recording.wav",
        "audio/wav",
    );
    let answer = llmshim::transcription(&router, &request).await.unwrap();
    assert_eq!(answer.text, "Approved 412, draft 309.");
    assert!(answer
        .usage
        .get("cost_usd")
        .is_none_or(serde_json::Value::is_null));
    assert!(answer
        .usage
        .get("cost_source")
        .is_none_or(serde_json::Value::is_null));
    assert!(!serde_json::to_string(&answer.usage)
        .unwrap()
        .contains("private-asset"));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    upload.assert_async().await;
}

fn selected_account(expired: bool) -> (tempfile::TempDir, ChatGptAuth) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("selected.json");
    std::fs::write(
        &path,
        json!({
            "access_token":"voice-old-access", "refresh_token":"voice-refresh",
            "account_id":"selected-account",
            "expires_at":if expired { 1 } else { chrono::Utc::now().timestamp() + 3600 },
        })
        .to_string(),
    )
    .unwrap();
    (directory, ChatGptAuth::new(path))
}

fn audio() -> TranscriptionRequest {
    TranscriptionRequest::new(
        "chatgpt/transcribe",
        vec![0, 255, 1],
        "clip.wav",
        "audio/wav",
    )
}

#[test]
fn transcription_keeps_the_explicit_origin_and_prefix() {
    use llmshim::provider::Provider;
    let (_directory, auth) = selected_account(false);
    for (base, expected) in [
        (
            "https://chatgpt.com/backend-api/codex",
            "https://chatgpt.com/backend-api/transcribe",
        ),
        (
            "https://chatgpt.com/backend-api/codex/",
            "https://chatgpt.com/backend-api/transcribe",
        ),
        (
            "http://127.0.0.1:1234/prefix/codex",
            "http://127.0.0.1:1234/prefix/transcribe",
        ),
        (
            "https://gateway.example/prefix",
            "https://gateway.example/prefix/transcribe",
        ),
        (
            "https://chatgpt.com.example/backend-api/codex",
            "https://chatgpt.com.example/backend-api/transcribe",
        ),
    ] {
        let provider = ChatGpt::new(auth.clone()).with_base_url(base.into());
        assert_eq!(
            provider
                .transcription_request("transcribe", &audio())
                .unwrap()
                .request
                .url,
            expected
        );
    }
}

#[tokio::test]
async fn unsupported_transcription_inputs_fail_before_authentication() {
    use llmshim::provider::Provider;
    let directory = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(directory.path().join("absent.json"));
    let provider = ChatGpt::new(auth.clone());
    let mut requests = Vec::new();
    let mut request = audio();
    request.prompt = Some("private context".into());
    requests.push(request);
    let mut request = audio();
    request.language = Some("en".into());
    requests.push(request);
    let mut request = audio();
    request.temperature = Some(0.5);
    requests.push(request);
    let mut request = audio();
    request.response_format = Some("text".into());
    requests.push(request);
    let mut request = audio();
    request.media_type = "audio/webm".into();
    requests.push(request);
    for request in requests {
        let error = provider
            .prepare_transcription_request("transcribe", &request)
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error,
            llmshim::error::ShimError::ProviderError { status: 400, .. }
        ));
    }
    for base in [
        "file:///private",
        "ftp://example.com",
        "https://user:password@example.com",
        "https://example.com?query=secret",
        "https://example.com#fragment",
    ] {
        let provider = ChatGpt::new(auth.clone()).with_base_url(base.into());
        let error = provider
            .prepare_transcription_request("transcribe", &audio())
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error,
            llmshim::error::ShimError::ProviderError { status: 400, .. }
        ));
        assert!(!error.to_string().contains(base));
    }
    let error = provider
        .prepare_transcription_request("unknown", &audio())
        .await
        .err()
        .unwrap();
    assert!(matches!(
        error,
        llmshim::error::ShimError::ProviderError { status: 400, .. }
    ));
    assert!(!auth.auth_path().exists());
}

#[tokio::test]
async fn transcription_refresh_uses_the_existing_selected_account_lock() {
    let mut server = Server::new_async().await;
    let (_directory, auth) = selected_account(true);
    let refresh = server.mock("POST", "/oauth/token")
        .match_body(Matcher::PartialJson(json!({"refresh_token":"voice-refresh"})))
        .with_header("content-type","application/json")
        .with_body(json!({"access_token":"voice-new-access","refresh_token":"voice-new-refresh","expires_in":3600}).to_string())
        .expect(1).create_async().await;
    let upload = server
        .mock("POST", "/transcribe")
        .match_header("authorization", "Bearer voice-new-access")
        .match_header("chatgpt-account-id", "selected-account")
        .with_header("content-type", "application/json")
        .with_body(r#"{"text":"refreshed words"}"#)
        .expect(2)
        .create_async()
        .await;
    let first = ChatGpt::new(auth.clone().with_auth_base(server.url())).with_base_url(server.url());
    let second =
        ChatGpt::new(ChatGptAuth::new(auth.auth_path().to_owned()).with_auth_base(server.url()))
            .with_base_url(server.url());
    let client = llmshim::client::ShimClient::new_scoped().unwrap();
    let request = audio();
    let (a, b) = tokio::join!(
        client.transcription(&first, "transcribe", &request),
        client.transcription(&second, "transcribe", &request)
    );
    assert_eq!(a.unwrap().text, "refreshed words");
    assert_eq!(b.unwrap().text, "refreshed words");
    refresh.assert_async().await;
    upload.assert_async().await;
}

#[tokio::test]
async fn transcription_rejects_redirects_and_malformed_text() {
    let mut server = Server::new_async().await;
    let (_directory, auth) = selected_account(false);
    let provider = ChatGpt::new(auth).with_base_url(server.url());
    let client = llmshim::client::ShimClient::new_scoped().unwrap();
    let redirected = server
        .mock("POST", "/redirected")
        .expect(0)
        .create_async()
        .await;
    for (status, body) in [
        (307, ""),
        (200, r#"{"asset_pointer":"private-asset"}"#),
        (200, r#"{"text":"   "}"#),
    ] {
        let upload = server
            .mock("POST", "/transcribe")
            .with_status(status)
            .with_header("location", &format!("{}/redirected", server.url()))
            .with_header("content-type", "application/json")
            .with_body(body)
            .expect(1)
            .create_async()
            .await;
        let error = client
            .transcription(&provider, "transcribe", &audio())
            .await
            .err()
            .unwrap();
        assert!(!error.to_string().contains("private-asset"));
        upload.assert_async().await;
        upload.remove_async().await;
    }
    redirected.assert_async().await;
}

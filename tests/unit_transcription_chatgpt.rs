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

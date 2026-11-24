//! Shared-file OAuth tests through the ChatGPT provider entry points.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use llmshim::{
    provider::Provider,
    providers::chatgpt::{ChatGpt, ChatGptAuth, LoginStatus},
    router::Router,
};
use mockito::{Matcher, Server};
use serde_json::{json, Value};

fn jwt(expiry: i64) -> String {
    format!(
        "e30.{}.test",
        URL_SAFE_NO_PAD.encode(json!({"exp": expiry}).to_string())
    )
}

fn codex_record(expiry: i64) -> Value {
    json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null,
        "tokens": {"access_token": jwt(expiry), "refresh_token": "old-refresh",
            "id_token": "old-id", "account_id": "account", "future_token_field": [1,2]},
        "last_refresh": "2020-01-01T00:00:00Z", "future_field": {"keep": true}})
}

#[test]
fn codex_cache_routes_new_model_and_rejects_invalid_tokens_and_models() {
    let directory = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(directory.path().join("auth.json"));
    let record = codex_record(chrono::Utc::now().timestamp() + 3600);
    std::fs::write(auth.auth_path(), record.to_string()).unwrap();
    assert_eq!(auth.status().unwrap(), LoginStatus::Ready);
    let router = Router::new().register("chatgpt", Box::new(ChatGpt::new(auth.clone())));
    let input = json!({"messages": [{"role":"user","content":"hi"}], "reasoning_effort":"max"});
    let (provider, model) = router.resolve("chatgpt/gpt-6.1-sol").unwrap();
    let native = provider.transform_request(&model, &input).unwrap();
    assert_eq!(native.body["model"], "gpt-6.1-sol");
    assert_eq!(native.body["reasoning"]["effort"], "max");
    assert!(native.headers.contains(&(
        "Authorization".into(),
        format!(
            "Bearer {}",
            record["tokens"]["access_token"].as_str().unwrap()
        )
    )));
    assert!(native
        .headers
        .contains(&("ChatGPT-Account-Id".into(), "account".into())));
    assert!(provider
        .transform_request("gpt-6.1-sol-unknown", &input)
        .is_err());
    assert_eq!(
        std::fs::read(auth.auth_path()).unwrap(),
        record.to_string().as_bytes()
    );
    for invalid in [
        json!(null),
        json!({"refresh_token":"private"}),
        json!({"access_token":42}),
    ] {
        let mut malformed = record.clone();
        malformed["tokens"] = invalid;
        std::fs::write(auth.auth_path(), malformed.to_string()).unwrap();
        let error = auth.status().unwrap_err().to_string();
        assert!(error.contains(&auth.auth_path().display().to_string()));
        assert!(!error.contains("private"));
    }
    std::fs::write(auth.auth_path(), codex_record(1).to_string()).unwrap();
    assert_eq!(auth.status().unwrap(), LoginStatus::NeedsRefresh);
}

#[tokio::test]
async fn codex_refresh_preserves_document_and_serializes_concurrent_requests() {
    let mut server = Server::new_async().await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.json");
    let original = codex_record(1);
    std::fs::write(&path, original.to_string()).unwrap();
    let expiry = chrono::Utc::now().timestamp() + 3600;
    let new_access = jwt(expiry);
    let refresh = server.mock("POST", "/oauth/token")
        .match_body(Matcher::PartialJson(json!({"refresh_token":"old-refresh", "grant_type":"refresh_token"})))
        .with_body(json!({"access_token":new_access,"refresh_token":"rotated-refresh","id_token":"new-id"}).to_string())
        .expect(1).create_async().await;
    let auth = ChatGptAuth::new(path.clone()).with_auth_base(server.url());
    let first = ChatGpt::new(auth.clone());
    let second = ChatGpt::new(ChatGptAuth::new(path.clone()).with_auth_base(server.url()));
    let input = json!({"messages": []});
    let before = chrono::Utc::now();
    let (one, two) = tokio::join!(
        first.prepare_request("gpt-6.1-sol", &input),
        second.prepare_request("gpt-6.1-sol", &input)
    );
    for request in [one.unwrap(), two.unwrap()] {
        assert!(request
            .headers
            .contains(&("Authorization".into(), format!("Bearer {new_access}"))));
    }
    let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let refreshed: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(stored["last_refresh"].clone()).unwrap();
    assert!(refreshed >= before && refreshed <= chrono::Utc::now());
    let mut expected = original;
    expected["tokens"]["access_token"] = json!(new_access);
    expected["tokens"]["refresh_token"] = json!("rotated-refresh");
    expected["tokens"]["id_token"] = json!("new-id");
    expected["tokens"]["expires_at"] = json!(expiry);
    expected["last_refresh"] = stored["last_refresh"].clone();
    assert_eq!(stored, expected);
    assert_eq!(auth.status().unwrap(), LoginStatus::Ready);
    first.prepare_request("gpt-6.1-sol", &input).await.unwrap();
    refresh.assert_async().await;
}

#[tokio::test]
async fn explicit_login_repairs_corrupt_cache_and_preserves_valid_codex_fields() {
    for initial in ["{private".to_owned(), codex_record(1).to_string()] {
        let mut server = Server::new_async().await;
        let directory = tempfile::tempdir().unwrap();
        let auth =
            ChatGptAuth::new(directory.path().join("auth.json")).with_auth_base(server.url());
        std::fs::write(auth.auth_path(), &initial).unwrap();
        let start = server
            .mock("POST", "/api/accounts/deviceauth/usercode")
            .with_body(json!({"device_auth_id":"device","user_code":"code"}).to_string())
            .expect(1)
            .create_async()
            .await;
        let poll = server
            .mock("POST", "/api/accounts/deviceauth/token")
            .with_body(
                json!({"authorization_code":"authorized","code_verifier":"verifier"}).to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let exchange = server
            .mock("POST", "/oauth/token")
            .with_body(
                json!({"access_token":"fresh","refresh_token":"rotated","expires_in":3600})
                    .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let code = auth.start_login().await.unwrap();
        auth.finish_login(code).await.unwrap();
        assert_eq!(auth.status().unwrap(), LoginStatus::Ready);
        let stored: Value =
            serde_json::from_slice(&std::fs::read(auth.auth_path()).unwrap()).unwrap();
        if initial.starts_with("{private") {
            assert_eq!(stored["access_token"], "fresh");
            assert!(stored.get("tokens").is_none());
        } else {
            assert_eq!(stored["tokens"]["access_token"], "fresh");
            assert_eq!(stored["future_field"], json!({"keep":true}));
            assert_eq!(stored["tokens"]["future_token_field"], json!([1, 2]));
        }
        start.assert_async().await;
        poll.assert_async().await;
        exchange.assert_async().await;
    }
}

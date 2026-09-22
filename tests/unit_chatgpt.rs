//! Local mock tests: no real credentials, OAuth servers, or model calls.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures::StreamExt;
use llmshim::{
    provider::Provider,
    providers::chatgpt::{ChatGpt, ChatGptAuth, LoginStatus},
    router::Router,
};
use mockito::{Matcher, Server};
use serde_json::{json, Value};
use tempfile::TempDir;

fn jwt(payload: Value) -> String {
    format!("e30.{}.test", URL_SAFE_NO_PAD.encode(payload.to_string()))
}

fn token_record(expired: bool) -> Value {
    json!({"access_token": "test-access", "refresh_token": "test-refresh", "account_id": "test-account",
        "expires_at": if expired { 1 } else { chrono::Utc::now().timestamp() + 3600 }})
}

#[test]
fn subscription_cache_key_and_native_schema_overrides_are_normalized() {
    let (_dir, auth) = auth_fixture(token_record(false));
    let p = ChatGpt::new(auth);
    let mut req = request();
    req["x-cache"] = json!({"key":"session:branch"});
    req["x-chatgpt"] = json!({"prompt_cache_key":"old","tools":[{"type":"function","name":"f","strict":true,"parameters":{"type":"object","properties":{"optional":{"type":"string","default":"a"}}}}]});
    let body = p.transform_request("gpt-6-astra", &req).unwrap().body;
    assert_eq!(body["prompt_cache_key"], "session:branch");
    assert!(body.get("x-cache").is_none());
    assert_eq!(
        body["tools"][0]["parameters"]["additionalProperties"],
        false
    );
    assert_eq!(
        body["tools"][0]["parameters"]["required"],
        json!(["optional"])
    );
}

fn auth_fixture(record: Value) -> (TempDir, ChatGptAuth) {
    let dir = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(dir.path().join("auth.json"));
    std::fs::write(auth.auth_path(), record.to_string()).unwrap();
    (dir, auth)
}

fn request() -> Value {
    json!({"model": "chatgpt/gpt-6-astra", "messages": [{"role": "user", "content": "Hi"}]})
}

fn terminal(output: Value) -> Value {
    json!({"type": "response.completed", "response": {
        "id": "resp_test", "status": "completed", "output": output,
        "usage": {"input_tokens": 10, "output_tokens": 4, "total_tokens": 14,
            "input_tokens_details": {"cached_tokens": 2}, "output_tokens_details": {"reasoning_tokens": 1}}
    }})
}

fn message(text: &str) -> Value {
    json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
}

fn sse(events: Vec<Value>) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\r\ndata:{}\r\n\r\n",
                event["type"].as_str().unwrap(),
                event
            )
        })
        .collect()
}

#[test]
fn translates_chat_tools_images_and_enforces_backend_constraints() {
    let (_dir, auth) = auth_fixture(token_record(false));
    let provider = ChatGpt::new(auth);
    let req = json!({
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "Be brief."}]},
            {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}]},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "weather", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "Sunny"}
        ],
        "tools": [{"type": "function", "function": {"name": "weather", "parameters": {"type": "object"}}}],
        "tool_choice": {"type": "function", "function": {"name": "weather"}},
        "reasoning_effort": "high", "max_tokens": 1, "max_completion_tokens": 2,
        "temperature": 0.7, "metadata": {"secret": "ignored"},
        "x-openai": {"instructions": "must not cross providers"},
        "x-chatgpt": {"model": "wrong", "store": true, "stream": false, "max_output_tokens": 3, "metadata": {}, "include": []}
    });
    let native = provider.transform_request("gpt-6-astra", &req).unwrap();
    assert_eq!(
        native.url,
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(native.body["model"], "gpt-6-astra");
    assert_eq!(native.body["instructions"], "Be brief.");
    assert_eq!(native.body["store"], false);
    assert_eq!(native.body["stream"], true);
    assert_eq!(
        native.body["include"],
        json!(["reasoning.encrypted_content"])
    );
    assert_eq!(native.body["reasoning"]["effort"], "high");
    assert_eq!(native.body["input"][0]["content"][0]["type"], "input_image");
    assert_eq!(native.body["input"][1]["type"], "function_call");
    assert_eq!(native.body["input"][2]["type"], "function_call_output");
    assert_eq!(native.body["tools"][0]["name"], "weather");
    assert_eq!(
        native.body["tool_choice"],
        json!({"type": "function", "name": "weather"})
    );
    for key in [
        "max_tokens",
        "max_completion_tokens",
        "max_output_tokens",
        "metadata",
        "temperature",
    ] {
        assert!(native.body.get(key).is_none(), "unexpected {key}");
    }
    assert!(native
        .headers
        .contains(&("Authorization".into(), "Bearer test-access".into())));
    assert!(native
        .headers
        .contains(&("ChatGPT-Account-Id".into(), "test-account".into())));
    assert!(native
        .headers
        .contains(&("Accept".into(), "text/event-stream".into())));
}

#[test]
fn supported_catalog_and_reasoning_match_the_four_current_models() {
    let expected = [
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
    ];
    let catalog = llmshim::models::available_models(&["chatgpt"]);
    assert_eq!(catalog.iter().map(|m| m.name).collect::<Vec<_>>(), expected);
    let (_dir, auth) = auth_fixture(token_record(false));
    let provider = ChatGpt::new(auth);
    for model in expected {
        for (effort, expected) in [
            ("max", "max"),
            ("high", "high"),
            ("minimal", "low"),
            (
                "none",
                if model == "gpt-6-astra" {
                    "low"
                } else {
                    "none"
                },
            ),
        ] {
            for field in ["reasoning_effort", "output_config"] {
                let mut input = request();
                input[field] = if field == "output_config" {
                    json!({"effort": effort})
                } else {
                    json!(effort)
                };
                let native = provider.transform_request(model, &input).unwrap();
                assert_eq!(native.body["model"], model);
                assert_eq!(
                    native.body["reasoning"]["effort"], expected,
                    "{model} {field} {effort}"
                );
            }
        }
    }
    let native = provider
        .transform_request(
            "gpt-6-astra",
            &json!({
                "messages": [], "reasoning_effort": "max", "reasoning_mode": "pro"
            }),
        )
        .unwrap();
    assert_eq!(native.body["reasoning"]["effort"], "max");
}

#[tokio::test]
async fn older_and_unlisted_models_fail_before_authentication_or_dispatch() {
    let mut server = Server::new_async().await;
    let untouched = server
        .mock("POST", Matcher::Any)
        .expect(0)
        .create_async()
        .await;
    let dir = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(dir.path().join("auth.json")).with_auth_base(server.url());
    let provider = ChatGpt::new(auth).with_base_url(server.url());
    for model in [
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.3-codex",
        "gpt-4o",
        "gpt-5.6",
        "gpt-5.6-sol-old",
        "gpt-6-unknown",
        "",
    ] {
        let sync_error = provider.transform_request(model, &request()).err().unwrap();
        let async_error = provider
            .prepare_request(model, &request())
            .await
            .err()
            .unwrap();
        for error in [sync_error, async_error] {
            assert!(matches!(
                error,
                llmshim::error::ShimError::ProviderError { status: 400, .. }
            ));
            assert!(error.to_string().contains("unsupported model"));
        }
    }
    let router = Router::new()
        .register("chatgpt", Box::new(provider))
        .alias("retired", "chatgpt/gpt-5.4");
    for model in ["chatgpt/gpt-5.4", "retired"] {
        let input = json!({"model": model, "messages": []});
        assert!(matches!(
            llmshim::completion(&router, &input).await.err().unwrap(),
            llmshim::error::ShimError::ProviderError { status: 400, .. }
        ));
        assert!(matches!(
            llmshim::stream(&router, &input).await.err().unwrap(),
            llmshim::error::ShimError::ProviderError { status: 400, .. }
        ));
    }
    untouched.assert_async().await;
}

#[tokio::test]
async fn device_login_exchanges_encoded_form_persists_and_logs_out() {
    let mut server = Server::new_async().await;
    let dir = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(dir.path().join("private/auth.json")).with_auth_base(server.url());
    assert_eq!(auth.status().unwrap(), LoginStatus::SignedOut);
    let start = server
        .mock("POST", "/api/accounts/deviceauth/usercode")
        .match_body(Matcher::Json(
            json!({"client_id": "app_EMoamEEZ73f0CkXaXp7hrann"}),
        ))
        .with_body(
            json!({"device_auth_id": "device", "usercode": "ABCD-1234", "interval": "5"})
                .to_string(),
        )
        .create_async()
        .await;
    let poll = server.mock("POST", "/api/accounts/deviceauth/token")
        .match_body(Matcher::Json(json!({"device_auth_id": "device", "user_code": "ABCD-1234"})))
        .with_body(json!({"authorization_code": "a+&b", "code_verifier": "v+&z", "code_challenge": "challenge"}).to_string()).create_async().await;
    let exchange = server
        .mock("POST", "/oauth/token")
        .match_header("content-type", "application/x-www-form-urlencoded")
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex("code=a%2B%26b".into()),
            Matcher::Regex("code_verifier=v%2B%26z".into()),
            Matcher::Regex("grant_type=authorization_code".into()),
        ]))
        .with_body(
            json!({"access_token": jwt(json!({"exp": chrono::Utc::now().timestamp() + 3600,
            "https://api.openai.com/auth": {"chatgpt_account_id": "derived-account"}})),
            "refresh_token": "saved-refresh", "id_token": jwt(json!({}))})
            .to_string(),
        )
        .create_async()
        .await;
    let code = auth.start_login().await.unwrap();
    assert_eq!(code.user_code, "ABCD-1234");
    assert_eq!(
        code.verification_url,
        format!("{}/codex/device", server.url())
    );
    auth.finish_login(code).await.unwrap();
    start.assert_async().await;
    poll.assert_async().await;
    exchange.assert_async().await;
    assert_eq!(auth.status().unwrap(), LoginStatus::Ready);
    let stored: Value = serde_json::from_slice(&std::fs::read(auth.auth_path()).unwrap()).unwrap();
    assert_eq!(stored["account_id"], "derived-account");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(auth.auth_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(auth.auth_path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    auth.logout().await.unwrap();
    auth.logout().await.unwrap();
    assert_eq!(auth.status().unwrap(), LoginStatus::SignedOut);
}

#[tokio::test]
async fn concurrent_requests_refresh_once_and_persist_rotated_tokens() {
    let mut server = Server::new_async().await;
    let (_dir, auth) = auth_fixture(token_record(true));
    let auth = auth.with_auth_base(server.url());
    assert_eq!(auth.status().unwrap(), LoginStatus::NeedsRefresh);
    let refresh = server.mock("POST", "/oauth/token")
        .match_body(Matcher::Json(json!({"client_id": "app_EMoamEEZ73f0CkXaXp7hrann", "grant_type": "refresh_token",
            "refresh_token": "test-refresh", "scope": "openid profile email"})))
        .with_body(json!({"access_token": "new-access", "refresh_token": "rotated-refresh", "expires_in": 3600,
            "id_token": jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "new-account"}}))}).to_string())
        .expect(1).create_async().await;
    // Distinct auth instances sharing only the file path, as in two processes.
    let a = ChatGpt::new(auth.clone());
    let b =
        ChatGpt::new(ChatGptAuth::new(auth.auth_path().to_owned()).with_auth_base(server.url()));
    let input = request();
    let (one, two) = tokio::join!(
        a.prepare_request("gpt-6-astra", &input),
        b.prepare_request("gpt-6-astra", &input)
    );
    for req in [one.unwrap(), two.unwrap()] {
        assert!(req
            .headers
            .contains(&("Authorization".into(), "Bearer new-access".into())));
        assert!(req
            .headers
            .contains(&("ChatGPT-Account-Id".into(), "new-account".into())));
    }
    refresh.assert_async().await;
    let stored: Value = serde_json::from_slice(&std::fs::read(auth.auth_path()).unwrap()).unwrap();
    assert_eq!(stored["refresh_token"], "rotated-refresh");
    assert_eq!(auth.status().unwrap(), LoginStatus::Ready);
}

#[tokio::test]
async fn refresh_without_rotation_preserves_refresh_token() {
    let mut server = Server::new_async().await;
    let (_dir, auth) = auth_fixture(token_record(true));
    let auth = auth.with_auth_base(server.url());
    let refresh = server
        .mock("POST", "/oauth/token")
        .with_body(json!({"access_token": "fresh", "expires_in": 3600}).to_string())
        .create_async()
        .await;
    ChatGpt::new(auth.clone())
        .prepare_request("gpt-6-astra", &request())
        .await
        .unwrap();
    let stored: Value = serde_json::from_slice(&std::fs::read(auth.auth_path()).unwrap()).unwrap();
    assert_eq!(stored["refresh_token"], "test-refresh");
    assert_eq!(stored["account_id"], "test-account");
    refresh.assert_async().await;
}

#[tokio::test]
async fn revoked_refresh_returns_redacted_error_without_starting_login() {
    let mut server = Server::new_async().await;
    let (_dir, auth) = auth_fixture(token_record(true));
    let auth = auth.with_auth_base(server.url());
    let before = std::fs::read(auth.auth_path()).unwrap();
    let refresh = server
        .mock("POST", "/oauth/token")
        .with_status(401)
        .with_body("sensitive-refresh-token")
        .create_async()
        .await;
    let login = server
        .mock("POST", "/api/accounts/deviceauth/usercode")
        .expect(0)
        .create_async()
        .await;
    let error = ChatGpt::new(auth.clone())
        .prepare_request("gpt-6-astra", &request())
        .await
        .err()
        .unwrap();
    assert!(!error.to_string().contains("sensitive-refresh-token"));
    assert!(error.to_string().contains("llmshim login chatgpt"));
    assert_eq!(std::fs::read(auth.auth_path()).unwrap(), before);
    refresh.assert_async().await;
    login.assert_async().await;
}

#[tokio::test]
async fn oauth_redirect_does_not_forward_tokens() {
    let mut target = Server::new_async().await;
    let leaked = target.mock("POST", "/leak").expect(0).create_async().await;
    let mut server = Server::new_async().await;
    let redirect = server
        .mock("POST", "/oauth/token")
        .with_status(307)
        .with_header("location", &format!("{}/leak", target.url()))
        .create_async()
        .await;
    let (_dir, auth) = auth_fixture(token_record(true));
    let error = ChatGpt::new(auth.with_auth_base(server.url()))
        .prepare_request("gpt-6-astra", &request())
        .await
        .err()
        .unwrap();
    assert!(matches!(
        error,
        llmshim::error::ShimError::ProviderError { status: 307, .. }
    ));
    redirect.assert_async().await;
    leaked.assert_async().await;
}

#[tokio::test]
async fn missing_corrupt_and_unrefreshable_cache_fail_without_network_login() {
    let dir = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(dir.path().join("auth.json"));
    let provider = ChatGpt::new(auth.clone());
    assert!(provider
        .prepare_request("gpt-6-astra", &request())
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("login chatgpt"));
    std::fs::write(auth.auth_path(), "{private-broken-cache").unwrap();
    let error = provider
        .prepare_request("gpt-6-astra", &request())
        .await
        .err()
        .unwrap();
    assert!(!error.to_string().contains("private-broken-cache"));
    std::fs::write(
        auth.auth_path(),
        json!({"access_token": "opaque"}).to_string(),
    )
    .unwrap();
    assert_eq!(auth.status().unwrap(), LoginStatus::NeedsLogin);
    assert!(provider
        .prepare_request("gpt-6-astra", &request())
        .await
        .is_err());
}

#[tokio::test]
async fn completion_collects_sse_and_preserves_usage_and_all_text() {
    let mut server = Server::new_async().await;
    let (_dir, auth) = auth_fixture(token_record(false));
    let response = server
        .mock("POST", "/responses")
        .match_header("authorization", "Bearer test-access")
        .match_header("chatgpt-account-id", "test-account")
        .match_body(Matcher::PartialJson(
            json!({"stream": true, "store": false, "model": "gpt-6-astra"}),
        ))
        .with_header("content-type", "text/event-stream")
        .with_body(sse(vec![terminal(json!([
            message("Hello "),
            message("世界")
        ]))]))
        .create_async()
        .await;
    let router = Router::new().register(
        "chatgpt",
        Box::new(ChatGpt::new(auth).with_base_url(server.url())),
    );
    let result = llmshim::completion(&router, &request()).await.unwrap();
    assert_eq!(result["model"], "chatgpt/gpt-6-astra");
    assert_eq!(result["choices"][0]["message"]["content"], "Hello 世界");
    assert_eq!(result["choices"][0]["finish_reason"], "stop");
    assert_eq!(result["usage"]["total_tokens"], 14);
    assert_eq!(result["usage"]["prompt_tokens_details"]["cached_tokens"], 2);
    response.assert_async().await;
}

#[tokio::test]
async fn fallback_collects_subscription_sse_and_keeps_its_wire_identity() {
    let mut server = Server::new_async().await;
    let (_dir, auth) = auth_fixture(token_record(false));
    let mock=server.mock("POST","/responses").match_header("chatgpt-account-id","test-account")
        .with_body(sse(vec![terminal(json!([{"type":"function_call","id":"fc_item","call_id":"native_call","name":"weather","arguments":"{}"}]))])).create_async().await;
    let router = Router::new().register(
        "chatgpt",
        Box::new(ChatGpt::new(auth).with_base_url(server.url())),
    );
    let config = llmshim::FallbackConfig::new(vec![
        "unconfigured/model".into(),
        "chatgpt/gpt-6-astra".into(),
    ])
    .max_retries(0);
    let result = llmshim::completion_with_fallback(&router, &request(), &config, None)
        .await
        .unwrap();
    assert_eq!(result["model"], "chatgpt/gpt-6-astra");
    let call = &result["choices"][0]["message"]["tool_calls"][0];
    assert!(call["id"].as_str().unwrap().starts_with("call_ls_"));
    assert_eq!(call["wire_ids"][0]["provider"], "chatgpt");
    assert_eq!(call["wire_ids"][0]["item_id"], "fc_item");
    mock.assert_async().await;
}

#[tokio::test]
async fn empty_terminal_output_recovers_text_and_function_items() {
    let mut server = Server::new_async().await;
    let (_dir, auth) = auth_fixture(token_record(false));
    let tool =
        json!({"type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{}"});
    let response = server.mock("POST", "/responses").with_body(sse(vec![
        json!({"type": "response.output_text.done", "output_index": 0, "content_index": 0, "text": "Checking."}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": tool}),
        terminal(json!([]))
    ])).create_async().await;
    let router = Router::new().register(
        "chatgpt",
        Box::new(ChatGpt::new(auth).with_base_url(server.url())),
    );
    let result = llmshim::completion(&router, &request()).await.unwrap();
    assert_eq!(result["choices"][0]["message"]["content"], "Checking.");
    assert_eq!(
        result["choices"][0]["message"]["tool_calls"][0]["wire_ids"][0]["id"],
        "call_1"
    );
    assert_eq!(result["choices"][0]["finish_reason"], "tool_calls");
    response.assert_async().await;
}

#[tokio::test]
async fn streaming_emits_reasoning_tools_and_terminal_usage() {
    let mut server = Server::new_async().await;
    let (_dir, auth) = auth_fixture(token_record(false));
    let response = server.mock("POST", "/responses").with_body(sse(vec![
        json!({"type": "response.reasoning_summary_text.delta", "delta": "Thinking"}),
        json!({"type": "response.output_text.delta", "delta": "Hello"}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "call_id": "call_1", "name": "weather"}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{}"}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{}"}}),
        terminal(json!([]))
    ])).create_async().await;
    let router = Router::new().register(
        "chatgpt",
        Box::new(ChatGpt::new(auth).with_base_url(server.url())),
    );
    let chunks: Vec<_> = llmshim::stream(&router, &request())
        .await
        .unwrap()
        .collect()
        .await;
    let chunks: Vec<Value> = chunks
        .into_iter()
        .map(|c| serde_json::from_str(&c.unwrap()).unwrap())
        .collect();
    assert_eq!(chunks.len(), 3);
    assert!(chunks
        .iter()
        .all(|chunk| chunk["model"] == "chatgpt/gpt-6-astra"));
    assert_eq!(
        chunks[0]["choices"][0]["delta"]["reasoning"][0]["text"],
        "Thinking"
    );
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "Hello");
    assert_eq!(
        chunks[2]["choices"][0]["delta"]["tool_calls"][0]["wire_ids"][0]["id"],
        "call_1"
    );
    assert_eq!(
        chunks[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{}"
    );
    assert_eq!(chunks[2]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chunks[2]["usage"]["total_tokens"], 14);
    response.assert_async().await;
}

#[tokio::test]
async fn failed_malformed_and_truncated_streams_are_errors_on_both_surfaces() {
    for body in [
        sse(vec![
            json!({"type": "response.failed", "response": {"error": {"message": "private"}}}),
        ]),
        sse(vec![json!({"type": "error", "message": "private"})]),
        sse(vec![
            json!({"type": "response.completed", "response": {"output": []}}),
        ]),
        "data: {broken\n\n".to_string(),
        "data: [DONE]\n\n".to_string(),
        sse(vec![
            json!({"type": "response.output_text.delta", "delta": "partial"}),
        ]),
    ] {
        let mut server = Server::new_async().await;
        let (_dir, auth) = auth_fixture(token_record(false));
        let response = server
            .mock("POST", "/responses")
            .with_body(body)
            .expect(2)
            .create_async()
            .await;
        let router = Router::new().register(
            "chatgpt",
            Box::new(ChatGpt::new(auth).with_base_url(server.url())),
        );
        let error = llmshim::completion(&router, &request()).await.unwrap_err();
        assert!(!error.to_string().contains("private"));
        let chunks: Vec<_> = llmshim::stream(&router, &request())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(chunks.last().unwrap().is_err());
        response.assert_async().await;
    }
}

#[tokio::test]
async fn incomplete_response_is_explicit_on_both_surfaces() {
    let mut server = Server::new_async().await;
    let (_dir, auth) = auth_fixture(token_record(false));
    let response = server.mock("POST", "/responses").with_body(sse(vec![json!({"type": "response.incomplete", "response": {
        "status": "incomplete", "output": [message("Partial")], "incomplete_details": {"reason": "max_output_tokens"}
    }})])).expect(2).create_async().await;
    let router = Router::new().register(
        "chatgpt",
        Box::new(ChatGpt::new(auth).with_base_url(server.url())),
    );
    assert_eq!(
        llmshim::completion(&router, &request()).await.unwrap()["choices"][0]["finish_reason"],
        "length"
    );
    let chunks: Vec<_> = llmshim::stream(&router, &request())
        .await
        .unwrap()
        .collect()
        .await;
    let last: Value = serde_json::from_str(chunks.last().unwrap().as_ref().unwrap()).unwrap();
    assert_eq!(last["choices"][0]["finish_reason"], "length");
    response.assert_async().await;
}

#[test]
fn cli_discovers_saved_login_and_lists_subscription_models() {
    let (dir, _) = auth_fixture(token_record(false));
    for (args, expected) in [
        (
            vec!["login", "chatgpt", "--status"],
            "ChatGPT login is ready",
        ),
        (vec!["models"], "chatgpt/gpt-6-astra"),
        (vec!["models"], "chatgpt/gpt-5.6-sol"),
        (vec!["models"], "chatgpt/gpt-5.6-terra"),
        (vec!["models"], "chatgpt/gpt-5.6-luna"),
    ] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
            .args(args)
            .env("CHATGPT_TOKEN_DIR", dir.path())
            .env("CHATGPT_AUTH_FILE", "auth.json")
            .output()
            .unwrap();
        assert!(result.status.success());
        let output = String::from_utf8_lossy(&result.stdout);
        assert!(output.contains(expected), "{output}");
        assert!(!output.contains("test-access"));
        assert!(!output.contains("chatgpt/gpt-5.4"));
        assert!(!output.contains("chatgpt/gpt-5.3-codex"));
    }
}

#[cfg(unix)]
#[test]
fn default_auth_status_repairs_legacy_modes_without_touching_override_paths() {
    use std::os::unix::fs::PermissionsExt;

    let temporary_home_directory = tempfile::tempdir().unwrap();
    let config_directory_path = temporary_home_directory.path().join(".llmshim");
    let auth_directory_path = config_directory_path.join("chatgpt");
    let auth_file_path = auth_directory_path.join("auth.json");
    std::fs::create_dir_all(&auth_directory_path).unwrap();
    std::fs::write(&auth_file_path, token_record(false).to_string()).unwrap();
    for path in [&config_directory_path, &auth_directory_path] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::set_permissions(&auth_file_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let command_output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["login", "chatgpt", "--status"])
        .env("HOME", temporary_home_directory.path())
        .env_remove("CHATGPT_TOKEN_DIR")
        .env_remove("CHATGPT_AUTH_FILE")
        .output()
        .unwrap();

    assert!(command_output.status.success());
    assert!(String::from_utf8_lossy(&command_output.stdout).contains("ChatGPT login is ready"));
    for path in [&config_directory_path, &auth_directory_path] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(
        std::fs::metadata(auth_file_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let override_directory = tempfile::tempdir().unwrap();
    let override_file = override_directory.path().join("auth.json");
    std::fs::write(&override_file, token_record(false).to_string()).unwrap();
    std::fs::set_permissions(&override_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        ChatGptAuth::new(override_file.clone()).status().unwrap(),
        LoginStatus::Ready
    );
    assert_eq!(
        std::fs::metadata(override_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
}

#[cfg(unix)]
#[test]
fn default_auth_status_rejects_a_symlinked_cache() {
    let temporary_home_directory = tempfile::tempdir().unwrap();
    let auth_directory_path = temporary_home_directory.path().join(".llmshim/chatgpt");
    let target_path = temporary_home_directory.path().join("auth-target.json");
    std::fs::create_dir_all(&auth_directory_path).unwrap();
    let target_contents = token_record(false).to_string();
    std::fs::write(&target_path, &target_contents).unwrap();
    std::os::unix::fs::symlink(&target_path, auth_directory_path.join("auth.json")).unwrap();

    let command_output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .args(["login", "chatgpt", "--status"])
        .env("HOME", temporary_home_directory.path())
        .env_remove("CHATGPT_TOKEN_DIR")
        .env_remove("CHATGPT_AUTH_FILE")
        .output()
        .unwrap();

    assert!(!command_output.status.success());
    assert!(String::from_utf8_lossy(&command_output.stderr)
        .contains("ChatGPT: cannot read or update the OAuth cache"));
    assert_eq!(
        std::fs::read_to_string(target_path).unwrap(),
        target_contents
    );
}

#[tokio::test]
async fn sse_handles_multiline_events_and_split_utf8_bytes() {
    let mut server = Server::new_async().await;
    let (_dir, auth) = auth_fixture(token_record(false));
    let final_event = serde_json::to_string_pretty(&terminal(json!([message("世界")]))).unwrap();
    let body = format!(": keepalive\r\n\r\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"世界\"}}\r\n\r\n{}\r\n",
        final_event.lines().map(|line| format!("data: {line}\r\n")).collect::<String>());
    let response = server
        .mock("POST", "/responses")
        .with_header("content-type", "text/event-stream")
        .with_chunked_body(move |writer| {
            for byte in body.as_bytes() {
                writer.write_all(&[*byte])?;
                writer.flush()?;
            }
            Ok(())
        })
        .expect(2)
        .create_async()
        .await;
    let router = Router::new().register(
        "chatgpt",
        Box::new(ChatGpt::new(auth).with_base_url(server.url())),
    );
    assert_eq!(
        llmshim::completion(&router, &request()).await.unwrap()["choices"][0]["message"]["content"],
        "世界"
    );
    let chunks: Vec<_> = llmshim::stream(&router, &request())
        .await
        .unwrap()
        .collect()
        .await;
    let first: Value = serde_json::from_str(chunks[0].as_ref().unwrap()).unwrap();
    assert_eq!(first["choices"][0]["delta"]["content"], "世界");
    assert!(chunks.iter().all(Result::is_ok));
    response.assert_async().await;
}

#[tokio::test]
async fn malformed_oauth_success_does_not_replace_cache() {
    for body in [
        "{private-invalid-json",
        "{\"access_token\":\"private-token\"}",
    ] {
        let mut server = Server::new_async().await;
        let (_dir, auth) = auth_fixture(token_record(true));
        let before = std::fs::read(auth.auth_path()).unwrap();
        let response = server
            .mock("POST", "/oauth/token")
            .with_body(body)
            .create_async()
            .await;
        let error = ChatGpt::new(auth.clone().with_auth_base(server.url()))
            .prepare_request("gpt-6-astra", &request())
            .await
            .err()
            .unwrap();
        assert!(!error.to_string().contains("private"));
        assert_eq!(std::fs::read(auth.auth_path()).unwrap(), before);
        response.assert_async().await;
    }
}

#[test]
fn explicit_subscription_routing_does_not_change_bare_gpt_models() {
    let aliases = std::collections::HashMap::new();
    assert_eq!(
        llmshim::router::parse_model("chatgpt/gpt-5.6-sol", &aliases).unwrap(),
        ("chatgpt".into(), "gpt-5.6-sol".into())
    );
    assert_eq!(
        llmshim::router::parse_model("gpt-6-astra", &aliases)
            .unwrap()
            .0,
        "openai"
    );
}

#[cfg(feature = "proxy")]
#[tokio::test]
async fn proxy_stream_preserves_complete_function_arguments() {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    let mut server = Server::new_async().await;
    let (_dir, auth) = auth_fixture(token_record(false));
    let body = sse(vec![
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "call_id": "a", "name": "weather", "arguments": ""}}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "call_id": "b", "name": "weather", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"city\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"city\":\"Rome\"}"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "\"Paris\"}"}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "function_call", "call_id": "b", "name": "weather", "arguments": "{\"city\":\"Rome\"}"}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "call_id": "a", "name": "weather", "arguments": "{\"city\":\"Paris\"}"}}),
        terminal(json!([])),
    ]);
    let response = server
        .mock("POST", "/responses")
        .with_body(body)
        .create_async()
        .await;
    let router = Router::new().register(
        "chatgpt",
        Box::new(ChatGpt::new(auth).with_base_url(server.url())),
    );
    let app = llmshim::proxy::app(router, None);
    let result = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/stream")
                .header("content-type", "application/json")
                .body(Body::from(request().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result.status(), 200);
    let body = to_bytes(result.into_body(), 100_000).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    let events: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|line| serde_json::from_str(line.trim()).unwrap())
        .collect();
    let calls: Vec<_> = events.iter().filter(|e| e["type"] == "tool_call").collect();
    assert_eq!(calls.len(), 2, "{events:?}");
    for (id, city) in [("a", "Paris"), ("b", "Rome")] {
        let call = calls.iter().find(|c| c["wire_ids"][0]["id"] == id).unwrap();
        assert_eq!(call["name"], "weather");
        let args: Value = serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["city"], city);
    }
    assert_eq!(events.last().unwrap()["type"], "done");
    response.assert_async().await;
}

#[cfg(feature = "proxy")]
#[tokio::test]
async fn proxy_chat_and_models_work_with_oauth_provider() {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    let mut server = Server::new_async().await;
    let (_dir, auth) = auth_fixture(token_record(false));
    let response = server
        .mock("POST", "/responses")
        .with_body(sse(vec![terminal(json!([message("Hello")]))]))
        .create_async()
        .await;
    let router = Router::new()
        .register(
            "openai",
            Box::new(llmshim::providers::openai::OpenAi::new("unused".into())),
        )
        .register(
            "chatgpt",
            Box::new(ChatGpt::new(auth).with_base_url(server.url())),
        );
    let app = llmshim::proxy::app(router, None);
    let result = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat")
                .header("content-type", "application/json")
                .body(Body::from(request().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(result.status(), 200);
    let result: Value =
        serde_json::from_slice(&to_bytes(result.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(result["provider"], "chatgpt");
    assert_eq!(result["model"], "chatgpt/gpt-6-astra");
    assert_eq!(result["message"]["content"], "Hello");
    let models = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let models: Value =
        serde_json::from_slice(&to_bytes(models.into_body(), 100_000).await.unwrap()).unwrap();
    assert!(models["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "chatgpt/gpt-6-astra"));
    response.assert_async().await;
}

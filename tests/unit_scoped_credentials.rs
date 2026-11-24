//! Supplied credentials stay on their supplied routes and bypass host proxies.
use llmshim::{
    client::{AttemptDeadlines, ShimClient},
    credentials::StoredCredentials,
    embeddings::EmbeddingRequest,
    error::ShimError,
    fallback::FallbackConfig,
    router::Router,
};
use serde_json::json;

struct Saved {
    key: String,
    base: String,
}

impl StoredCredentials for Saved {
    fn secret_for(&self, provider: &str) -> Option<String> {
        (provider == "openai").then(|| self.key.clone())
    }

    fn base_url_for(&self, provider: &str) -> Option<String> {
        (provider == "openai").then(|| self.base.clone())
    }
}

// This binary has one test so environment changes cannot race another test.
#[tokio::test]
async fn supplied_routes_and_transports_ignore_host_configuration() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".llmshim");
    std::fs::create_dir_all(config.join("chatgpt")).unwrap();
    std::fs::write(config.join("chatgpt/auth.json"), "{}").unwrap();
    std::fs::write(
        config.join("config.toml"),
        "[keys]\nanthropic = 'host-key'\n[routes.host]\nmodel = 'openai/gpt-6-sol'\n",
    )
    .unwrap();
    std::env::set_var("HOME", home.path());
    std::env::set_var("ANTHROPIC_API_KEY", "environment-key");
    std::env::set_var("OPENAI_API_KEY", "ambient-openai-key");
    std::env::set_var("VLLM_BASE_URL", "http://host.invalid/v1");
    std::env::set_var("LLMSHIM_MAX_RETRIES", "0");
    let mut upstream = mockito::Server::new_async().await;
    let mut proxy = mockito::Server::new_async().await;
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
    ] {
        std::env::set_var(name, proxy.url());
    }
    for name in ["NO_PROXY", "no_proxy"] {
        std::env::remove_var(name);
    }
    let proxy_request = proxy
        .mock("POST", mockito::Matcher::Any)
        .with_status(502)
        .expect(1)
        .create_async()
        .await;
    let saved = Saved {
        key: "supplied-key".into(),
        base: upstream.url(),
    };
    let scoped = Router::from_scoped_credentials(&saved);
    assert!(
        Router::from_scoped_credentials(&llmshim::credentials::EnvironmentOnly)
            .provider_keys()
            .is_empty()
    );
    assert!(Router::from_scoped_credentials(&Saved {
        key: saved.key.clone(),
        base: "https://user:password@host.example/v1".into(),
    })
    .provider_keys()
    .is_empty());
    assert_eq!(scoped.provider_keys(), vec!["openai"]);
    assert!(scoped.route_names().is_empty());
    let host = Router::from_credentials_with_env(&saved, &|name| std::env::var(name).ok());
    for provider in ["anthropic", "chatgpt", "vllm"] {
        assert!(host.get(provider).is_ok());
        assert!(scoped.get(provider).is_err());
    }
    assert_eq!(host.route_names(), vec!["host"]);
    let embeddings = upstream
        .mock("POST", "/embeddings")
        .match_header("authorization", "Bearer supplied-key")
        .with_body(json!({"data": [{"index": 0, "embedding": [0.5, 1.0]}]}).to_string())
        .expect(3)
        .create_async()
        .await;
    let request = EmbeddingRequest::new("openai/text-embedding-3-small", vec!["hello".into()]);
    assert_eq!(
        llmshim::embeddings(&scoped, &request)
            .await
            .unwrap()
            .vectors,
        vec![vec![0.5, 1.0]]
    );
    let client = ShimClient::new_scoped()
        .unwrap()
        .with_attempt_deadlines(AttemptDeadlines::default())
        .unwrap();
    assert_eq!(
        llmshim::embeddings_with_client(&scoped, &request, &client)
            .await
            .unwrap()
            .vectors,
        vec![vec![0.5, 1.0]]
    );
    let client = ShimClient::new()
        .without_ambient_proxy()
        .unwrap()
        .with_attempt_deadlines(AttemptDeadlines::default())
        .unwrap();
    llmshim::embeddings_with_client(&scoped, &request, &client)
        .await
        .unwrap();
    embeddings.assert_async().await;
    let ambient = ShimClient::new();
    let (provider, model) = scoped.resolve(&request.model).unwrap();
    assert!(ambient
        .embeddings(provider, &model, &json!({"input": ["hello"]}))
        .await
        .is_err());
    proxy_request.assert_async().await;

    let completion = upstream
        .mock("POST", "/responses")
        .match_header("authorization", "Bearer supplied-key")
        .with_body(
            json!({"id": "reply", "status": "completed", "output": [], "usage": {}}).to_string(),
        )
        .expect(2)
        .create_async()
        .await;
    let request = json!({"model": "openai/gpt-6-sol", "messages": []});
    assert_eq!(
        llmshim::completion(&scoped, &request).await.unwrap()["id"],
        "reply"
    );
    assert_eq!(
        llmshim::completion_with_fallback(
            &scoped,
            &request,
            &FallbackConfig::default().max_retries(0),
            None,
        )
        .await
        .unwrap()["id"],
        "reply"
    );
    completion.assert_async().await;

    let invalid = Router::from_scoped_credentials(&Saved {
        key: "invalid\ncredential".into(),
        base: upstream.url(),
    });
    let request = json!({"model": "openai/gpt-6-sol", "messages": []});
    assert!(matches!(
        llmshim::completion(&invalid, &request).await,
        Err(ShimError::Http(_))
    ));
    let error = llmshim::completion_with_fallback(
        &invalid,
        &request,
        &FallbackConfig::default().max_retries(0),
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ShimError::AllFailed(_)));
    assert!(!error.to_string().contains("credential"));
}

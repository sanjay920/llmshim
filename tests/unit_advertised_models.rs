use llmshim::models::{available_models, spec, MODELS};

const EXPECTED: &[&str] = &[
    "openai/gpt-6-astra",
    "openai/gpt-5.6-sol",
    "openai/gpt-5.6-terra",
    "openai/gpt-5.6-luna",
    "anthropic/claude-fable-5-1",
    "anthropic/claude-opus-5",
    "anthropic/claude-sonnet-5",
    "anthropic/claude-haiku-4-5-20251001",
    "gemini/gemini-3.8-flash",
    "gemini/gemini-3.5-flash-lite",
    "xai/grok-4.6",
    "chatgpt/gpt-6-astra",
    "chatgpt/gpt-5.6-sol",
    "chatgpt/gpt-5.6-terra",
    "chatgpt/gpt-5.6-luna",
];

#[test]
fn catalog_advertises_only_the_selected_current_tiers() {
    assert_eq!(
        MODELS.iter().map(|model| model.id).collect::<Vec<_>>(),
        EXPECTED
    );
    assert_eq!(
        available_models(&["gemini"])
            .iter()
            .map(|model| model.id)
            .collect::<Vec<_>>(),
        ["gemini/gemini-3.8-flash", "gemini/gemini-3.5-flash-lite"]
    );
    assert_eq!(spec("gpt-6-astra").unwrap().id, "openai/gpt-6-astra");
    assert!(available_models(&["openrouter", "vllm", "sglang"]).is_empty());
}

#[test]
fn cli_discovery_uses_the_same_curated_catalog() {
    let dir = tempfile::tempdir().unwrap();
    // Discovery only checks that the cache exists; it never authenticates.
    std::fs::write(dir.path().join("auth.json"), "{}").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
        .arg("models")
        .env("OPENAI_API_KEY", "test")
        .env("ANTHROPIC_API_KEY", "test")
        .env("GEMINI_API_KEY", "test")
        .env("XAI_API_KEY", "test")
        .env("CHATGPT_TOKEN_DIR", dir.path())
        .env("CHATGPT_AUTH_FILE", "auth.json")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let ids: Vec<_> = stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(ids, EXPECTED);
}

#[cfg(feature = "proxy")]
#[tokio::test]
async fn proxy_discovery_excludes_legacy_and_preview_models() {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use llmshim::{
        providers::{
            anthropic::Anthropic, chatgpt::ChatGpt, gemini::Gemini, openai::OpenAi, xai::Xai,
        },
        router::Router,
    };
    use tower::ServiceExt;
    let router = Router::new()
        .register("openai", Box::new(OpenAi::new("test".into())))
        .register("anthropic", Box::new(Anthropic::new("test".into())))
        .register("gemini", Box::new(Gemini::new("test".into())))
        .register("xai", Box::new(Xai::new("test".into())))
        .register("chatgpt", Box::new(ChatGpt::default()));
    // Pruning discovery must not block explicit legacy requests.
    assert_eq!(router.resolve("openai/gpt-5.4").unwrap().1, "gpt-5.4");
    assert_eq!(
        router.resolve("anthropic/claude-fable-5").unwrap().1,
        "claude-fable-5"
    );
    let response = llmshim::proxy::app(router, None)
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let json: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    let ids: Vec<_> = json["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, EXPECTED);
}

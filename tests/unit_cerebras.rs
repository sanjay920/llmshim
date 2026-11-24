use llmshim::{
    credentials::StoredCredentials, provider::Provider, providers::cerebras::Cerebras,
    router::Router,
};
use serde_json::{json, Value};

const MODEL: &str = "qwen-3.8-27b";

#[test]
fn request_uses_cerebras_auth_namespace_and_chat_wire() {
    let provider = Cerebras::new("cerebras-test-key".into());
    let req = provider
        .transform_request(
            MODEL,
            &json!({
                "messages":[{"role":"user","content":"hello"}],
                "max_tokens":100, "reasoning_effort":"none", "stream":true,
                "stream_options":{"include_usage":true},
                "x-cerebras":{"clear_thinking":true,"reasoning_format":"parsed"},
                "x-openai":{"store":true}, "x-openrouter":{"models":["foreign"]}
            }),
        )
        .unwrap();
    assert_eq!(req.url, "https://api.cerebras.ai/v1/chat/completions");
    assert!(req
        .headers
        .contains(&("Authorization".into(), "Bearer cerebras-test-key".into())));
    assert_eq!(req.body["model"], MODEL);
    assert_eq!(req.body["reasoning_effort"], "none");
    assert_eq!(req.body["clear_thinking"], true);
    assert_eq!(req.body["stream_options"]["include_usage"], true);
    assert!(req.body.get("store").is_none());
    assert!(req.body.get("models").is_none());
    assert_eq!(
        provider.request_admission_policy().native_namespace(),
        Some("x-cerebras")
    );
    assert!(provider
        .transform_embedding_request(MODEL, &json!({"input":"hello"}))
        .is_err());
}

#[test]
fn reasoning_and_usage_survive_unary_stream_and_replay() {
    let provider = Cerebras::new("test".into());
    let response = provider.transform_response(MODEL, json!({
        "choices":[{"message":{"role":"assistant","content":"4","reasoning":"two plus two"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20}
    })).unwrap();
    let message = &response["choices"][0]["message"];
    assert_eq!(message["reasoning"][0]["text"], "two plus two");
    assert_eq!(message["reasoning"][0]["origin"]["provider"], "cerebras");
    let request = provider
        .transform_request(
            MODEL,
            &json!({"messages":[
                {"role":"user","content":"2+2?"},message,{"role":"user","content":"again"}
            ]}),
        )
        .unwrap();
    assert_eq!(request.body["messages"][1]["reasoning"], "two plus two");
    assert_eq!(response["usage"]["cache_read_tokens"], 0);
    let chunk = provider
        .transform_stream_chunk(
            MODEL,
            &json!({
                "choices":[{"delta":{"reasoning":"think"},"index":0}]
            })
            .to_string(),
        )
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&chunk).unwrap();
    assert_eq!(
        parsed["choices"][0]["delta"]["reasoning"][0]["text"],
        "think"
    );
    assert_eq!(
        provider.transform_stream_chunk(MODEL, "[DONE]").unwrap(),
        None
    );
    assert!(provider
        .transform_response(MODEL, json!({"error":{"message":"bad key","code":401}}))
        .is_err());
}

struct Saved;
impl StoredCredentials for Saved {
    fn secret_for(&self, provider: &str) -> Option<String> {
        (provider == "cerebras").then(|| "stored-key".into())
    }
    fn base_url_for(&self, _: &str) -> Option<String> {
        Some("http://localhost:8000/v1/".into())
    }
}

#[test]
fn routing_supports_stored_keys_custom_addresses_and_env_precedence() {
    let router = Router::from_credentials_with_env(&Saved, &|_| None);
    let (provider, model) = router.resolve("cerebras/qwen-3.8-27b").unwrap();
    let req = provider
        .transform_request(&model, &json!({"messages":[]}))
        .unwrap();
    assert_eq!(req.url, "http://localhost:8000/v1/chat/completions");
    assert!(req
        .headers
        .contains(&("Authorization".into(), "Bearer stored-key".into())));
    let router = Router::from_credentials_with_env(&Saved, &|name| {
        (name == "CEREBRAS_API_KEY").then(|| "env-key".into())
    });
    let (provider, model) = router.resolve(MODEL).unwrap();
    assert_eq!(provider.name(), "cerebras");
    let req = provider
        .transform_request(&model, &json!({"messages":[]}))
        .unwrap();
    assert_eq!(req.url, "https://api.cerebras.ai/v1/chat/completions");
    assert!(req
        .headers
        .contains(&("Authorization".into(), "Bearer env-key".into())));
    // Dedicated endpoint IDs use the explicit prefix without name inference.
    assert_eq!(
        router.resolve("cerebras/my-endpoint").unwrap().1,
        "my-endpoint"
    );
    let cfg: llmshim::config::Config =
        toml::from_str("[keys]\ncerebras = 'saved-test-key'").unwrap();
    assert_eq!(cfg.keys.cerebras.as_deref(), Some("saved-test-key"));
}

#[test]
fn cli_saves_masks_and_loads_the_cerebras_key() {
    let home = tempfile::tempdir().unwrap();
    let command = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_llmshim"))
            .args(args)
            .env("HOME", home.path())
            .env("LLMSHIM_CATALOG_OFFLINE", "1")
            .output()
            .unwrap()
    };
    let saved = command(&["set", "cerebras", "synthetic-cerebras-secret"]);
    assert!(saved.status.success());
    assert!(!String::from_utf8_lossy(&saved.stdout).contains("synthetic-cerebras-secret"));
    let got = command(&["get", "cerebras"]);
    assert!(got.status.success());
    let displayed = String::from_utf8_lossy(&got.stdout);
    assert_eq!(displayed.trim(), "synt...cret");
    assert!(!displayed.contains("synthetic-cerebras-secret"));
    let cfg: llmshim::config::Config =
        toml::from_str(&std::fs::read_to_string(home.path().join(".llmshim/config.toml")).unwrap())
            .unwrap();
    assert_eq!(
        cfg.keys.cerebras.as_deref(),
        Some("synthetic-cerebras-secret")
    );
}

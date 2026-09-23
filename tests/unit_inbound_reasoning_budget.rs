use llmshim::provider::Provider;
use llmshim::providers::{
    anthropic::Anthropic,
    chatgpt::{ChatGpt, ChatGptAuth},
    gemini::Gemini,
    openai::OpenAi,
    openai_compat::OpenAiCompatible,
    openrouter::OpenRouter,
    xai::Xai,
};
use llmshim::reasoning::{filter_reasoning_for_target, WireFormat};
use serde_json::{json, Value};

const LIMIT_ERROR: &str = "request reasoning metadata exceeds derived size limit";

fn legacy_request(model_bytes: usize, block_count: usize) -> Value {
    json!({
        "model": "local/gpt-5.6-luna",
        "messages": [{
            "role": "assistant",
            "content": "answer",
            "reasoning_origin": {
                "provider": "source",
                "model": "m".repeat(model_bytes),
                "family": "claude",
                "wire": "openai-chat",
                "received_at": "2026-09-22T00:00:00Z"
            },
            "reasoning_details": (0..block_count)
                .map(|index| json!({"type":"reasoning.text","text":"x","index":index}))
                .collect::<Vec<_>>()
        }]
    })
}

fn assert_limit(error: llmshim::error::ShimError) {
    match error {
        llmshim::error::ShimError::ProviderError { status, body, .. } => {
            assert_eq!(status, 400);
            assert_eq!(body, LIMIT_ERROR);
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn every_synchronous_provider_route_refuses_before_transforming() {
    let request = legacy_request(64 * 1024, 64);
    assert!(request.to_string().len() < 80 * 1024);
    let original = request.clone();
    let token_directory = tempfile::tempdir().unwrap();
    let providers: Vec<(Box<dyn Provider>, &str)> = vec![
        (Box::new(OpenAi::new("key".into())), "gpt-5.6-luna"),
        (Box::new(Anthropic::new("key".into())), "claude-sonnet-5"),
        (Box::new(Gemini::new("key".into())), "gemini-3.8-flash"),
        (Box::new(Xai::new("key".into())), "grok-4.7"),
        (
            Box::new(OpenRouter::new("key".into())),
            "anthropic/claude-sonnet-5",
        ),
        (
            Box::new(OpenAiCompatible::new("vllm", "http://127.0.0.1:1/v1", None)),
            "gpt-5.6-luna",
        ),
        (
            Box::new(
                OpenAiCompatible::new("sglang", "http://127.0.0.1:1/v1", None)
                    .with_wire(WireFormat::OpenAiResponses),
            ),
            "gpt-5.6-luna",
        ),
        (
            Box::new(ChatGpt::new(ChatGptAuth::new(
                token_directory.path().join("missing.json"),
            ))),
            "gpt-5.6-luna",
        ),
    ];
    for (provider, model) in providers {
        let error = match provider.transform_request(model, &request) {
            Ok(_) => panic!("provider accepted excessive reasoning metadata"),
            Err(error) => error,
        };
        assert_limit(error);
        assert_eq!(request, original);
    }
}

#[tokio::test]
async fn chatgpt_async_preparation_refuses_before_reading_credentials() {
    let token_directory = tempfile::tempdir().unwrap();
    let provider = ChatGpt::new(ChatGptAuth::new(
        token_directory.path().join("missing.json"),
    ));
    let request = legacy_request(64 * 1024, 64);
    let error = match provider.prepare_request("gpt-5.6-luna", &request).await {
        Ok(_) => panic!("ChatGPT accepted excessive reasoning metadata"),
        Err(error) => error,
    };
    assert_limit(error);
}

#[test]
fn accepted_legacy_blocks_preserve_exact_source_and_payload() {
    let provider = OpenRouter::new("key".into());
    let model = "anthropic/claude-sonnet-4.6";
    let origin = provider.replay_target(model).origin();
    let details = json!([
        {"type":"reasoning.text","text":"first","signature":"sig","format":"anthropic-claude-v1","index":0},
        {"type":"reasoning.encrypted","data":"opaque","id":"rs_a","index":1}
    ]);
    let request = json!({
        "model": format!("openrouter/{model}"),
        "messages": [{
            "role":"assistant",
            "content":"answer",
            "reasoning_origin":origin,
            "reasoning_details":details
        }]
    });
    let original = request.clone();
    let prepared = provider.transform_request(model, &request).unwrap();
    assert_eq!(prepared.body["messages"][0]["reasoning_details"], details);
    assert_eq!(request, original);
}

#[test]
fn malformed_opaque_rows_drop_before_origin_clone_and_preserve_later_fallback() {
    let provider = OpenRouter::new("key".into());
    let model = "anthropic/claude-sonnet-4.6";
    let origin = provider.replay_target(model).origin();
    let thinking = json!([{
        "type":"thinking",
        "thinking":"kept",
        "signature":"sig"
    }]);
    let request = json!({
        "model":format!("openrouter/{model}"),
        "messages":[{
            "role":"assistant",
            "content":"answer",
            "reasoning_origin":origin,
            "reasoning_details":[
                {"type":"redacted_thinking"},
                {"type":"redacted_thinking","data":null},
                {"type":"reasoning.encrypted","data":7},
                {"type":"reasoning.encrypted","data":[]}
            ],
            "thinking_blocks":thinking
        }]
    });
    let original = request.clone();
    let prepared = provider.transform_request(model, &request).unwrap();
    assert!(prepared.body["messages"][0]
        .get("reasoning_details")
        .is_none());
    assert_eq!(prepared.body["messages"][0]["thinking_blocks"], thinking);
    assert_eq!(request, original);
}

#[test]
fn public_filter_is_fallible_atomic_and_keeps_small_mismatch_behavior() {
    let provider = OpenAi::new("key".into());
    let target = provider.replay_target("gpt-5.6-luna");
    let request = legacy_request(64 * 1024, 64);
    let mut excessive = request["messages"][0].clone();
    let original = excessive.clone();
    assert_limit(filter_reasoning_for_target(&mut excessive, &target).unwrap_err());
    assert_eq!(excessive, original);

    let mut mismatched = legacy_request(32, 2)["messages"][0].clone();
    filter_reasoning_for_target(&mut mismatched, &target).unwrap();
    assert!(mismatched.get("reasoning").is_none());
    assert!(mismatched.get("reasoning_origin").is_none());
    assert!(mismatched.get("reasoning_details").is_none());
}

#[cfg(feature = "proxy")]
#[tokio::test]
async fn typed_chat_rejects_unary_and_stream_before_upstream_dispatch() {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use llmshim::router::Router;
    use tower::ServiceExt;

    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", mockito::Matcher::Any)
        .expect(0)
        .create_async()
        .await;
    for stream in [false, true] {
        let router = Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        );
        let mut request = legacy_request(64 * 1024, 64);
        request["stream"] = json!(stream);
        let original = request.clone();
        let response = llmshim::proxy::app(router, None)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert!(String::from_utf8(body.to_vec())
            .unwrap()
            .contains(LIMIT_ERROR));
        assert_eq!(request, original);
    }
    upstream.assert_async().await;
}

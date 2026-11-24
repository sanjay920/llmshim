//! Embeddings: one vector per text, through each provider's own wire.
//!
//! Every test here goes through `llmshim::embeddings` against a loopback stub,
//! because the contract is the round trip: what leaves as a batch of texts must
//! come back as vectors that pair with them one-to-one, or be refused before
//! anything is sent.
use llmshim::embeddings::EmbeddingRequest;
use llmshim::error::ShimError;
use llmshim::providers::{
    anthropic::Anthropic, gemini::Gemini, openai::OpenAi, openai_compat::OpenAiCompatible,
};
use llmshim::router::Router;
use serde_json::{json, Value};

/// A client with retries off, so a stub that answers with a retryable status
/// fails the test instead of sleeping the backoff ladder.
fn no_retries() {
    std::env::set_var("LLMSHIM_MAX_RETRIES", "0");
}

fn router(provider: Box<dyn llmshim::provider::Provider>, key: &str) -> Router {
    no_retries();
    Router::new().register(key, provider)
}

fn openai_router(server: &mockito::ServerGuard) -> Router {
    router(
        Box::new(OpenAi::new("test-key".into()).with_base_url(server.url())),
        "openai",
    )
}

fn vllm_router(server: &mockito::ServerGuard, key: Option<&str>) -> Router {
    router(
        Box::new(OpenAiCompatible::new(
            "vllm",
            server.url(),
            key.map(str::to_owned),
        )),
        "vllm",
    )
}

fn texts(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_string()).collect()
}

/// An OpenAI embeddings body. `indexed` decides whether the vectors carry the
/// `index` OpenAI sends, which is what a response out of order is sorted by.
fn openai_body(mut vectors: Vec<(u64, Vec<f64>)>, prompt_tokens: u64) -> String {
    let data: Vec<Value> = vectors
        .drain(..)
        .map(|(index, embedding)| json!({"object":"embedding","index":index,"embedding":embedding}))
        .collect();
    json!({
        "object": "list",
        "model": "text-embedding-3-small",
        "data": data,
        "usage": {"prompt_tokens": prompt_tokens, "total_tokens": prompt_tokens},
    })
    .to_string()
}

fn indexed(vectors: &[f64]) -> Vec<(u64, Vec<f64>)> {
    vectors
        .iter()
        .enumerate()
        .map(|(index, value)| (index as u64, vec![*value]))
        .collect()
}

async fn provider_error(error: ShimError) -> (u16, String) {
    match error {
        ShimError::ProviderError { status, body, .. } => (status, body),
        other => panic!("expected a provider error, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_returns_one_vector_per_text_in_index_order_with_the_catalogs_price() {
    let mut server = mockito::Server::new_async().await;
    let stub = server
        .mock("POST", "/embeddings")
        .match_header("authorization", "Bearer test-key")
        .match_body(mockito::Matcher::PartialJson(json!({
            "model": "text-embedding-3-small",
            "input": ["first", "second"],
            "encoding_format": "float",
        })))
        .with_status(200)
        .with_body(openai_body(
            vec![(1, vec![3.0, 4.0]), (0, vec![1.0, 2.0])],
            1_000_000,
        ))
        .expect(1)
        .create_async()
        .await;

    let embeddings = llmshim::embeddings(
        &openai_router(&server),
        &EmbeddingRequest::new("openai/text-embedding-3-small", texts(&["first", "second"])),
    )
    .await
    .unwrap();

    stub.assert_async().await;
    assert_eq!(embeddings.provider, "openai");
    assert_eq!(embeddings.model, "text-embedding-3-small");
    assert_eq!(
        embeddings.revision.as_deref(),
        Some("text-embedding-3-small"),
        "the model the provider's own body named is the revision"
    );
    assert_eq!(
        embeddings.vectors,
        vec![vec![1.0, 2.0], vec![3.0, 4.0]],
        "vectors pair with the texts by index, not by arrival order"
    );
    assert_eq!(embeddings.usage.input_tokens, Some(1_000_000));
    // text-embedding-3-small is 0.02 USD per million input tokens.
    let cost = embeddings.usage.cost_usd.expect("the catalog prices it");
    assert!((cost - 0.02).abs() < 1e-9, "cost was {cost}");
    assert_eq!(embeddings.usage.cost_source.as_deref(), Some("catalog"));
}

#[tokio::test]
async fn gemini_batch_embed_contents_takes_one_request_object_per_text() {
    let mut server = mockito::Server::new_async().await;
    let stub = server
        .mock("POST", "/models/gemini-embedding-001:batchEmbedContents")
        .match_query(mockito::Matcher::UrlEncoded(
            "key".into(),
            "test-key".into(),
        ))
        .match_body(mockito::Matcher::PartialJson(json!({
            "requests": [
                {
                    "model": "models/gemini-embedding-001",
                    "content": {"parts": [{"text": "first"}]},
                    "outputDimensionality": 256,
                },
                {
                    "model": "models/gemini-embedding-001",
                    "content": {"parts": [{"text": "second"}]},
                    "outputDimensionality": 256,
                },
            ]
        })))
        .with_status(200)
        .with_body(
            json!({"embeddings": [{"values": [1.0, 2.0]}, {"values": [3.0, 4.0]}]}).to_string(),
        )
        .expect(1)
        .create_async()
        .await;

    let embeddings = llmshim::embeddings(
        &router(
            Box::new(Gemini::new("test-key".into()).with_base_url(server.url())),
            "gemini",
        ),
        &EmbeddingRequest::new("gemini/gemini-embedding-001", texts(&["first", "second"]))
            .with_dimensions(256),
    )
    .await
    .unwrap();

    stub.assert_async().await;
    assert_eq!(embeddings.vectors, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    assert_eq!(
        embeddings.revision, None,
        "batchEmbedContents names no model in its response"
    );
    assert_eq!(
        embeddings.usage.input_tokens, None,
        "batchEmbedContents reports no token counts, so the bill is unknown"
    );
    assert_eq!(embeddings.usage.cost_usd, None);
}

#[tokio::test]
async fn a_gemini_error_body_under_a_200_keeps_its_own_status_and_message() {
    let mut server = mockito::Server::new_async().await;
    let stub = server
        .mock("POST", "/models/gemini-embedding-001:batchEmbedContents")
        .match_query(mockito::Matcher::UrlEncoded(
            "key".into(),
            "test-key".into(),
        ))
        .with_status(200)
        .with_body(
            json!({"error": {"code": 429, "message": "rate limited by upstream"}}).to_string(),
        )
        .expect(1)
        .create_async()
        .await;

    let error = llmshim::embeddings(
        &router(
            Box::new(Gemini::new("test-key".into()).with_base_url(server.url())),
            "gemini",
        ),
        &EmbeddingRequest::new("gemini/gemini-embedding-001", texts(&["text"])),
    )
    .await
    .unwrap_err();

    stub.assert_async().await;
    let (status, body) = provider_error(error).await;
    assert_eq!(
        status, 429,
        "the body's own code is the status, not HTTP 200"
    );
    assert_eq!(body, "rate limited by upstream");
}

#[tokio::test]
async fn an_openai_compatible_server_answers_on_the_same_route() {
    let mut server = mockito::Server::new_async().await;
    let stub = server
        .mock("POST", "/embeddings")
        .match_body(mockito::Matcher::PartialJson(json!({
            "model": "nomic-embed-text",
            "input": ["local text"],
        })))
        .with_status(200)
        .with_body(
            json!({
                "model": "nomic-embed-text-v1.5",
                "data": [{"index": 0, "embedding": [0.5, -0.25]}],
                "usage": {"prompt_tokens": 3, "total_tokens": 3},
            })
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;

    let embeddings = llmshim::embeddings(
        &vllm_router(&server, None),
        &EmbeddingRequest::new("vllm/nomic-embed-text", texts(&["local text"])),
    )
    .await
    .unwrap();

    stub.assert_async().await;
    assert_eq!(embeddings.provider, "vllm");
    assert_eq!(embeddings.vectors, vec![vec![0.5, -0.25]]);
    assert_eq!(
        embeddings.revision.as_deref(),
        Some("nomic-embed-text-v1.5")
    );
    assert_eq!(embeddings.usage.input_tokens, Some(3));
}

#[tokio::test]
async fn a_provider_without_an_embeddings_api_refuses_by_name() {
    no_retries();
    let router = Router::new().register("anthropic", Box::new(Anthropic::new("test-key".into())));
    let error = llmshim::embeddings(
        &router,
        &EmbeddingRequest::new("anthropic/claude-sonnet-5-5", texts(&["text"])),
    )
    .await
    .unwrap_err();

    let (status, body) = provider_error(error).await;
    assert_eq!(status, 400);
    assert_eq!(body, "anthropic has no embeddings API");
}

#[tokio::test]
async fn a_model_the_catalog_does_not_mark_as_embedding_is_refused_without_a_request() {
    let mut server = mockito::Server::new_async().await;
    let never = server
        .mock("POST", "/embeddings")
        .expect(0)
        .create_async()
        .await;

    let error = llmshim::embeddings(
        &openai_router(&server),
        &EmbeddingRequest::new("openai/gpt-6-astra", texts(&["text"])),
    )
    .await
    .unwrap_err();

    never.assert_async().await;
    let (status, body) = provider_error(error).await;
    assert_eq!(status, 400);
    assert!(
        body.contains("openai/gpt-6-astra") && body.contains("embedding = true"),
        "the refusal names the model and the override that would allow it: {body}"
    );
}

#[tokio::test]
async fn a_self_hosted_model_the_catalog_never_heard_of_is_sent() {
    let mut server = mockito::Server::new_async().await;
    let stub = server
        .mock("POST", "/embeddings")
        .with_status(200)
        .with_body(json!({"data": [{"index": 0, "embedding": [1.0]}]}).to_string())
        .expect(1)
        .create_async()
        .await;

    let embeddings = llmshim::embeddings(
        &vllm_router(&server, Some("secret")),
        &EmbeddingRequest::new("vllm/bge-m3", texts(&["text"])),
    )
    .await
    .unwrap();

    stub.assert_async().await;
    assert_eq!(embeddings.vectors, vec![vec![1.0]]);
}

#[tokio::test]
async fn a_width_above_the_models_own_is_refused_and_a_narrower_one_is_sent() {
    let mut server = mockito::Server::new_async().await;
    let stub = server
        .mock("POST", "/embeddings")
        .match_body(mockito::Matcher::PartialJson(json!({"dimensions": 512})))
        .with_status(200)
        .with_body(openai_body(indexed(&[1.0]), 4))
        .expect(1)
        .create_async()
        .await;

    let narrowed = llmshim::embeddings(
        &openai_router(&server),
        &EmbeddingRequest::new("openai/text-embedding-3-small", texts(&["text"]))
            .with_dimensions(512),
    )
    .await
    .unwrap();
    assert_eq!(narrowed.vectors, vec![vec![1.0]]);

    let error = llmshim::embeddings(
        &openai_router(&server),
        &EmbeddingRequest::new("openai/text-embedding-3-small", texts(&["text"]))
            .with_dimensions(3073),
    )
    .await
    .unwrap_err();

    stub.assert_async().await;
    let (status, body) = provider_error(error).await;
    assert_eq!(status, 400);
    assert!(body.contains("3073") && body.contains("1536"), "{body}");
}

#[tokio::test]
async fn a_batch_over_the_providers_text_limit_is_refused_and_one_at_it_is_sent() {
    let mut server = mockito::Server::new_async().await;
    let stub = server
        .mock("POST", "/embeddings")
        .with_status(200)
        .with_body(openai_body(indexed(&vec![1.0; 2048]), 2048))
        .expect(1)
        .create_async()
        .await;

    let at_limit = llmshim::embeddings(
        &openai_router(&server),
        &EmbeddingRequest::new(
            "openai/text-embedding-3-small",
            vec!["text".to_string(); 2048],
        ),
    )
    .await
    .unwrap();
    assert_eq!(at_limit.vectors.len(), 2048);

    let error = llmshim::embeddings(
        &openai_router(&server),
        &EmbeddingRequest::new(
            "openai/text-embedding-3-small",
            vec!["text".to_string(); 2049],
        ),
    )
    .await
    .unwrap_err();

    stub.assert_async().await;
    let (status, body) = provider_error(error).await;
    assert_eq!(status, 400);
    assert!(body.contains("2049") && body.contains("2048"), "{body}");
}

#[tokio::test]
async fn a_batch_over_the_byte_limit_is_refused_and_one_at_it_is_sent() {
    let mut server = mockito::Server::new_async().await;
    let stub = server
        .mock("POST", "/embeddings")
        .with_status(200)
        .with_body(openai_body(indexed(&[1.0]), 4))
        .expect(1)
        .create_async()
        .await;

    let at_limit = llmshim::embeddings(
        &openai_router(&server),
        &EmbeddingRequest::new("openai/text-embedding-3-small", vec!["x".repeat(1 << 20)]),
    )
    .await
    .unwrap();
    assert_eq!(at_limit.vectors, vec![vec![1.0]]);

    let error = llmshim::embeddings(
        &openai_router(&server),
        &EmbeddingRequest::new(
            "openai/text-embedding-3-small",
            vec!["x".repeat((1 << 20) + 1)],
        ),
    )
    .await
    .unwrap_err();

    stub.assert_async().await;
    let (status, body) = provider_error(error).await;
    assert_eq!(status, 400);
    assert!(body.contains("bytes"), "{body}");
}

#[tokio::test]
async fn an_empty_batch_is_refused() {
    let mut server = mockito::Server::new_async().await;
    let never = server
        .mock("POST", "/embeddings")
        .expect(0)
        .create_async()
        .await;

    let error = llmshim::embeddings(
        &openai_router(&server),
        &EmbeddingRequest::new("openai/text-embedding-3-small", Vec::new()),
    )
    .await
    .unwrap_err();

    never.assert_async().await;
    let (status, body) = provider_error(error).await;
    assert_eq!(status, 400);
    assert!(body.contains("at least one text"), "{body}");
}

#[tokio::test]
async fn a_response_with_the_wrong_number_of_vectors_is_refused() {
    let mut server = mockito::Server::new_async().await;
    let stub = server
        .mock("POST", "/embeddings")
        .with_status(200)
        .with_body(openai_body(indexed(&[1.0]), 4))
        .expect(1)
        .create_async()
        .await;

    let error = llmshim::embeddings(
        &openai_router(&server),
        &EmbeddingRequest::new("openai/text-embedding-3-small", texts(&["one", "two"])),
    )
    .await
    .unwrap_err();

    stub.assert_async().await;
    let (status, body) = provider_error(error).await;
    assert_eq!(status, 502);
    assert!(body.contains("1 vectors for 2 texts"), "{body}");
}

#[tokio::test]
async fn an_upstream_refusal_keeps_its_status_and_body() {
    let mut server = mockito::Server::new_async().await;
    let stub = server
        .mock("POST", "/embeddings")
        .with_status(401)
        .with_body("invalid api key")
        .expect(1)
        .create_async()
        .await;

    let error = llmshim::embeddings(
        &openai_router(&server),
        &EmbeddingRequest::new("openai/text-embedding-3-small", texts(&["text"])),
    )
    .await
    .unwrap_err();

    stub.assert_async().await;
    let (status, body) = provider_error(error).await;
    assert_eq!(status, 401);
    assert_eq!(body, "invalid api key");
}

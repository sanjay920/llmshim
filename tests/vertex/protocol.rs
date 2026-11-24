use super::*;
use futures::StreamExt;

#[test]
fn regional_global_and_anthropic_paths_keep_identity_out_of_prepared_requests() {
    for (region, host) in [
        ("us-central1", "us-central1-aiplatform.googleapis.com"),
        ("global", "aiplatform.googleapis.com"),
    ] {
        for stream in [false, true] {
            let request = json!({"messages":[{"role":"user","content":"Hello"}],"stream":stream});
            let gemini = Vertex::gemini(identity(region));
            let prepared = gemini
                .transform_request("gemini-2.5-pro", &request)
                .unwrap();
            let operation = if stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            };
            assert_eq!(
                prepared.url,
                format!(
                    "https://{host}/v1/projects/fixture-project/locations/\
                {region}/publishers/google/models/gemini-2.5-pro:{operation}"
                )
            );
            let expected: Value =
                serde_json::from_str(include_str!("../fixtures/vertex/gemini-request.json"))
                    .unwrap();
            assert_eq!(prepared.body, expected);
            assert!(!prepared.url.contains("key="));
            assert!(!prepared
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("authorization")));
            let anthropic = Vertex::anthropic(identity(region));
            let prepared = anthropic
                .transform_request("claude-sonnet-4-5@20250929", &request)
                .unwrap();
            let operation = if stream {
                "streamRawPredict"
            } else {
                "rawPredict"
            };
            assert_eq!(
                prepared.url,
                format!(
                    "https://{host}/v1/projects/fixture-project/locations/\
                {region}/publishers/anthropic/models/claude-sonnet-4-5@20250929:{operation}"
                )
            );
            let mut expected: Value =
                serde_json::from_str(include_str!("../fixtures/vertex/anthropic-request.json"))
                    .unwrap();
            expected["stream"] = json!(stream);
            assert_eq!(prepared.body, expected);
            assert!(prepared.body.get("model").is_none());
            assert_eq!(prepared.body["anthropic_version"], "vertex-2023-10-16");
            assert!(!prepared.headers.iter().any(|(name, _)| name == "x-api-key"));
        }
    }
}
#[test]
fn gemini_functions_signatures_usage_and_blocked_fixture_reuse_native_conversion() {
    let provider = Vertex::gemini(identity("global"));
    let native = serde_json::from_str(include_str!("../fixtures/vertex/gemini.json")).unwrap();
    let response = provider
        .transform_response("gemini-2.5-pro", native)
        .unwrap();
    let message = &response["choices"][0]["message"];
    assert_eq!(message["tool_calls"][0]["function"]["name"], "weather");
    assert!(message.to_string().contains("Zml4dHVyZQ=="));
    assert_eq!(response["usage"]["prompt_tokens"], 10);
    assert_eq!(response["usage"]["completion_tokens"], 5);
    assert_eq!(response["usage"]["reasoning_tokens"], 2);
    assert_eq!(response["usage"]["cache_read_tokens"], 4);
    let blocked = serde_json::from_str(include_str!("../fixtures/vertex/blocked.json")).unwrap();
    let response = provider
        .transform_response("gemini-2.5-pro", blocked)
        .unwrap();
    assert_eq!(response["choices"][0]["finish_reason"], "content_filter");
}
async fn chunks(provider: &Vertex, model: &str, wire: Vec<u8>) -> Vec<Value> {
    let bytes = futures::stream::iter(wire.into_iter().map(|b| Ok(bytes::Bytes::from(vec![b]))));
    let mut events = provider.transport().unwrap().events(model, Box::pin(bytes));
    let mut normalizer = provider.stream_normalizer(model);
    let mut chunks = Vec::new();
    while let Some(event) = events.next().await {
        if let Some(chunk) = normalizer.push(&event.unwrap()).unwrap() {
            chunks.push(serde_json::from_str(&chunk).unwrap());
        }
    }
    if let Some(chunk) = normalizer.finish().unwrap() {
        chunks.push(serde_json::from_str(&chunk).unwrap());
    }
    chunks
}
#[tokio::test]
async fn recorded_sse_handles_gemini_signatures_and_anthropic_tool_fragments() {
    let wire = format!(
        "data: {}\n\n",
        include_str!("../fixtures/vertex/gemini.json").trim()
    );
    let gemini = Vertex::gemini(identity("global"));
    let result = chunks(&gemini, "gemini-2.5-pro", wire.into_bytes()).await;
    assert!(result
        .iter()
        .any(|chunk| chunk.to_string().contains("Zml4dHVyZQ==")));
    assert!(result
        .iter()
        .any(|chunk| chunk["usage"]["completion_tokens"] == 5));
    let anthropic = Vertex::anthropic(identity("us-central1"));
    let result = chunks(
        &anthropic,
        "claude-sonnet-4-5@20250929",
        include_bytes!("../fixtures/vertex/anthropic.sse").to_vec(),
    )
    .await;
    let calls: Vec<_> = result
        .iter()
        .filter_map(|chunk| chunk["choices"][0]["delta"]["tool_calls"].as_array())
        .flatten()
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["arguments"], "{\"city\":\"Paris\"}");
    assert!(result.iter().any(|c| c["usage"]["cache_read_tokens"] == 4));
    assert!(result.iter().any(|c| c["usage"]["completion_tokens"] == 3));
}
#[tokio::test]
async fn codes_classify_google_and_midstream_anthropic_errors() {
    let provider = Vertex::gemini(identity("global"));
    let native =
        serde_json::from_str(include_str!("../fixtures/vertex/google-error.json")).unwrap();
    let error = provider.transform_response("model", native).unwrap_err();
    let error = VertexError::from_shim(&error).unwrap();
    assert_eq!(error.kind, VertexErrorKind::Throttled);
    assert_eq!(error.status, 429);
    let provider = Vertex::anthropic(identity("global"));
    let wire = include_bytes!("../fixtures/vertex/anthropic-error.sse").to_vec();
    let bytes = futures::stream::iter([Ok(bytes::Bytes::from(wire))]);
    let mut events = provider
        .transport()
        .unwrap()
        .events("model", Box::pin(bytes));
    assert!(events.next().await.unwrap().is_ok());
    let error = events.next().await.unwrap().unwrap_err();
    assert_eq!(
        VertexError::from_shim(&error).unwrap().kind,
        VertexErrorKind::Unavailable
    );
    assert!(events.next().await.is_none());
}

#[tokio::test]
async fn blocked_sse_is_a_terminal_filter_and_retains_native_usage() {
    let wire = format!(
        "data: {}\n\n",
        include_str!("../fixtures/vertex/blocked.json").trim()
    );
    let provider = Vertex::gemini(identity("global"));
    let result = chunks(&provider, "gemini-2.5-pro", wire.into_bytes()).await;
    assert!(result
        .iter()
        .any(|chunk| chunk["choices"][0]["finish_reason"] == "content_filter"));
    assert!(result
        .iter()
        .any(|chunk| chunk["usage"]["vertex"]["promptTokenCount"] == 5));
}

#[test]
fn untrusted_error_codes_cannot_echo_tokens_or_prompts() {
    let provider = Vertex::gemini(identity("global"));
    let error = provider
        .transform_response(
            "model",
            json!({"error":{
        "code":403,"status":"secret-fixture","message":"secret-fixture",
        "details":[{"secret":"secret-fixture"}]}}),
        )
        .unwrap_err();
    assert_eq!(
        VertexError::from_shim(&error).unwrap().kind,
        VertexErrorKind::AccessDenied
    );
    assert!(!error.to_string().contains("secret-fixture"));
}

#[test]
fn invalid_route_components_fail_locally_and_model_reserved_bytes_stay_in_one_segment() {
    for (project, region) in [
        ("bad/project", "global"),
        ("fixture-project", "bad.host"),
        ("fixture-project", ""),
        ("", "us-central1"),
    ] {
        let provider = Vertex::gemini(GoogleIdentity::token_source(
            project,
            region,
            "fixture",
            Arc::new(Fixed),
        ));
        assert!(provider
            .transform_request(
                "model",
                &json!({
            "messages":[{"role":"user","content":"Hi"}]})
            )
            .is_err());
    }
    let provider = Vertex::gemini(identity("global"));
    let request = json!({"messages":[{"role":"user","content":"Hi"}]});
    let prepared = provider.transform_request("model/a:b", &request).unwrap();
    assert!(prepared
        .url
        .ends_with("/models/model%2Fa%3Ab:generateContent"));
    assert!(provider.transform_request("", &request).is_err());
}

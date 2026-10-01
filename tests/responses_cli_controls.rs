#![cfg(feature = "proxy")]

#[path = "support/native_post.rs"]
mod native_post;
use axum::http::StatusCode;
use llmshim::{providers::openai_compat::OpenAiCompatible, reasoning::WireFormat, router::Router};
use native_post::post;
use serde_json::json;

#[tokio::test]
async fn cli_controls_accept_source_values_and_refuse_near_misses() {
    let mut server = mockito::Server::new_async().await;
    let directory = tempfile::tempdir().unwrap();
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(
                OpenAiCompatible::new("local", server.url(), None)
                    .with_wire(WireFormat::OpenAiResponses),
            ),
        ),
        None,
    )
    .layer(axum::Extension(std::sync::Arc::new(
        llmshim::proxy::wire::Receipts::new(directory.path().to_owned()),
    )));
    for (field, accepted, refused) in [
        (
            "client_metadata",
            json!({"session_id": "test"}),
            json!({"session_id": 3}),
        ),
        ("prompt_cache_key", json!("affinity"), json!(3)),
        ("parallel_tool_calls", json!(false), json!("false")),
        ("service_tier", json!("priority"), json!("unknown")),
        (
            "stream_options",
            json!({"reasoning_summary_delivery": "sequential_cutoff"}),
            json!({"reasoning_summary_delivery": "unknown"}),
        ),
        (
            "access_programs",
            json!({"cyber": "daybreak_blue"}),
            json!({"cyber": "unknown"}),
        ),
        (
            "access_programs",
            json!({"cyber": "standard"}),
            json!({"extra": "standard"}),
        ),
        (
            "reasoning",
            json!({"context": "all_turns"}),
            json!({"context": "last_turn"}),
        ),
        (
            "reasoning",
            json!({"context": "auto"}),
            json!({"context": 3}),
        ),
        (
            "reasoning",
            json!({"context": "current_turn"}),
            json!({"surprise": "all_turns"}),
        ),
        ("reasoning", json!({"effort": 1024}), json!({"effort": -1})),
        (
            "reasoning",
            json!({"summary": "detailed"}),
            json!({"summary": "unknown"}),
        ),
        ("text", json!({"verbosity": "low"}), json!({"verbosity": 3})),
        (
            "text",
            json!({"verbosity": "high"}),
            json!({"surprise": "high"}),
        ),
    ] {
        let mut expected = json!({});
        if !matches!(field, "client_metadata" | "stream_options") {
            expected[field] = accepted.clone();
        }
        let upstream = server
            .mock("POST", "/responses")
            .match_body(mockito::Matcher::PartialJson(expected))
            .with_body(
                json!({"id": "response", "status": "completed", "output": [{
                    "type": "message", "content": [{"type": "output_text", "text": "ok"}],
                }]})
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let mut request = json!({"model": "local/test", "input": "hello"});
        request[field] = accepted;
        let (status, result) = post(app.clone(), "/v1/responses", request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{field}: {result}");
        assert_eq!(result["output"][0]["content"][0]["text"], "ok");
        request[field] = refused;
        let (status, result) = post(app.clone(), "/v1/responses", request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {result}");
        assert!(result["error"]["message"].as_str().unwrap().contains(field));
        upstream.assert_async().await;
        upstream.remove_async().await;
    }
}

#[tokio::test]
async fn native_hosted_tools_and_nullable_controls_keep_their_request_shape() {
    let mut server = mockito::Server::new_async().await;
    let tools = json!([
        {"type": "web_search", "external_web_access": false,
            "filters": {"allowed_domains": ["example.com"]}},
        {"type": "tool_search", "execution": "server", "description": "Find tools",
            "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}},
    ]);
    let upstream = server
        .mock("POST", "/responses")
        .match_body(mockito::Matcher::PartialJson(json!({"tools": tools})))
        .with_body(json!({"id": "response", "status": "completed", "output": []}).to_string())
        .expect(1)
        .create_async()
        .await;
    let native = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(
                OpenAiCompatible::new("local", server.url(), None)
                    .with_wire(WireFormat::OpenAiResponses),
            ),
        ),
        None,
    );
    let request = json!({"model": "local/test", "input": "hello", "tools": tools,
        "reasoning": null, "text": null, "access_programs": null});
    let (status, result) = post(native.clone(), "/v1/responses", request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let mut malformed = request.clone();
    malformed["tools"][1]["execution"] = json!("unknown");
    let (status, result) = post(native, "/v1/responses", malformed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
    upstream.assert_async().await;
    let foreign = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        ),
        None,
    );
    let (status, result) = post(foreign, "/v1/responses", request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
    assert!(result["error"]["message"]
        .as_str()
        .unwrap()
        .contains("native provider"));
}

#[tokio::test]
async fn unknown_fields_are_refused_beside_supported_client_metadata() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/responses")
        .expect(0)
        .create_async()
        .await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(
                OpenAiCompatible::new("local", server.url(), None)
                    .with_wire(WireFormat::OpenAiResponses),
            ),
        ),
        None,
    );
    let (status, result) = post(
        app,
        "/v1/responses",
        json!({"model": "local/test",
        "input": "hello", "client_metadata": {"session_id": "test"},
        "unknown_cli_field": true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        result["error"]["message"],
        "unsupported Responses request field: unknown_cli_field"
    );
    upstream.assert_async().await;
}

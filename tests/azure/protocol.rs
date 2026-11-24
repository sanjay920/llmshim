use super::*;
use futures::StreamExt;

#[test]
fn deployments_and_v1_request_match_recorded_shape_without_credentials() {
    let provider = provider();
    let golden: Value =
        serde_json::from_str(include_str!("../fixtures/azure/request.json")).unwrap();
    let mut input = golden.clone();
    input.as_object_mut().unwrap().remove("model");
    input.as_object_mut().unwrap().remove("stream_options");
    let sent = provider
        .transform_request("chat-deployment", &input)
        .unwrap();
    assert_eq!(
        sent.url,
        "https://fixture.openai.azure.com/openai/v1/chat/completions"
    );
    assert_eq!(sent.body, golden);
    assert_eq!(
        sent.headers,
        vec![("Content-Type".into(), "application/json".into())]
    );
    assert!(provider.transform_request("unconfigured", &input).is_err());
    assert!(provider
        .transform_request(
            "chat-deployment",
            &json!({"messages":[],
        "x-azure":{"model":"another"}})
        )
        .is_err());
}

#[test]
fn tools_usage_and_content_filters_are_preserved_and_errors_are_typed() {
    let provider = provider();
    let native = serde_json::from_str(include_str!("../fixtures/azure/response.json")).unwrap();
    let response = provider
        .transform_response("chat-deployment", native)
        .unwrap();
    assert_eq!(response["model"], "chat-deployment");
    let message = response["choices"][0]["message"].clone();
    let call = &message["tool_calls"][0];
    assert_eq!(call["function"]["name"], "weather");
    assert_eq!(call["function"]["arguments"], r#"{"city":"Oslo"}"#);
    let replay = provider
        .transform_request(
            "chat-deployment",
            &json!({"messages":[
        {"role":"user","content":"Weather?"}, message,
        {"role":"tool","tool_call_id":call["id"],"content":"sunny"}]}),
        )
        .unwrap();
    assert_eq!(
        replay.body["messages"][1]["tool_calls"][0]["id"],
        "call_fixture"
    );
    assert_eq!(replay.body["messages"][2]["tool_call_id"], "call_fixture");
    assert_eq!(response["usage"]["cache_read_tokens"], 40);
    assert_eq!(
        response["usage"]["completion_tokens_details"]["reasoning_tokens"],
        5
    );
    let filtered = provider
        .transform_response(
            "chat-deployment",
            serde_json::from_str(include_str!("../fixtures/azure/filtered.json")).unwrap(),
        )
        .unwrap();
    assert_eq!(filtered["choices"][0]["finish_reason"], "content_filter");
    assert_eq!(
        filtered["choices"][0]["content_filter_results"]["violence"]["filtered"],
        true
    );
    let transport = provider.transport().unwrap();
    for (status, body, kind) in [
        (
            400,
            include_str!("../fixtures/azure/filter-error.json"),
            AzureErrorKind::ContentFilter,
        ),
        (
            404,
            include_str!("../fixtures/azure/error.json"),
            AzureErrorKind::NotFound,
        ),
        (
            403,
            r#"{"error":{"code":"arbitrary-secret","message":"token expired"}}"#,
            AzureErrorKind::AccessDenied,
        ),
        (
            429,
            r#"{"error":{"code":"RateLimitReached"}}"#,
            AzureErrorKind::Throttled,
        ),
    ] {
        let error = transport.error(
            status,
            &Default::default(),
            body.into(),
            Some(Duration::from_secs(2)),
        );
        let typed = AzureError::from_shim(&error).unwrap();
        assert_eq!(typed.kind, kind);
        assert_eq!(typed.status, status);
        assert!(!error.to_string().contains("secret"));
        assert!(
            matches!(error, llmshim::error::ShimError::ProviderError {retry_after:Some(d), ..}
            if d==Duration::from_secs(2))
        );
    }
}

#[tokio::test]
async fn sse_tools_and_empty_choices_usage_tail_survive_byte_splits() {
    let provider = provider();
    let wire = include_bytes!("../fixtures/azure/chat.sse");
    let bytes = futures::stream::iter(wire.iter().map(|b| Ok(bytes::Bytes::from(vec![*b]))));
    let transport = provider.transport().unwrap();
    let mut events = transport.events("chat-deployment", Box::pin(bytes));
    let mut normalizer = transport
        .stream_normalizer(
            provider.replay_target("chat-deployment"),
            Default::default(),
        )
        .unwrap();
    let mut chunks = Vec::new();
    while let Some(event) = events.next().await {
        if let Some(chunk) = normalizer.push(&event.unwrap()).unwrap() {
            chunks.push(serde_json::from_str::<Value>(&chunk).unwrap());
        }
    }
    normalizer.finish().unwrap();
    let usage = chunks.iter().find_map(|chunk| chunk.get("usage")).unwrap();
    assert_eq!(usage["prompt_tokens"], 100);
    assert_eq!(usage["completion_tokens"], 20);
    assert_eq!(usage["cache_read_tokens"], 40);
    assert_eq!(usage["completion_tokens_details"]["reasoning_tokens"], 5);
    assert!(chunks
        .iter()
        .all(|chunk| chunk["model"] == "chat-deployment"));
    let arguments: String = chunks
        .iter()
        .filter_map(|chunk| {
            chunk["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
        })
        .collect();
    assert_eq!(arguments, r#"{"city":"Oslo"}"#);
}

#[test]
fn azure_never_uses_direct_api_prices_for_a_deployment() {
    assert!(llmshim::cost::for_target("openai", "gpt-6-sol").is_some());
    let mut answer = json!({"usage":{"prompt_tokens":100,"completion_tokens":20}});
    llmshim::cost::stamp("azure", "gpt-6-sol", &mut answer);
    assert!(answer["usage"]["cost_usd"].is_null());
    assert!(!llmshim::cost::is_priceable("azure", "gpt-6-sol"));
}

#[tokio::test]
async fn content_filter_sse_stops_and_midstream_error_remains_typed() {
    let provider = provider();
    let transport = provider.transport().unwrap();
    let source = futures::stream::once(async {
        Ok(bytes::Bytes::from_static(include_bytes!(
            "../fixtures/azure/filtered.sse"
        )))
    });
    let mut events = transport.events("chat-deployment", Box::pin(source));
    let mut normalizer = provider.stream_normalizer("chat-deployment");
    let mut reasons = Vec::new();
    while let Some(event) = events.next().await {
        if let Some(chunk) = normalizer.push(&event.unwrap()).unwrap() {
            let chunk: Value = serde_json::from_str(&chunk).unwrap();
            if let Some(reason) = chunk["choices"][0]["finish_reason"].as_str() {
                reasons.push(reason.to_owned());
            }
        }
    }
    if let Some(chunk) = normalizer.finish().unwrap() {
        let chunk: Value = serde_json::from_str(&chunk).unwrap();
        if let Some(reason) = chunk["choices"][0]["finish_reason"].as_str() {
            reasons.push(reason.to_owned());
        }
    }
    assert!(!reasons.is_empty());
    assert!(reasons.iter().all(|reason| reason == "content_filter"));
    let wire = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":"start"},"finish_reason":null}]}),
        include_str!("../fixtures/azure/filter-error.json").trim()
    );
    let source = futures::stream::once(async move { Ok(bytes::Bytes::from(wire)) });
    let mut events = transport.events("chat-deployment", Box::pin(source));
    assert!(events.next().await.unwrap().is_ok());
    let error = events.next().await.unwrap().unwrap_err();
    assert_eq!(
        AzureError::from_shim(&error).unwrap().kind,
        AzureErrorKind::ContentFilter
    );
    assert!(!error.to_string().contains("secret-fixture"));
    assert!(events.next().await.is_none());
}

#[test]
fn direct_stream_conversion_rejects_non_object_events_without_panicking() {
    let provider = provider();
    for event in ["[]", "1", "true", r#""text""#, "null"] {
        let error = provider
            .transform_stream_chunk("chat-deployment", event)
            .unwrap_err();
        let error = AzureError::from_shim(&error).unwrap();
        assert_eq!(error.kind, AzureErrorKind::Protocol);
        assert_eq!(error.code, "InvalidEvent");
    }
}

#[test]
fn native_overrides_cannot_replace_filtered_messages_or_stream_controls() {
    for field in ["messages", "stream", "stream_options"] {
        let mut input = json!({"messages":[{"role":"user","content":"Hi"}],"x-azure":{}});
        input["x-azure"][field] = json!(null);
        let error = provider()
            .transform_request("chat-deployment", &input)
            .err()
            .expect("override refused");
        let typed = AzureError::from_shim(&error).unwrap();
        assert_eq!(typed.kind, AzureErrorKind::Validation);
        assert_eq!(typed.status, 400);
        assert_eq!(typed.code, format!("UnsupportedOverride:{field}"));
    }
}

#[test]
fn mapped_models_determine_prices_even_when_deployment_names_collide() {
    let models = ["claude-sonnet-4-6", "claude-opus-4-6"];
    let provider = provider()
        .with_deployment(models[0], models[1])
        .unwrap()
        .with_deployment(models[1], models[0])
        .unwrap();
    let usage = json!({"prompt_tokens":100,"completion_tokens":20});
    let mut prices = Vec::new();
    for (deployment, model) in [(models[0], models[1]), (models[1], models[0])] {
        let target = provider.replay_target(deployment);
        let expected = llmshim::cost::cost_usd("azure", model, &usage).unwrap();
        let mut answer = json!({"usage":usage});
        llmshim::cost::stamp(&target.provider, target.pricing_model(), &mut answer);
        assert_eq!(answer["usage"]["cost_usd"], expected);
        prices.push(expected);
    }
    assert_ne!(prices[0], prices[1]);
    let target = super::provider().replay_target(models[0]);
    assert!(llmshim::cost::is_priceable("azure", models[0]));
    assert!(!llmshim::cost::is_priceable(
        &target.provider,
        target.pricing_model()
    ));
}

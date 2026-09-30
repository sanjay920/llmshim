use llmshim::{
    client::ShimClient,
    error::ShimError,
    providers::{anthropic::Anthropic, gemini::Gemini, openai::OpenAi},
    router::Router,
};
use mockito::Matcher;
use serde_json::{json, Value};

#[tokio::test]
async fn openai_routes_bytes_controls_and_prices_only_known_usage() {
    let mut server = mockito::Server::new_async().await;
    let mock = server.mock("POST", "/images/generations")
        .match_header("authorization", "Bearer test-key")
        .match_body(Matcher::Json(json!({"model":"gpt-image-1", "prompt":"a cat", "n":2, "output_format":"webp", "quality":"low"})))
        .with_body(json!({"output_format":"webp", "data":[{"b64_json":"AQID", "revised_prompt":"a small cat"},{"b64_json":"BAU="}], "usage":{"input_tokens":10,"input_tokens_details":{"text_tokens":10,"image_tokens":0},"output_tokens":20,"output_tokens_details":{"image_tokens":20,"text_tokens":0}}}).to_string())
        .expect(1).create_async().await;
    let router = Router::new()
        .register(
            "openai",
            Box::new(OpenAi::new("test-key".into()).with_base_url(server.url())),
        )
        .alias("art", "openai/gpt-image-1")
        .route(
            "drawing",
            llmshim::config::Route {
                model: "art".into(),
                settings: [("quality".into(), json!("low"))].into(),
            },
        );
    let result = llmshim::images(&router, &json!({"model":"route/drawing", "prompt":"a cat", "n":2,"output_format":"webp", "messages":[],"x-gemini":{"aspectRatio":"16:9"}})).await.unwrap();
    assert_eq!(result.images.len(), 2);
    assert_eq!(result.images[0].bytes, [1, 2, 3]);
    assert_eq!(result.images[1].bytes, [4, 5]);
    assert_eq!(result.images[0].media_type, "image/webp");
    assert_eq!(
        result.images[0].revised_prompt.as_deref(),
        Some("a small cat")
    );
    assert_eq!(result.usage["cost_usd"], 0.00085);
    assert_eq!(result.usage["cost_source"], "catalog");
    mock.assert_async().await;
}

#[tokio::test]
async fn gemini_transports_image_modalities_and_preserves_usage() {
    let mut server = mockito::Server::new_async().await;
    let router = Router::new()
        .register(
            "gemini",
            Box::new(Gemini::new("google-key".into()).with_base_url(server.url())),
        )
        .alias("drawing", "gemini/gemini-3.1-flash-image");
    for model in [
        "drawing",
        "gemini-2.5-flash-image",
        "gemini-3.1-flash-lite-image",
        "gemini-3-pro-image",
    ] {
        let native_model = if model == "drawing" {
            "gemini-3.1-flash-image"
        } else {
            model
        };
        let (request, expected_body) = if model == "drawing" {
            (
                json!({"model":model,"prompt":"a cat","n":1,"x-gemini":{"imageConfig":{"aspectRatio":"16:9","imageSize":"2K"},"responseModalities":["TEXT"]},"quality":"high"}),
                json!({"contents":[{"role":"user","parts":[{"text":"a cat"}]}],"generationConfig":{"responseModalities":["TEXT","IMAGE"],"imageConfig":{"aspectRatio":"16:9","imageSize":"2K"}}}),
            )
        } else {
            (
                json!({"model":model,"prompt":"a cat"}),
                json!({"contents":[{"role":"user","parts":[{"text":"a cat"}]}],"generationConfig":{"responseModalities":["TEXT","IMAGE"]}}),
            )
        };
        let mock = server.mock("POST", format!("/models/{native_model}:generateContent").as_str())
            .match_header("x-goog-api-key", "google-key")
            .match_body(Matcher::Json(expected_body))
            .with_body(json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"Here is your cat"},{"thought":true,"inlineData":{"data":"CQ==","mimeType":"image/png"}},{"inlineData":{"data":"AQID","mimeType":"image/jpeg"}},{"thought":false,"inlineData":{"data":"BAU=","mimeType":"image/png"}}]}}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":100,"totalTokenCount":112,"promptTokensDetails":[{"modality":"TEXT","tokenCount":12}],"candidatesTokensDetails":[{"modality":"IMAGE","tokenCount":100}]}}).to_string())
            .expect(1).create_async().await;
        let response = llmshim::images(&router, &request).await.unwrap();
        assert_eq!(response.images.len(), 2);
        assert_eq!(response.images[0].bytes, [1, 2, 3]);
        assert_eq!(response.images[0].media_type, "image/jpeg");
        assert_eq!(response.images[1].bytes, [4, 5]);
        assert_eq!(response.images[1].media_type, "image/png");
        assert_eq!(response.usage["promptTokenCount"], 12);
        assert_eq!(response.usage["candidatesTokenCount"], 100);
        assert_eq!(response.usage["totalTokenCount"], 112);
        assert_eq!(
            response.usage["candidatesTokensDetails"][0]["modality"],
            "IMAGE"
        );
        let expected = match native_model {
            "gemini-3.1-flash-image" => 0.006006,
            "gemini-2.5-flash-image" => 0.0030036,
            "gemini-3.1-flash-lite-image" => 0.003003,
            "gemini-3-pro-image" => 0.012024,
            _ => unreachable!(),
        };
        assert_eq!(response.usage["cost_usd"], expected);
        assert_eq!(response.usage["cost_source"], "catalog");
        mock.assert_async().await;
        mock.remove_async().await;
    }
}

#[tokio::test]
async fn invalid_requests_fail_before_transport() {
    let client = ShimClient::new();
    let openai = OpenAi::new("unused".into()).with_base_url("http://127.0.0.1:1".into());
    let gemini = Gemini::new("unused".into()).with_base_url("http://127.0.0.1:1".into());
    for request in [
        json!({}),
        json!({"prompt":" "}),
        json!({"prompt":1}),
        json!({"prompt":"cat","n":0}),
        json!({"prompt":"cat","n":11}),
        json!({"prompt":"cat","n":1.5}),
        json!({"prompt":"cat","stream":true}),
        json!({"prompt":"cat","output_format":"gif"}),
        json!({"prompt":"cat","output_format":null}),
    ] {
        assert!(
            matches!(
                client.images(&openai, "gpt-image-1", &request).await,
                Err(ShimError::ProviderError { status: 400, .. })
            ),
            "{request}"
        );
    }
    for (provider, model, request) in [
        (
            &openai as &dyn llmshim::provider::Provider,
            "gpt-5",
            json!({"prompt":"cat"}),
        ),
        (&gemini, "gemini-3.1-flash-image/x", json!({"prompt":"cat"})),
        (&gemini, "imagen-4.0-generate-001", json!({"prompt":"cat"})),
        (&gemini, "gemini-3", json!({"prompt":"cat"})),
        (
            &gemini,
            "gemini-3.1-flash-image",
            json!({"prompt":"cat","n":2}),
        ),
    ] {
        assert!(matches!(
            client.images(provider, model, &request).await,
            Err(ShimError::ProviderError { status: 400, .. })
        ));
    }
    assert!(matches!(
        client
            .images(
                &Anthropic::new("unused".into()),
                "claude",
                &json!({"prompt":"cat"})
            )
            .await,
        Err(ShimError::ProviderError { status: 400, .. })
    ));
    assert!(matches!(
        llmshim::images(&Router::new(), &json!({"prompt":"cat"})).await,
        Err(ShimError::MissingModel)
    ));
}

#[tokio::test]
async fn malformed_or_filtered_output_is_an_error_and_http_status_survives() {
    let mut server = mockito::Server::new_async().await;
    let provider = OpenAi::new("key".into()).with_base_url(server.url());
    for body in [
        json!({}),
        json!({"data":[]}),
        json!({"data":[{"url":"http://127.0.0.1:1/secret"}]}),
        json!({"data":[{"b64_json":"!"}]}),
        json!({"data":[{"b64_json":""}]}),
        json!({"data":[{"b64_json":"AQ=="}],"output_format":"gif"}),
        json!({"data":[{"b64_json":"AQ=="}],"output_format":null}),
    ] {
        let mock = server
            .mock("POST", "/images/generations")
            .with_body(body.to_string())
            .expect(1)
            .create_async()
            .await;
        assert!(
            matches!(
                ShimClient::new()
                    .images(&provider, "gpt-image-1", &json!({"prompt":"cat"}))
                    .await,
                Err(ShimError::ProviderError { status: 502, .. })
            ),
            "{body}"
        );
        mock.assert_async().await;
        mock.remove_async().await;
    }
    let mock = server
        .mock("POST", "/images/generations")
        .with_status(400)
        .with_body("blocked")
        .expect(1)
        .create_async()
        .await;
    assert!(
        matches!(ShimClient::new().images(&provider,"gpt-image-1",&json!({"prompt":"cat"})).await,Err(ShimError::ProviderError{status:400,body,..}) if body == "blocked")
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn pricing_near_misses_stay_unknown_and_provider_bill_wins() {
    let mut server = mockito::Server::new_async().await;
    let provider = OpenAi::new("key".into()).with_base_url(server.url());
    let known = json!({"input_tokens":10,"input_tokens_details":{"text_tokens":10,"image_tokens":0},"output_tokens":20,"output_tokens_details":{"image_tokens":20,"text_tokens":0}});
    for (model, usage, cost) in [
        ("gpt-image-future", known.clone(), Value::Null),
        ("gpt-image-1.5", known.clone(), json!(0.00069)),
        ("gpt-image-1-mini", known.clone(), json!(0.00018)),
        (
            "gpt-image-1",
            json!({"input_tokens":10,"input_tokens_details":{"text_tokens":10,"image_tokens":0},"output_tokens":20}),
            Value::Null,
        ),
        (
            "gpt-image-1",
            {
                let mut u = known.clone();
                u["output_tokens_details"]["image_tokens"] = json!(19);
                u
            },
            Value::Null,
        ),
        (
            "gpt-image-1",
            {
                let mut u = known.clone();
                u["output_tokens_details"]["text_tokens"] = json!(1);
                u
            },
            Value::Null,
        ),
        ("gpt-image-1", json!({}), Value::Null),
        ("gpt-image-1", json!("unavailable"), Value::Null),
        (
            "gpt-image-1",
            json!({"cost":0.25,"input_tokens":10,"input_tokens_details":{"text_tokens":10,"image_tokens":0},"output_tokens":20,"output_tokens_details":{"image_tokens":20,"text_tokens":0}}),
            json!(0.25),
        ),
        (
            "gpt-image-1",
            json!({"input_tokens":0,"input_tokens_details":{"text_tokens":0,"image_tokens":0},"output_tokens":0,"output_tokens_details":{"image_tokens":0,"text_tokens":0}}),
            Value::Null,
        ),
        (
            "gpt-image-1",
            json!({"input_tokens":10,"input_tokens_details":{"text_tokens":10,"image_tokens":1},"output_tokens":20,"output_tokens_details":{"image_tokens":20,"text_tokens":0}}),
            Value::Null,
        ),
        (
            "gpt-image-1",
            json!({"input_tokens":11,"input_tokens_details":{"text_tokens":10,"image_tokens":0},"output_tokens":20,"output_tokens_details":{"image_tokens":20,"text_tokens":0}}),
            Value::Null,
        ),
        (
            "gpt-image-1",
            json!({"input_tokens":10,"input_tokens_details":{"text_tokens":10,"image_tokens":0,"cached_tokens":1},"output_tokens":20,"output_tokens_details":{"image_tokens":20,"text_tokens":0}}),
            Value::Null,
        ),
        ("gpt-image-1", json!({"cost":0.0}), json!(0.0)),
        ("gpt-image-1", json!({"cost":-1}), Value::Null),
    ] {
        let mock = server
            .mock("POST", "/images/generations")
            .with_body(json!({"data":[{"b64_json":"AQ=="}],"usage":usage}).to_string())
            .expect(1)
            .create_async()
            .await;
        let response = ShimClient::new()
            .images(&provider, model, &json!({"prompt":"cat"}))
            .await
            .unwrap();
        assert_eq!(response.usage["cost_usd"], cost, "{model}");
        assert_eq!(
            response.usage["cost_source"],
            if usage["cost"].as_f64().is_some_and(|n| n >= 0.0) {
                "provider"
            } else if cost.is_null() {
                "unknown"
            } else {
                "catalog"
            }
        );
        mock.assert_async().await;
        mock.remove_async().await;
    }
}

#[tokio::test]
async fn missing_format_uses_requested_format_and_invalid_gemini_output_fails() {
    let mut server = mockito::Server::new_async().await;
    let openai = OpenAi::new("key".into()).with_base_url(server.url());
    for (body, valid) in [
        (json!({"data":[{"b64_json":"AQ=="}]}), true),
        (Value::Null, false),
        (json!("upstream error"), false),
    ] {
        let mock = server
            .mock("POST", "/images/generations")
            .with_body(body.to_string())
            .expect(1)
            .create_async()
            .await;
        let response = ShimClient::new()
            .images(
                &openai,
                "gpt-image-1",
                &json!({"prompt":"cat","output_format":"jpeg"}),
            )
            .await;
        if valid {
            assert_eq!(response.unwrap().images[0].media_type, "image/jpeg");
        } else {
            assert!(response.is_err());
        }
        mock.assert_async().await;
        mock.remove_async().await;
    }
    let gemini = Gemini::new("key".into()).with_base_url(server.url());
    for body in [
        json!({"candidates":[]}),
        json!({"promptFeedback":{"blockReason":"IMAGE_SAFETY"}}),
        json!({"candidates":[{"content":{"parts":[{"text":"No image"}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"inlineData":{"data":"AQ==","mimeType":"text/html"}}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"inlineData":{"data":"AQ=="}}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png"}}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"inlineData":{"data":"!","mimeType":"image/png"}}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"inlineData":{"data":"","mimeType":"image/png"}}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"thought":true,"inlineData":{"data":"AQ==","mimeType":"image/png"}}]}}]}),
        json!({"candidates":[{"finishReason":"IMAGE_SAFETY","content":{"parts":[{"inlineData":{"data":"AQ==","mimeType":"image/png"}}]}}]}),
        json!({"candidates":[{"content":{"parts":{}}}]}),
        json!({}),
    ] {
        let mock = server
            .mock("POST", "/models/gemini-3.1-flash-image:generateContent")
            .with_body(body.to_string())
            .expect(1)
            .create_async()
            .await;
        assert!(
            matches!(
                ShimClient::new()
                    .images(&gemini, "gemini-3.1-flash-image", &json!({"prompt":"cat"}))
                    .await,
                Err(ShimError::ProviderError { status: 502, .. })
            ),
            "{body}"
        );
        mock.assert_async().await;
        mock.remove_async().await;
    }
}

#[tokio::test]
async fn gemini_prices_only_classified_image_output_and_reported_bill_wins() {
    let mut server = mockito::Server::new_async().await;
    let provider = Gemini::new("key".into()).with_base_url(server.url());
    let known = json!({"promptTokenCount":10,"promptTokensDetails":[{"modality":"TEXT","tokenCount":10}],"candidatesTokenCount":20,"candidatesTokensDetails":[{"modality":"IMAGE","tokenCount":20}]});
    for (field, value, expected) in [
        ("cost", json!(0.25), json!(0.25)),
        ("cost", json!(0.0), json!(0.0)),
        ("cost", json!(-1), json!(0.001205)),
        (
            "candidatesTokensDetails",
            json!([{"modality":"IMAGE","tokenCount":20},{"modality":"TEXT","tokenCount":0}]),
            json!(0.001205),
        ),
        (
            "candidatesTokensDetails",
            json!([{"modality":"IMAGE","tokenCount":19},{"modality":"TEXT","tokenCount":1}]),
            Value::Null,
        ),
        (
            "promptTokensDetails",
            json!([{"modality":"IMAGE","tokenCount":10}]),
            Value::Null,
        ),
        ("promptTokensDetails", Value::Null, Value::Null),
        (
            "promptTokensDetails",
            json!([{"tokenCount":10}]),
            Value::Null,
        ),
        (
            "promptTokensDetails",
            json!([{"modality":"TEXT","tokenCount":"10"}]),
            Value::Null,
        ),
        ("promptTokenCount", json!(11), Value::Null),
        ("candidatesTokenCount", json!(21), Value::Null),
        (
            "candidatesTokensDetails",
            json!([{"modality":"IMAGE","tokenCount":18446744073709551615_u64},{"modality":"IMAGE","tokenCount":21}]),
            Value::Null,
        ),
        ("cachedContentTokenCount", json!(1), Value::Null),
        ("thoughtsTokenCount", json!(1), Value::Null),
        ("cachedContentTokenCount", json!(0), json!(0.001205)),
        ("thoughtsTokenCount", json!(0), json!(0.001205)),
    ] {
        let mut usage = known.clone();
        usage[field] = value;
        let mock = server
            .mock("POST", "/models/gemini-3.1-flash-image:generateContent")
            .with_body(json!({"candidates":[{"content":{"parts":[{"inlineData":{"data":"AQ==","mimeType":"image/png"}}]}}],"usageMetadata":usage}).to_string())
            .expect(1).create_async().await;
        let response = ShimClient::new()
            .images(
                &provider,
                "gemini-3.1-flash-image",
                &json!({"prompt":"cat"}),
            )
            .await
            .unwrap();
        assert_eq!(response.usage["cost_usd"], expected, "{usage}");
        assert_eq!(
            response.usage["cost_source"],
            if field == "cost" && usage["cost"].as_f64().unwrap() >= 0.0 {
                "provider"
            } else if expected.is_null() {
                "unknown"
            } else {
                "catalog"
            }
        );
        mock.assert_async().await;
        mock.remove_async().await;
    }
}

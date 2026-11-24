use llmshim::{
    error::ShimError,
    providers::{anthropic::Anthropic, openai::OpenAi},
    router::Router,
};
use serde_json::json;

#[tokio::test]
async fn speech_round_trip_formats_prices_and_second_call() {
    let mut server = mockito::Server::new_async().await;
    let router = Router::new().register(
        "openai",
        Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
    );
    for (model, format, media, price) in [
        ("tts-1", "mp3", "audio/mpeg", 0.000045),
        ("tts-1-hd", "wav", "audio/wav", 0.00009),
        ("tts-1", "flac", "audio/flac", 0.000045),
        ("tts-1", "opus", "audio/ogg", 0.000045),
        ("tts-1", "aac", "audio/aac", 0.000045),
        ("tts-1", "pcm", "application/octet-stream", 0.000045),
    ] {
        let mock = server.mock("POST", "/audio/speech")
            .match_header("authorization", "Bearer key")
            .match_body(mockito::Matcher::Json(json!({"model":model,"input":"hé🦀","voice":"alloy","response_format":format,"speed":0.25})))
            .with_header("content-type", media).with_body([0, 255, 1]).expect(2).create_async().await;
        for _ in 0..2 {
            let response = llmshim::speech(&router, &json!({"model":format!("openai/{model}"),"input":"hé🦀","voice":"alloy","response_format":format,"speed":0.25})).await.unwrap();
            assert_eq!(response.bytes, [0, 255, 1]);
            assert_eq!(
                response.media_type,
                if format == "pcm" { "audio/pcm" } else { media }
            );
            assert_eq!(response.usage["input_characters"], 3);
            assert_eq!(response.usage["cost_usd"], price);
            assert_eq!(response.usage["cost_source"], "catalog");
        }
        mock.assert_async().await;
    }
}

#[tokio::test]
async fn speech_bounds_and_invalid_controls_refuse_without_transport() {
    let mut server = mockito::Server::new_async().await;
    let router = Router::new().register(
        "openai",
        Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
    );
    let absent = server
        .mock("POST", "/audio/speech")
        .expect(0)
        .create_async()
        .await;
    let base = json!({"model":"openai/tts-1","input":"hello","voice":"alloy"});
    for (key, value) in [
        ("input", json!("")),
        ("input", json!(" ")),
        ("input", json!("a".repeat(4097))),
        ("input", json!(1)),
        ("voice", json!("bad voice")),
        ("voice", json!("")),
        ("voice", json!("é")),
        ("voice", json!("a".repeat(65))),
        ("voice", json!(null)),
        ("response_format", json!("html")),
        ("response_format", json!(null)),
        ("speed", json!(0.249)),
        ("speed", json!(4.001)),
        ("speed", json!("1")),
        ("stream", json!(true)),
        ("stream", json!(null)),
        ("model", json!("openai/gpt-5")),
    ] {
        let mut request = base.clone();
        request[key] = value;
        assert!(
            matches!(
                llmshim::speech(&router, &request).await,
                Err(ShimError::ProviderError { status: 400, .. })
            ),
            "{request}"
        );
    }
    absent.assert_async().await;
    absent.remove_async().await;
    for (text, speed) in [("é".repeat(4096), 4.0), ("a".into(), 0.25)] {
        let mock = server
            .mock("POST", "/audio/speech")
            .with_header("content-type", "audio/mpeg")
            .with_body([1])
            .expect(1)
            .create_async()
            .await;
        assert_eq!(llmshim::speech(&router,&json!({"model":"openai/tts-1","input":text,"voice":"nova","speed":speed,"stream":false})).await.unwrap().bytes,[1]);
        mock.assert_async().await;
        mock.remove_async().await;
    }
    let router = Router::new().register("anthropic", Box::new(Anthropic::new("unused".into())));
    assert!(matches!(
        llmshim::speech(
            &router,
            &json!({"model":"anthropic/claude","input":"hello","voice":"alloy"})
        )
        .await,
        Err(ShimError::ProviderError { status: 400, .. })
    ));
    assert!(matches!(
        llmshim::speech(&Router::new(), &json!({})).await,
        Err(ShimError::MissingModel)
    ));
}

#[tokio::test]
async fn speech_rejects_empty_and_non_audio_success_and_preserves_http_error() {
    let mut server = mockito::Server::new_async().await;
    let router = Router::new().register(
        "openai",
        Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
    );
    for (status, media, bytes, expected) in [
        (200, "audio/mpeg", vec![], 502),
        (
            200,
            "application/json",
            b"{\"error\":\"bad\"}".to_vec(),
            502,
        ),
        (200, "audio/wav", vec![1], 502),
        (200, "", vec![1], 502),
        (400, "text/plain", b"blocked".to_vec(), 400),
    ] {
        let mock = server
            .mock("POST", "/audio/speech")
            .with_status(status)
            .with_header("content-type", media)
            .with_body(bytes)
            .expect(1)
            .create_async()
            .await;
        let error = llmshim::speech(
            &router,
            &json!({"model":"openai/tts-1","input":"hello","voice":"alloy"}),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error,ShimError::ProviderError{status,..} if *status==expected),
            "{error}"
        );
        if expected == 400 {
            assert!(matches!(error,ShimError::ProviderError{body,..} if body=="blocked"));
        }
        mock.assert_async().await;
        mock.remove_async().await;
    }
}

#[tokio::test]
async fn speech_routes_supply_defaults_and_aliases_keep_caller_overrides() {
    let mut server = mockito::Server::new_async().await;
    let router = Router::new()
        .register(
            "openai",
            Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
        )
        .alias("speaker", "openai/tts-1-hd")
        .route(
            "voice",
            llmshim::config::Route {
                model: "speaker".into(),
                settings: [
                    ("voice".into(), json!("ash")),
                    ("response_format".into(), json!("wav")),
                ]
                .into(),
            },
        );
    for (voice, input) in [
        ("ash", json!({"model":"route/voice","input":"hi"})),
        (
            "sage",
            json!({"model":"route/voice","input":"hi","voice":"sage"}),
        ),
        (
            "coral",
            json!({"model":"speaker","input":"hi","voice":"coral","response_format":"wav"}),
        ),
    ] {
        let mock = server
            .mock("POST", "/audio/speech")
            .match_body(mockito::Matcher::Json(
                json!({"model":"tts-1-hd","input":"hi","voice":voice,"response_format":"wav"}),
            ))
            .with_header("content-type", "Audio/WAV; charset=binary")
            .with_body([1])
            .expect(1)
            .create_async()
            .await;
        let response = llmshim::speech(&router, &input).await.unwrap();
        assert_eq!(response.media_type, "audio/wav");
        assert_eq!(response.usage["cost_usd"], 0.00006);
        mock.assert_async().await;
    }
}

#[test]
fn native_speech_response_requires_a_supported_format_and_input_for_accounting() {
    use llmshim::provider::Provider;
    let provider = OpenAi::new("unused".into());
    let request = json!({"input":"hello","response_format":"mp3"});
    let response = provider
        .speech_response("tts-1", &request, vec![1], "audio/mpeg")
        .unwrap();
    assert_eq!(response.usage["cost_usd"], 0.000075);
    for request in [
        json!({"input":"hello","response_format":"html"}),
        json!({"response_format":"mp3"}),
    ] {
        assert!(provider
            .speech_response("tts-1", &request, vec![1], "application/octet-stream")
            .is_err());
    }
    let unpriced = provider
        .speech_response("tts-future", &request, vec![1], "audio/mpeg")
        .unwrap();
    assert!(unpriced.usage["cost_usd"].is_null());
    assert_eq!(unpriced.usage["cost_source"], "unknown");
}

#[tokio::test]
async fn new_voice_names_reach_the_provider_and_unknown_voice_errors_survive() {
    let mut server = mockito::Server::new_async().await;
    let router = Router::new().register(
        "openai",
        Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
    );
    let voice = "a".repeat(64);
    let accepted = server
        .mock("POST", "/audio/speech")
        .match_body(mockito::Matcher::PartialJson(json!({"voice":voice})))
        .with_header("content-type", "audio/mpeg")
        .with_body([1])
        .expect(1)
        .create_async()
        .await;
    assert_eq!(
        llmshim::speech(
            &router,
            &json!({"model":"openai/tts-1","input":"hello","voice":voice})
        )
        .await
        .unwrap()
        .bytes,
        [1]
    );
    accepted.assert_async().await;
    let unknown = server
        .mock("POST", "/audio/speech")
        .match_body(mockito::Matcher::PartialJson(
            json!({"voice":"futurevoice"}),
        ))
        .with_status(400)
        .with_body("unknown voice")
        .expect(1)
        .create_async()
        .await;
    assert!(
        matches!(llmshim::speech(&router,&json!({"model":"openai/tts-1","input":"hello","voice":"futurevoice"})).await,Err(ShimError::ProviderError{status:400,body,..}) if body=="unknown voice")
    );
    unknown.assert_async().await;
}

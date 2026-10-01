use llmshim::{
    audio::{TranscriptionRequest, MAX_TRANSCRIPTION_BYTES},
    error::ShimError,
    providers::{anthropic::Anthropic, openai::OpenAi},
    router::Router,
};
use serde_json::json;

fn router(server: &mockito::ServerGuard) -> Router {
    Router::new()
        .register(
            "openai",
            Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
        )
        .alias("listener", "openai/whisper-1")
}

fn request(model: &str) -> TranscriptionRequest {
    TranscriptionRequest::new(model, vec![0, 255, 1], "clip.wav", "audio/wav")
}

#[tokio::test]
async fn multipart_preserves_file_and_controls_for_each_model_and_second_call() {
    let mut server = mockito::Server::new_async().await;
    let router = router(&server);
    for model in ["whisper-1", "gpt-4o-transcribe", "gpt-4o-mini-transcribe"] {
        let mut request = request(&format!("openai/{model}"));
        request.language = Some("en".into());
        request.prompt = Some("Names: Ada".into());
        request.temperature = Some(0.0);
        request.duration_seconds = Some(60.0);
        let mock = server
            .mock("POST", "/audio/transcriptions")
            .match_header("authorization", "Bearer key")
            .match_header(
                "content-type",
                mockito::Matcher::Regex("^multipart/form-data; boundary=".into()),
            )
            .match_body(mockito::Matcher::AllOf(
                [
                    ("model", model),
                    ("language", "en"),
                    ("prompt", "Names: Ada"),
                    ("temperature", "0"),
                    ("response_format", "json"),
                ]
                .into_iter()
                .map(|(name, value)| {
                    mockito::Matcher::Regex(format!("name=\"{name}\"\\r\\n\\r\\n{value}\\r\\n"))
                })
                .collect(),
            ))
            .match_request(|r| {
                let body = r.body().unwrap();
                let mut file =
                    b"name=\"file\"; filename=\"clip.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
                        .to_vec();
                file.extend([0, 255, 1]);
                file.extend(b"\r\n--");
                body.windows(file.len()).any(|part| part == file)
                    && !String::from_utf8_lossy(body).contains("duration")
            })
            .with_header("content-type", "application/json")
            .with_body(json!({"text":"Ada speaks."}).to_string())
            .expect(2)
            .create_async()
            .await;
        for _ in 0..2 {
            let response = llmshim::transcription(&router, &request).await.unwrap();
            assert_eq!(response.text, "Ada speaks.");
            assert_eq!(
                response.usage["cost_usd"],
                if model == "whisper-1" {
                    json!(0.006)
                } else {
                    json!(null)
                }
            );
        }
        mock.assert_async().await;
    }
}

#[tokio::test]
async fn text_and_json_transcripts_include_silence_and_preserve_errors() {
    let mut server = mockito::Server::new_async().await;
    let router = router(&server);
    for (format, body, valid) in [
        ("text", b"hello\n".to_vec(), true),
        ("text", vec![], true),
        ("text", vec![255], false),
        ("json", br#"{"text":""}"#.to_vec(), true),
        ("json", br#"{"text":7}"#.to_vec(), false),
        ("json", br#"{"error":"bad"}"#.to_vec(), false),
    ] {
        let mock = server
            .mock("POST", "/audio/transcriptions")
            .with_header(
                "content-type",
                if format == "text" {
                    "text/plain"
                } else {
                    "application/json"
                },
            )
            .with_body(body.clone())
            .expect(1)
            .create_async()
            .await;
        let mut request = request("listener");
        request.response_format = Some(format.into());
        let response = llmshim::transcription(&router, &request).await;
        if valid {
            let response = response.unwrap();
            assert_eq!(
                response.text,
                if format == "text" {
                    String::from_utf8(body).unwrap()
                } else {
                    String::new()
                }
            );
            assert!(response.usage["cost_usd"].is_null());
            assert_eq!(response.usage["cost_source"], "unknown");
        } else {
            assert!(response.is_err());
        }
        mock.assert_async().await;
        mock.remove_async().await;
    }
    let wrong_media = server
        .mock("POST", "/audio/transcriptions")
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":"bad"}"#)
        .expect(1)
        .create_async()
        .await;
    let mut text_request = request("listener");
    text_request.response_format = Some("text".into());
    assert!(matches!(
        llmshim::transcription(&router, &text_request).await,
        Err(ShimError::ProviderError { status: 502, .. })
    ));
    wrong_media.assert_async().await;
    wrong_media.remove_async().await;
    let mock = server
        .mock("POST", "/audio/transcriptions")
        .with_status(400)
        .with_body("bad audio")
        .expect(1)
        .create_async()
        .await;
    assert!(
        matches!(llmshim::transcription(&router,&request("listener")).await,Err(ShimError::ProviderError{status:400,body,..}) if body == "bad audio")
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn upload_and_metadata_bounds_refuse_locally_with_exact_boundaries_accepted() {
    let mut server = mockito::Server::new_async().await;
    let router = router(&server);
    let zero = server
        .mock("POST", "/audio/transcriptions")
        .expect(0)
        .create_async()
        .await;
    let mut cases = Vec::new();
    for bytes in [vec![], vec![1; MAX_TRANSCRIPTION_BYTES + 1]] {
        let mut r = request("listener");
        r.bytes = bytes;
        cases.push(r);
    }
    for filename in ["", "a/b.wav", "a\\b.wav", "bad\n.wav", &"a".repeat(256)] {
        let mut r = request("listener");
        r.filename = filename.into();
        cases.push(r);
    }
    for media in ["", "bad\r\nmedia", &"a".repeat(129)] {
        let mut r = request("listener");
        r.media_type = media.into();
        cases.push(r);
    }
    for language in ["e", "eng", "EN", "é"] {
        let mut r = request("listener");
        r.language = Some(language.into());
        cases.push(r);
    }
    let mut r = request("listener");
    r.prompt = Some("a".repeat(16 * 1024 + 1));
    cases.push(r);
    for format in ["verbose_json", "srt"] {
        let mut r = request("listener");
        r.response_format = Some(format.into());
        cases.push(r);
    }
    let mut r = request("openai/gpt-4o-transcribe");
    r.response_format = Some("text".into());
    cases.push(r);
    let mut r = request("openai/gpt-4o-mini-transcribe");
    r.response_format = Some("text".into());
    cases.push(r);
    cases.push(request("openai/gpt-4o"));
    for temperature in [-0.01, 1.01, f64::NAN, f64::INFINITY] {
        let mut r = request("listener");
        r.temperature = Some(temperature);
        cases.push(r);
    }
    for seconds in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let mut r = request("listener");
        r.duration_seconds = Some(seconds);
        cases.push(r);
    }
    for r in &cases {
        let error = llmshim::transcription(&router, r).await.unwrap_err();
        assert!(
            matches!(error, ShimError::ProviderError { status: 400, .. }),
            "{error:?}"
        );
    }
    let oversize = llmshim::transcription(&router, &cases[1])
        .await
        .unwrap_err();
    assert!(matches!(oversize,ShimError::ProviderError{body,..} if body.contains("25 MB")));
    zero.assert_async().await;
    zero.remove_async().await;
    for temperature in [0.0, 1.0] {
        let mock = server
            .mock("POST", "/audio/transcriptions")
            .with_body(r#"{"text":"ok"}"#)
            .expect(1)
            .create_async()
            .await;
        let mut r = request("listener");
        r.temperature = Some(temperature);
        r.bytes = vec![1; MAX_TRANSCRIPTION_BYTES];
        r.filename = "a".repeat(255);
        r.media_type = format!("audio/{}", "a".repeat(122));
        r.prompt = Some("a".repeat(16 * 1024));
        r.language = Some("en".into());
        assert_eq!(
            llmshim::transcription(&router, &r).await.unwrap().text,
            "ok"
        );
        mock.assert_async().await;
        mock.remove_async().await;
    }
    let router = Router::new().register("anthropic", Box::new(Anthropic::new("unused".into())));
    assert!(matches!(
        llmshim::transcription(&router, &request("anthropic/claude")).await,
        Err(ShimError::ProviderError { status: 400, .. })
    ));
}

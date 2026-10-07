use llmshim::{
    audio::TranscriptionRequest, error::ShimError, providers::openrouter::OpenRouter,
    router::Router,
};
use serde_json::{json, Value};

fn router(server: &mockito::ServerGuard) -> Router {
    Router::new().register(
        "openrouter",
        Box::new(OpenRouter::new("or-key".into()).with_base_url(server.url())),
    )
}

fn request(model: &str) -> TranscriptionRequest {
    TranscriptionRequest::new(model, vec![0, 255, 1], "clip.wav", "audio/wav")
}

#[tokio::test]
async fn multipart_upload_carries_the_slug_file_and_bearer_key() {
    let mut server = mockito::Server::new_async().await;
    let router = router(&server);
    // No local model list: a slug the catalog has never seen still goes out.
    for slug in ["openai/whisper-1", "acme/new-speech-to-text"] {
        let mut request = request(&format!("openrouter/{slug}"));
        request.language = Some("en".into());
        request.temperature = Some(0.0);
        request.duration_seconds = Some(60.0);
        let mock = server
            .mock("POST", "/audio/transcriptions")
            .match_header("authorization", "Bearer or-key")
            .match_header(
                "content-type",
                mockito::Matcher::Regex("^multipart/form-data; boundary=".into()),
            )
            .match_body(mockito::Matcher::AllOf(
                [
                    ("model", slug),
                    ("language", "en"),
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
                let text = String::from_utf8_lossy(body);
                body.windows(file.len()).any(|part| part == file)
                    && !text.contains("duration")
                    && !text.contains("name=\"prompt\"")
            })
            .with_header("content-type", "application/json")
            .with_body(json!({"text":"Ada speaks."}).to_string())
            .expect(1)
            .create_async()
            .await;
        let response = llmshim::transcription(&router, &request).await.unwrap();
        assert_eq!(response.text, "Ada speaks.");
        mock.assert_async().await;
        mock.remove_async().await;
    }
}

#[tokio::test]
async fn reported_cost_is_the_bill_and_a_missing_cost_is_unknown() {
    let mut server = mockito::Server::new_async().await;
    let router = router(&server);
    let counts = json!({"seconds":9.2,"input_tokens":83,"output_tokens":30,"total_tokens":113});
    let mut billed = counts.clone();
    billed["cost"] = json!(0.000508);
    let mut negative = counts.clone();
    negative["cost"] = json!(-1);
    for (usage, cost, source) in [
        (billed, json!(0.000508), "provider"),
        (json!({"cost":0.0}), json!(0.0), "provider"),
        // A catalog-priced OpenAI model name is still not estimated here, even
        // with a caller duration: OpenRouter's bill is the only source.
        (counts.clone(), Value::Null, "unknown"),
        (negative, Value::Null, "unknown"),
        (Value::Null, Value::Null, "unknown"),
    ] {
        let mut body = json!({"text":"hello"});
        if !usage.is_null() {
            body["usage"] = usage.clone();
        }
        let mock = server
            .mock("POST", "/audio/transcriptions")
            .with_header("content-type", "application/json")
            .with_body(body.to_string())
            .expect(1)
            .create_async()
            .await;
        let mut request = request("openrouter/openai/whisper-1");
        request.duration_seconds = Some(30.0);
        let response = llmshim::transcription(&router, &request).await.unwrap();
        assert_eq!(response.usage["cost_usd"], cost, "{usage}");
        assert_eq!(response.usage["cost_source"], source, "{usage}");
        for field in ["seconds", "input_tokens", "output_tokens", "total_tokens"] {
            assert_eq!(response.usage[field], usage[field], "{field}");
        }
        assert!(response.usage.get("caller_duration_seconds").is_none());
        mock.assert_async().await;
        mock.remove_async().await;
    }
}

#[tokio::test]
async fn unsupported_controls_refuse_locally_and_status_errors_pass_through() {
    let mut server = mockito::Server::new_async().await;
    let router = router(&server);
    let zero = server
        .mock("POST", "/audio/transcriptions")
        .expect(0)
        .create_async()
        .await;
    let mut text = request("openrouter/openai/whisper-1");
    text.response_format = Some("text".into());
    let mut prompt = request("openrouter/openai/whisper-1");
    prompt.prompt = Some("Names: Ada".into());
    let mut empty = request("openrouter/openai/whisper-1");
    empty.bytes.clear();
    for request in [text, prompt, empty] {
        assert!(matches!(
            llmshim::transcription(&router, &request).await,
            Err(ShimError::ProviderError { status: 400, .. })
        ));
    }
    zero.assert_async().await;
    zero.remove_async().await;
    for (status, message) in [(400, "unsupported audio"), (402, "insufficient credits")] {
        let mock = server
            .mock("POST", "/audio/transcriptions")
            .with_status(status)
            .with_body(message)
            .expect(1)
            .create_async()
            .await;
        let error = llmshim::transcription(&router, &request("openrouter/openai/whisper-1"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ShimError::ProviderError { status: s, body, .. } if *s == status as u16 && body == message),
            "{error:?}"
        );
        mock.assert_async().await;
        mock.remove_async().await;
    }
    let no_text = server
        .mock("POST", "/audio/transcriptions")
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":{"message":"bad","code":400}}"#)
        .expect(1)
        .create_async()
        .await;
    assert!(matches!(
        llmshim::transcription(&router, &request("openrouter/openai/whisper-1")).await,
        Err(ShimError::ProviderError { status: 502, .. })
    ));
    no_text.assert_async().await;
}

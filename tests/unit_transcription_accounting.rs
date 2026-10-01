use llmshim::{audio::TranscriptionRequest, providers::openai::OpenAi, router::Router};
use serde_json::{json, Value};

#[tokio::test]
async fn transcription_prices_native_tokens_and_provider_bill_overrides_catalog() {
    let mut server = mockito::Server::new_async().await;
    let router = Router::new().register(
        "openai",
        Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
    );
    let known = json!({"type":"tokens","input_tokens":14,"input_token_details":{"audio_tokens":12,"text_tokens":2},"output_tokens":45,"total_tokens":59});
    for (model, price) in [
        ("gpt-4o-transcribe", 0.000527),
        ("gpt-4o-mini-transcribe", 0.0002635),
    ] {
        for (field, value, cost) in [
            ("extra", json!(true), json!(price)),
            ("cost", json!(0.25), json!(0.25)),
            ("cost", json!(0.0), json!(0.0)),
            ("cost", json!(-1), json!(price)),
            (
                "input_token_details",
                json!({"audio_tokens":2,"text_tokens":12}),
                json!(
                    price
                        - if model == "gpt-4o-transcribe" {
                            0.000035
                        } else {
                            0.0000175
                        }
                ),
            ),
            ("input_tokens", json!(15), Value::Null),
            ("input_tokens", json!(-1), Value::Null),
            ("input_token_details", Value::Null, Value::Null),
            (
                "input_token_details",
                json!({"audio_tokens":13,"text_tokens":2}),
                Value::Null,
            ),
            (
                "input_token_details",
                json!({"audio_tokens":18446744073709551615_u64,"text_tokens":2}),
                Value::Null,
            ),
            ("total_tokens", json!(60), Value::Null),
            ("total_tokens", Value::Null, Value::Null),
            ("output_tokens", json!("45"), Value::Null),
            ("type", json!("duration"), Value::Null),
        ] {
            let mut usage = known.clone();
            usage[field] = value;
            let mock = server
                .mock("POST", "/audio/transcriptions")
                .with_body(json!({"text":"hello","usage":usage}).to_string())
                .expect(1)
                .create_async()
                .await;
            let r = TranscriptionRequest::new(
                format!("openai/{model}"),
                vec![1],
                "clip.wav",
                "audio/wav",
            );
            let response = llmshim::transcription(&router, &r).await.unwrap();
            assert_eq!(response.usage["cost_usd"], cost, "{usage}");
            assert_eq!(response.usage["extra"], usage["extra"]);
            assert_eq!(
                response.usage["input_token_details"],
                usage["input_token_details"]
            );
            assert_eq!(
                response.usage["cost_source"],
                if field == "cost" && usage["cost"].as_f64().unwrap() >= 0.0 {
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
    for (duration, usage, cost, source) in [
        (None, json!({}), Value::Null, "unknown"),
        (Some(30.0), json!({}), json!(0.003), "catalog"),
        (Some(30.0), json!({"cost":0.125}), json!(0.125), "provider"),
        (
            None,
            json!({"input_tokens":10,"output_tokens":10}),
            Value::Null,
            "unknown",
        ),
    ] {
        let mock = server
            .mock("POST", "/audio/transcriptions")
            .with_body(json!({"text":"hello","usage":usage}).to_string())
            .expect(1)
            .create_async()
            .await;
        let mut r = TranscriptionRequest::new("openai/whisper-1", vec![1], "clip.wav", "audio/wav");
        r.duration_seconds = duration;
        let response = llmshim::transcription(&router, &r).await.unwrap();
        assert_eq!(response.usage["cost_usd"], cost);
        assert_eq!(response.usage["cost_source"], source);
        assert_eq!(
            response
                .usage
                .get("caller_duration_seconds")
                .and_then(Value::as_f64),
            duration
        );
        mock.assert_async().await;
        mock.remove_async().await;
    }
}

#[test]
fn native_transcription_response_preserves_unknown_prices_and_rejects_missing_text() {
    use llmshim::provider::Provider;
    let provider = OpenAi::new("unused".into());
    let mut request = TranscriptionRequest::new("whisper-1", vec![1], "clip.wav", "audio/wav");
    let response = provider.transcription_response("unknown",&request,json!({"text":"hello","usage":{"type":"tokens","input_tokens":1,"input_token_details":{"audio_tokens":1,"text_tokens":0},"output_tokens":1,"total_tokens":2}})).unwrap();
    assert_eq!(response.text, "hello");
    assert!(response.usage["cost_usd"].is_null());
    assert!(provider
        .transcription_response("whisper-1", &request, json!({}))
        .is_err());
    request.duration_seconds = Some(-1.0);
    assert!(provider
        .transcription_response("whisper-1", &request, json!({"text":"hello"}))
        .unwrap()
        .usage["cost_usd"]
        .is_null());
}

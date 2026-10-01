use super::{body, AttemptKind, DispatchFailure, ShimClient};
use crate::{audio::SpeechResponse, error::Result, provider::Provider};
use serde_json::Value;

impl ShimClient {
    pub async fn transcription(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &crate::audio::TranscriptionRequest,
    ) -> Result<crate::audio::TranscriptionResponse> {
        crate::audio::validate_transcription(request)?;
        let upload = provider.transcription_request(model, request)?;
        let result = async {
            let attempt = self
                .send_prepared_with_body(
                    None,
                    AttemptKind::Completion,
                    model,
                    None,
                    &upload.request,
                    |builder| Ok(builder.multipart(multipart_form(&upload)?)),
                )
                .await?;
            let response = if upload.request.body["response_format"] == "text" {
                let media_type = media_type(&attempt.response);
                if !media_type.eq_ignore_ascii_case("text/plain") {
                    return Err(DispatchFailure::Upstream(crate::error::provider_error(
                        502,
                        "unexpected transcription text media type",
                    )));
                }
                let bytes = body::read(
                    attempt.response,
                    self.response_body_limits.success_bytes,
                    self.deadlines.unary_body_idle,
                    attempt.attempt_deadline,
                )
                .await
                .map_err(body::BodyReadError::into_dispatch_failure)?;
                let text = String::from_utf8(bytes).map_err(|_| {
                    DispatchFailure::Upstream(crate::error::provider_error(
                        502,
                        "transcription text is not UTF-8",
                    ))
                })?;
                serde_json::json!({"text": text})
            } else {
                body::read_json(
                    attempt.response,
                    self.response_body_limits.success_bytes,
                    self.deadlines.unary_body_idle,
                    attempt.attempt_deadline,
                )
                .await
                .map_err(body::BodyReadError::into_dispatch_failure)?
            };
            provider
                .transcription_response(model, request, response)
                .map_err(DispatchFailure::Upstream)
        }
        .await;
        if let Some(outcome) = Self::breaker_outcome(&result) {
            self.observe(provider, outcome).await;
        }
        result.map_err(DispatchFailure::into_public)
    }

    pub async fn speech(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &Value,
    ) -> Result<SpeechResponse> {
        crate::audio::validate(request)?;
        let prepared = provider.speech_request(model, request)?;
        let result = async {
            let attempt = self
                .send_prepared(None, AttemptKind::Completion, model, None, &prepared)
                .await?;
            let media_type = media_type(&attempt.response).to_owned();
            let bytes = body::read(
                attempt.response,
                self.response_body_limits.success_bytes,
                self.deadlines.unary_body_idle,
                attempt.attempt_deadline,
            )
            .await
            .map_err(body::BodyReadError::into_dispatch_failure)?;
            provider
                .speech_response(model, &prepared.body, bytes, &media_type)
                .map_err(DispatchFailure::Upstream)
        }
        .await;
        if let Some(outcome) = Self::breaker_outcome(&result) {
            self.observe(provider, outcome).await;
        }
        result.map_err(DispatchFailure::into_public)
    }
}

fn media_type(response: &reqwest::Response) -> &str {
    response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
}

fn multipart_form(upload: &crate::audio::TranscriptionUpload) -> Result<reqwest::multipart::Form> {
    let part = reqwest::multipart::Part::stream_with_length(
        upload.bytes.clone(),
        upload.bytes.len() as u64,
    )
    .file_name(upload.filename.clone())
    .mime_str(&upload.media_type)
    .map_err(|_| crate::error::provider_error(400, "invalid transcription media type"))?;
    let mut form = reqwest::multipart::Form::new().part("file", part);
    let fields = upload.request.body.as_object().ok_or_else(|| {
        crate::error::provider_error(400, "transcription multipart fields must be strings")
    })?;
    for (key, value) in fields {
        let value = value.as_str().ok_or_else(|| {
            crate::error::provider_error(400, "transcription multipart fields must be strings")
        })?;
        form = form.text(key.clone(), value.to_owned());
    }
    Ok(form)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        breaker::{BreakerConfig, ProviderBreaker},
        error::ShimError,
        providers::openai::OpenAi,
    };
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn speech_body_limit_is_local_and_valid_binary_preserves_health() {
        let mut server = mockito::Server::new_async().await;
        let provider = OpenAi::new("key".into()).with_base_url(server.url());
        for (bytes, expected_error, unhealthy) in [
            (vec![1, 2], false, false),
            (vec![1, 2, 3], true, false),
            (vec![], true, true),
        ] {
            let breaker = Arc::new(ProviderBreaker::with_config(BreakerConfig {
                trip_threshold: 1,
                ..BreakerConfig::default()
            }));
            let client = ShimClient {
                response_body_limits: body::ResponseBodyLimits {
                    success_bytes: 2,
                    ..body::ResponseBodyLimits::default()
                },
                ..ShimClient::new().with_breaker(breaker.clone())
            };
            let mock = server
                .mock("POST", "/audio/speech")
                .with_header("content-type", "audio/mpeg")
                .with_body(bytes)
                .expect(1)
                .create_async()
                .await;
            let response = client
                .speech(
                    &provider,
                    "tts-1",
                    &json!({"input":"hello","voice":"alloy"}),
                )
                .await;
            if expected_error {
                assert!(matches!(
                    response,
                    Err(ShimError::ProviderError { status: 502, .. })
                ));
            } else {
                assert_eq!(response.unwrap().bytes, [1, 2]);
            }
            assert_eq!(breaker.should_skip("openai"), unhealthy);
            mock.assert_async().await;
        }
    }
    #[tokio::test]
    async fn speech_retries_preserve_the_prepared_body() {
        let mut server = mockito::Server::new_async().await;
        let provider = OpenAi::new("key".into()).with_base_url(server.url());
        let prepared =
            json!({"model":"tts-1","input":"hello","voice":"alloy","response_format":"mp3"});
        let retry = server
            .mock("POST", "/audio/speech")
            .match_body(mockito::Matcher::Json(prepared.clone()))
            .with_status(503)
            .with_header("retry-after", "0")
            .with_body("retry")
            .expect(1)
            .create_async()
            .await;
        let success = server
            .mock("POST", "/audio/speech")
            .match_body(mockito::Matcher::Json(prepared))
            .with_header("content-type", "audio/mpeg")
            .with_body([1])
            .expect(1)
            .create_async()
            .await;
        let client = ShimClient {
            retry: super::super::RetryConfig {
                max_retries: 1,
                ..super::super::RetryConfig::default()
            },
            ..ShimClient::new()
        };
        let response = client
            .speech(
                &provider,
                "tts-1",
                &json!({"input":"hello","voice":"alloy"}),
            )
            .await
            .unwrap();
        assert_eq!(response.bytes, [1]);
        assert_eq!(response.usage["input_characters"], 5);
        retry.assert_async().await;
        success.assert_async().await;
    }
    #[tokio::test]
    async fn transcription_retries_rebuild_the_file_and_fields() {
        let mut server = mockito::Server::new_async().await;
        let provider = OpenAi::new("key".into()).with_base_url(server.url());
        let matches = |r: &mockito::Request| {
            let body = r.body().unwrap();
            body.windows(6).any(|w| w == [0, 255, 1, 13, 10, 45])
                && String::from_utf8_lossy(body).contains("name=\"model\"\r\n\r\nwhisper-1")
        };
        let retry = server
            .mock("POST", "/audio/transcriptions")
            .match_request(matches)
            .with_status(503)
            .with_header("retry-after", "0")
            .expect(1)
            .create_async()
            .await;
        let success = server
            .mock("POST", "/audio/transcriptions")
            .match_request(matches)
            .with_body(r#"{"text":"hello"}"#)
            .expect(1)
            .create_async()
            .await;
        let client = ShimClient {
            retry: super::super::RetryConfig {
                max_retries: 1,
                ..super::super::RetryConfig::default()
            },
            ..ShimClient::new()
        };
        let request = crate::audio::TranscriptionRequest::new(
            "whisper-1",
            vec![0, 255, 1],
            "clip.wav",
            "audio/wav",
        );
        assert_eq!(
            client
                .transcription(&provider, "whisper-1", &request)
                .await
                .unwrap()
                .text,
            "hello"
        );
        retry.assert_async().await;
        success.assert_async().await;
    }

    #[tokio::test]
    async fn transcription_json_and_text_body_limits_preserve_breaker_health() {
        let mut server = mockito::Server::new_async().await;
        let provider = OpenAi::new("key".into()).with_base_url(server.url());
        for (format, body, valid) in [
            ("json", r#"{"text":""}"#, true),
            ("json", r#"{"text":"a"}"#, false),
            ("text", "hello world", true),
            ("text", "hello worlds", false),
        ] {
            let breaker = Arc::new(ProviderBreaker::with_config(BreakerConfig {
                trip_threshold: 1,
                ..BreakerConfig::default()
            }));
            let client = ShimClient {
                response_body_limits: body::ResponseBodyLimits {
                    success_bytes: 11,
                    ..body::ResponseBodyLimits::default()
                },
                ..ShimClient::new().with_breaker(breaker.clone())
            };
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
                .with_body(body)
                .expect(1)
                .create_async()
                .await;
            let mut request = crate::audio::TranscriptionRequest::new(
                "whisper-1",
                vec![1],
                "clip.wav",
                "audio/wav",
            );
            request.response_format = Some(format.into());
            let response = client.transcription(&provider, "whisper-1", &request).await;
            if valid {
                assert_eq!(
                    response.unwrap().text,
                    if format == "text" { body } else { "" }
                );
            } else {
                assert!(
                    matches!(response,Err(ShimError::ProviderError{status:502,body,..}) if body=="upstream response body exceeds size limit")
                );
            }
            assert!(!breaker.should_skip("openai"));
            mock.assert_async().await;
            mock.remove_async().await;
        }
    }

    #[tokio::test]
    async fn transcription_body_idle_and_total_deadlines_stop_stalled_responses() {
        use std::time::Duration;
        let mut server = mockito::Server::new_async().await;
        let provider = OpenAi::new("key".into()).with_base_url(server.url());
        for format in ["json", "text"] {
            for idle in [true, false] {
                let mut client = ShimClient::new();
                client.deadlines.unary_body_idle = if idle {
                    Duration::from_millis(10)
                } else {
                    Duration::from_secs(1)
                };
                client.deadlines.unary_attempt_total = if idle {
                    Duration::from_secs(1)
                } else {
                    Duration::from_millis(10)
                };
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
                    .with_chunked_body(move |writer| {
                        writer.write_all(if format == "json" { b"{ " } else { b"he" })?;
                        writer.flush()?;
                        std::thread::sleep(Duration::from_millis(100));
                        writer.write_all(if format == "json" {
                            b"\"text\":\"hello\"}"
                        } else {
                            b"llo"
                        })
                    })
                    .expect(1)
                    .create_async()
                    .await;
                let mut request = crate::audio::TranscriptionRequest::new(
                    "whisper-1",
                    vec![1],
                    "clip.wav",
                    "audio/wav",
                );
                request.response_format = Some(format.into());
                assert!(matches!(
                    client.transcription(&provider, "whisper-1", &request).await,
                    Err(ShimError::ProviderError { status: 504, .. })
                ));
                mock.assert_async().await;
                mock.remove_async().await;
            }
        }
    }
}

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
        self.transcription_dispatch(provider, model, request, None)
            .await
    }

    /// [`Self::transcription`] under a trusted attempt policy (note 0038): the attempt is admitted
    /// by the policy before it is sent and reported to it as it ends, with the usage the adapter
    /// read (tokens or seconds) and its own price (null when unknown).
    pub async fn transcription_with_policy(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &crate::audio::TranscriptionRequest,
        policy_context: &crate::policy::DispatchPolicyContext,
    ) -> Result<crate::audio::TranscriptionResponse> {
        self.transcription_dispatch(provider, model, request, Some(policy_context))
            .await
    }

    async fn transcription_dispatch(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &crate::audio::TranscriptionRequest,
        policy_context: Option<&crate::policy::DispatchPolicyContext>,
    ) -> Result<crate::audio::TranscriptionResponse> {
        crate::audio::validate_transcription(request)?;
        let upload = provider
            .prepare_transcription_request(model, request)
            .await?;
        let target = policy_context.map(|_| provider.request_replay_target(model, &upload.request));
        let result = async {
            let super::AttemptResponse {
                response: upstream,
                mut tracker,
                attempt_deadline,
            } = self
                .send_prepared_with_body(
                    policy_context,
                    AttemptKind::Completion,
                    model,
                    target.as_ref(),
                    &upload.request,
                    None,
                    |builder| Ok(builder.multipart(multipart_form(&upload)?)),
                )
                .await?;
            let read = async {
                if upload.request.body["response_format"] == "text" {
                    let media_type = media_type(&upstream);
                    if !media_type.eq_ignore_ascii_case("text/plain") {
                        return Err(DispatchFailure::Upstream(crate::error::provider_error(
                            502,
                            "unexpected transcription text media type",
                        )));
                    }
                    let bytes = body::read(
                        upstream,
                        self.response_body_limits.success_bytes,
                        self.deadlines.unary_body_idle,
                        attempt_deadline,
                    )
                    .await
                    .map_err(body::BodyReadError::into_dispatch_failure)?;
                    let text = String::from_utf8(bytes).map_err(|_| {
                        DispatchFailure::Upstream(crate::error::provider_error(
                            502,
                            "transcription text is not UTF-8",
                        ))
                    })?;
                    Ok(serde_json::json!({"text": text}))
                } else {
                    body::read_json(
                        upstream,
                        self.response_body_limits.success_bytes,
                        self.deadlines.unary_body_idle,
                        attempt_deadline,
                    )
                    .await
                    .map_err(body::BodyReadError::into_dispatch_failure)
                }
            }
            .await;
            let native = match read {
                Ok(native) => native,
                Err(error) => {
                    super::finish_invalid_response(
                        &mut tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    return Err(error);
                }
            };
            let transcript = match provider.transcription_response(model, request, native) {
                Ok(transcript) => transcript,
                Err(error) => {
                    super::finish_invalid_response(
                        &mut tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    return Err(DispatchFailure::Upstream(error));
                }
            };
            if let Some(tracker) = tracker.as_mut() {
                // The counters are the provider's own (`input_tokens` / `output_tokens`, or only
                // seconds for a per-minute model); the price is the adapter's, null when unknown.
                let usage = &transcript.usage;
                let tokens = |key: &str| usage.get(key).and_then(Value::as_u64);
                let complete =
                    tokens("input_tokens").is_some() && tokens("output_tokens").is_some();
                let frame = serde_json::json!({
                    "prompt_tokens": tokens("input_tokens").unwrap_or(0),
                    "completion_tokens": tokens("output_tokens").unwrap_or(0),
                    "cost_usd": usage.get("cost_usd").cloned().unwrap_or(Value::Null),
                    "cost_source": super::images::known_cost_source(usage),
                });
                let observed = tracker.usage(&frame, true, complete, false);
                super::bounded_policy(observed, self.deadlines.policy_callback, attempt_deadline)
                    .await
                    .map_err(|_| super::coordinator_timeout_observation())?
                    .map_err(DispatchFailure::PolicyObservation)?;
            }
            super::finish_completed_response(
                &mut tracker,
                self.deadlines.policy_callback,
                attempt_deadline,
            )
            .await?;
            Ok(transcript)
        }
        .await;
        if let Some(outcome) = Self::breaker_outcome(&result) {
            self.observe(provider, outcome).await;
        }
        result.map_err(DispatchFailure::into_public)
    }

    /// [`Self::speech`] under a trusted attempt policy (note 0037). The binary speech endpoint
    /// returns no usage, but its price is fixed by the request (characters at the catalog's rate), so
    /// the attempt is reported with that exact figure and its character count.
    pub async fn speech_with_policy(
        &self,
        provider: &dyn Provider,
        model: &str,
        request: &Value,
        policy_context: &crate::policy::DispatchPolicyContext,
    ) -> Result<SpeechResponse> {
        crate::audio::validate(request)?;
        let prepared = provider.speech_request(model, request)?;
        let target = provider.request_replay_target(model, &prepared);
        let result = async {
            let super::AttemptResponse {
                response,
                mut tracker,
                attempt_deadline,
            } = self
                .send_prepared(
                    Some(policy_context),
                    AttemptKind::Completion,
                    model,
                    Some(&target),
                    &prepared,
                    provider.transport().as_deref(),
                )
                .await?;
            let media_type = media_type(&response).to_owned();
            let bytes = match body::read(
                response,
                self.response_body_limits.success_bytes,
                self.deadlines.unary_body_idle,
                attempt_deadline,
            )
            .await
            {
                Ok(bytes) => bytes,
                Err(error) => {
                    super::finish_invalid_response(
                        &mut tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    return Err(error.into_dispatch_failure());
                }
            };
            let speech = match provider.speech_response(model, &prepared.body, bytes, &media_type) {
                Ok(speech) => speech,
                Err(error) => {
                    super::finish_invalid_response(
                        &mut tracker,
                        self.deadlines.policy_callback,
                        attempt_deadline,
                    )
                    .await?;
                    return Err(DispatchFailure::Upstream(error));
                }
            };
            if let Some(tracker) = tracker.as_mut() {
                // No token counters exist; the characters and their fixed price are the whole bill,
                // so the counters are complete and the price is the catalog's exact one.
                let usage = serde_json::json!({
                    "prompt_tokens": 0,
                    "completion_tokens": 0,
                    "input_characters": speech.usage["input_characters"],
                    "cost_usd": speech.usage["cost_usd"],
                    "cost_source": super::images::known_cost_source(&speech.usage),
                });
                let observed = tracker.usage(&usage, true, true, false);
                super::bounded_policy(observed, self.deadlines.policy_callback, attempt_deadline)
                    .await
                    .map_err(|_| super::coordinator_timeout_observation())?
                    .map_err(DispatchFailure::PolicyObservation)?;
            }
            super::finish_completed_response(
                &mut tracker,
                self.deadlines.policy_callback,
                attempt_deadline,
            )
            .await?;
            Ok(speech)
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
                .send_prepared(None, AttemptKind::Completion, model, None, &prepared, None)
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
        for format in ["json", "text"] {
            for idle in [true, false] {
                // A server of its own for each case: the previous case's writer is
                // still sleeping on its thread after the client gave up, and a
                // shared server would make the next case's headers wait behind it.
                let mut server = mockito::Server::new_async().await;
                let provider = OpenAi::new("key".into()).with_base_url(server.url());
                let mut client = ShimClient::new();
                // The stalled body sleeps 5 s. The deadline under test is 1.5 s
                // and the other is far beyond it. The total deadline also covers
                // connecting and the response headers, so a short one (10 ms, then
                // 150 ms) expired before the body began on a slow runner, which is
                // a retried failure: two requests, not the one this expects. 1.5 s
                // leaves a margin of seconds-class slowness for the headers.
                client.deadlines.unary_body_idle = if idle {
                    Duration::from_millis(1500)
                } else {
                    Duration::from_secs(30)
                };
                client.deadlines.unary_attempt_total = if idle {
                    Duration::from_secs(30)
                } else {
                    Duration::from_millis(1500)
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
                        std::thread::sleep(Duration::from_millis(5000));
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
            }
        }
    }
}

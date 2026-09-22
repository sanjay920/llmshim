use super::convert;
use super::error::ApiError;
use super::types::*;
use super::AppState;
use crate::log::RequestTimer;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;

fn dispatch_error(error: crate::error::ShimError, overload_wait: std::time::Duration) -> ApiError {
    match &error {
        crate::error::ShimError::ProviderError {
            status: 429,
            retry_after: Some(wait),
            ..
        } => ApiError::RateLimited(*wait),
        crate::error::ShimError::ProviderError {
            status: 503, body, ..
        } if body == "attempt policy coordinator unavailable" => {
            ApiError::Overloaded(overload_wait)
        }
        _ => ApiError::from(error),
    }
}

/// POST /v1/chat — non-streaming completion (or streaming if stream=true)
pub async fn chat(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<Response, ApiError> {
    if req.stream {
        // Delegate to streaming (which runs its own admission control).
        return Ok(chat_stream_inner(state, req).await);
    }
    let prepared_request = convert::prepare_request(&state.router, &req)?;
    if let Some(fallback_models) = &req.fallback {
        convert::validate_resolvable_fallbacks(
            &state.router,
            &prepared_request.payload,
            fallback_models,
        )?;
    }

    let timer = RequestTimer::start();
    let value = prepared_request.payload;
    let policy_context = super::attempt::context(state.limiter.clone(), state.backpressure.clone());

    let result = if let Some(fallback_models) = &req.fallback {
        // Build fallback chain: primary model + fallback models
        let mut models = vec![req.model.clone()];
        models.extend(fallback_models.iter().cloned());
        let config = crate::FallbackConfig::new(models);
        crate::completion_with_fallback_and_policy(
            &state.router,
            &value,
            &config,
            state.logger.as_ref(),
            &policy_context,
        )
        .await
    } else {
        crate::completion_with_logger_and_policy(
            &state.router,
            &value,
            state.logger.as_ref(),
            &policy_context,
        )
        .await
    };

    let resp = match result {
        Ok(resp) => resp,
        Err(error) => return Err(dispatch_error(error, state.backpressure.queue_timeout())),
    };

    // Resolve provider name from the response (it may have fallen back to a different model)
    let actual_model = resp
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(&req.model);
    let provider_name = state
        .router
        .resolve(actual_model)
        .or_else(|_| state.router.resolve(&req.model))
        .map(|(p, _)| p.name().to_string())
        .unwrap_or_default();

    let elapsed = timer.elapsed();
    let chat_resp = convert::value_to_response(&resp, &provider_name, elapsed.as_millis() as u64);

    Ok(Json(chat_resp).into_response())
}

/// POST /v1/chat/stream — always streaming SSE
pub async fn chat_stream(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Response {
    chat_stream_inner(state, req).await
}

async fn chat_stream_inner(state: Arc<AppState>, req: ChatRequest) -> Response {
    let prepared_request = match convert::prepare_request(&state.router, &req) {
        Ok(prepared_request) => prepared_request,
        Err(error) => return ApiError::from(error).into_response(),
    };
    let value = prepared_request.payload;
    let policy_context = super::attempt::context(state.limiter.clone(), state.backpressure.clone());
    let mut upstream_stream =
        match crate::stream_with_policy(&state.router, &value, &policy_context).await {
            Ok(stream) => stream,
            Err(error) => {
                return dispatch_error(error, state.backpressure.queue_timeout()).into_response()
            }
        };

    let event_stream = async_stream::stream! {
        while let Some(chunk) = upstream_stream.next().await {
                    match chunk {
                        Ok(chunk_json) => {
                            let events = convert::chunk_to_events(&chunk_json);
                            for event in events {
                                let event_type = match &event {
                                    StreamEvent::Content { .. } => "content",
                                    StreamEvent::Reasoning { .. } => "reasoning",
                                    StreamEvent::ToolCall { .. } => "tool_call",
                                    StreamEvent::Usage(_) => "usage",
                                    StreamEvent::Done { .. } => "done",
                                    StreamEvent::Error { .. } => "error",
                                };
                                if let Ok(data) = serde_json::to_string(&event) {
                                    yield Ok(Event::default().event(event_type).data(data));
                                }
                            }
                        }
                        Err(e) => {
                            let error_event = super::error::stream_error(&e.to_string());
                            if let Ok(data) = serde_json::to_string(&error_event) {
                                yield Ok(Event::default().event("error").data(data));
                            }
                            break;
                        }
                    }
        }
    };

    // Pin the stream's item type (all yields are `Ok`, so `E` is otherwise
    // ambiguous) before handing it to `Sse`.
    fn pin_item<S: futures::Stream<Item = Result<Event, Infallible>>>(s: S) -> S {
        s
    }
    Sse::new(pin_item(event_stream)).into_response()
}

/// GET /v1/models — list available models in both the OpenAI list shape
/// (`object`/`data`, which is what an OpenAI SDK's `models.list()` parses) and
/// llmshim's own `models` array, which existing clients keep reading.
pub async fn list_models(State(state): State<Arc<AppState>>) -> Json<ModelsResponse> {
    Json(convert::models_response(&state.router.provider_keys()))
}

/// GET /health — health check
pub async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let providers = state
        .router
        .provider_keys()
        .into_iter()
        .map(String::from)
        .collect();

    Json(HealthResponse {
        status: "ok".to_string(),
        providers,
    })
}

// ===========================================================================
// Handler integration tests — axum in-process (oneshot), NO network.
//
// These drive the real `app_with_state` router. Every rejection path is
// exercised *before* any upstream dispatch (backpressure timeout, or a
// pre-penalized rate limiter), so no test ever contacts a provider API or
// needs an external service.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::super::origin::OriginPolicy;
    use super::super::ratelimit::{
        Backpressure, InMemoryRateLimiter, RateKey, RateLimitConfig, RateLimiter,
    };
    use super::super::{app_with_origin_policy, app_with_state, AppState};
    use crate::router::Router;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt; // for `oneshot`

    fn chat_body(model: &str) -> Body {
        Body::from(
            serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        )
    }

    async fn post_chat(app: axum::Router, path: &str, model: &str) -> axum::http::Response<Body> {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(chat_body(model))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    async fn post_json(
        app: axum::Router,
        path: &str,
        payload: serde_json::Value,
    ) -> axum::http::Response<Body> {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    struct RecordingLimiter {
        penalized: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl RateLimiter for RecordingLimiter {
        async fn acquire(
            &self,
            _key: &RateKey,
            _permits: u32,
        ) -> Result<(), super::super::ratelimit::RetryAfter> {
            Ok(())
        }

        async fn penalize(&self, key: &RateKey, _retry_after: Duration) {
            self.penalized.lock().unwrap().push(key.provider.clone());
        }
    }

    fn compatible_response(content: serde_json::Value) -> String {
        serde_json::json!({
            "id": "response",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
        .to_string()
    }

    #[tokio::test]
    async fn provider_rpm_one_allows_exactly_one_real_send() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_status(500)
            .with_body("retryable")
            .expect(1)
            .create_async()
            .await;
        let state = Arc::new(AppState {
            router: Router::new().register(
                "local",
                Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                    "local",
                    server.url(),
                    None,
                )),
            ),
            logger: None,
            limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::with_global(
                Some(1),
                None,
            ))),
            backpressure: Backpressure::new(8, Duration::from_secs(1)),
        });
        let response = post_chat(app_with_state(state), "/v1/chat", "local/test").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn repair_requires_a_fresh_provider_gate() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_body(compatible_response(serde_json::json!("1")))
            .expect(1)
            .create_async()
            .await;
        let state = Arc::new(AppState {
            router: Router::new().register(
                "local",
                Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                    "local",
                    server.url(),
                    None,
                )),
            ),
            logger: None,
            limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::with_global(
                Some(1),
                None,
            ))),
            backpressure: Backpressure::new(8, Duration::from_secs(1)),
        });
        let response = post_json(
            app_with_state(state),
            "/v1/chat",
            serde_json::json!({
                "model": "local/test",
                "messages": [{"role": "user", "content": "answer"}],
                "response_format": {
                    "type": "json_schema",
                    "json_schema": {
                        "name": "answer",
                        "schema": {"type": "integer", "minimum": 3}
                    }
                },
                "x-shim": {"structured_output": "prompt"}
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn fallback_charges_the_actual_provider_lane() {
        let mut primary_server = mockito::Server::new_async().await;
        let primary = primary_server
            .mock("POST", "/chat/completions")
            .with_status(500)
            .with_body("retryable")
            .expect(1)
            .create_async()
            .await;
        let mut secondary_server = mockito::Server::new_async().await;
        let secondary = secondary_server
            .mock("POST", "/chat/completions")
            .with_body(compatible_response(serde_json::json!("ok")))
            .expect(1)
            .create_async()
            .await;
        let router = Router::new()
            .register(
                "primary",
                Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                    "primary",
                    primary_server.url(),
                    None,
                )),
            )
            .register(
                "secondary",
                Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                    "secondary",
                    secondary_server.url(),
                    None,
                )),
            );
        let state = Arc::new(AppState {
            router,
            logger: None,
            limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::with_global(
                Some(1),
                None,
            ))),
            backpressure: Backpressure::new(8, Duration::from_secs(1)),
        });
        let response = post_json(
            app_with_state(state.clone()),
            "/v1/chat",
            serde_json::json!({
                "model": "primary/test",
                "messages": [{"role": "user", "content": "hi"}],
                "fallback": ["secondary/test"]
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let direct_secondary = post_chat(app_with_state(state), "/v1/chat", "secondary/test").await;
        assert_eq!(direct_secondary.status(), StatusCode::TOO_MANY_REQUESTS);
        primary.assert_async().await;
        secondary.assert_async().await;
    }

    #[tokio::test]
    async fn intermediate_429_penalizes_its_actual_provider_before_success() {
        let mut server = mockito::Server::new_async().await;
        let rate_limited = server
            .mock("POST", "/chat/completions")
            .with_status(429)
            .with_header("retry-after", "0")
            .with_body("slow down")
            .expect(1)
            .create_async()
            .await;
        let successful = server
            .mock("POST", "/chat/completions")
            .with_body(compatible_response(serde_json::json!("ok")))
            .expect(1)
            .create_async()
            .await;
        let limiter = Arc::new(RecordingLimiter {
            penalized: std::sync::Mutex::new(Vec::new()),
        });
        let state = Arc::new(AppState {
            router: Router::new().register(
                "actual",
                Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                    "actual",
                    server.url(),
                    None,
                )),
            ),
            logger: None,
            limiter: limiter.clone(),
            backpressure: Backpressure::new(8, Duration::from_secs(1)),
        });
        let response = post_chat(app_with_state(state), "/v1/chat", "actual/test").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(limiter.penalized.lock().unwrap().as_slice(), ["actual"]);
        rate_limited.assert_async().await;
        successful.assert_async().await;
    }

    #[tokio::test]
    async fn backpressure_timeout_returns_503_with_retry_after() {
        // Cap of 1, and we hold the only permit → the request's admission times
        // out and sheds load. No provider is ever contacted.
        let bp = Backpressure::new(1, Duration::from_millis(50));
        let _held = bp.acquire().await.expect("hold the only permit");

        let state = Arc::new(AppState {
            router: Router::new().register(
                "openai",
                Box::new(crate::providers::openai::OpenAi::new("test-key".into())),
            ),
            logger: None,
            limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default())),
            backpressure: bp.clone(),
        });

        let resp = post_chat(app_with_state(state), "/v1/chat", "openai/gpt-5.5").await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            resp.headers().contains_key(axum::http::header::RETRY_AFTER),
            "503 must carry Retry-After"
        );
    }

    #[tokio::test]
    async fn rate_limited_returns_429_with_retry_after() {
        // A registered provider so the RateKey resolves; the limiter is
        // pre-penalized so `acquire` fails and we return 429 *before* dispatch.
        let router = Router::new().register(
            "openai",
            Box::new(crate::providers::openai::OpenAi::new("test-key".into())),
        );
        let limiter = Arc::new(InMemoryRateLimiter::new(RateLimitConfig::with_global(
            Some(100),
            None,
        )));
        limiter
            .penalize(&RateKey::provider("openai"), Duration::from_secs(30))
            .await;

        let state = Arc::new(AppState {
            router,
            logger: None,
            limiter: limiter.clone(),
            backpressure: Backpressure::new(256, Duration::from_millis(50)),
        });

        let resp = post_chat(app_with_state(state), "/v1/chat", "openai/gpt-5.5").await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry = resp
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .expect("429 must carry Retry-After")
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(
            retry >= 1,
            "Retry-After should be a positive number of seconds"
        );
    }

    #[tokio::test]
    async fn rate_limited_also_applies_on_stream_endpoint() {
        let router = Router::new().register(
            "openai",
            Box::new(crate::providers::openai::OpenAi::new("test-key".into())),
        );
        let limiter = Arc::new(InMemoryRateLimiter::new(RateLimitConfig::with_global(
            Some(100),
            None,
        )));
        limiter
            .penalize(&RateKey::provider("openai"), Duration::from_secs(30))
            .await;

        let state = Arc::new(AppState {
            router,
            logger: None,
            limiter,
            backpressure: Backpressure::new(256, Duration::from_millis(50)),
        });

        let resp = post_chat(app_with_state(state), "/v1/chat/stream", "openai/gpt-5.5").await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().contains_key(axum::http::header::RETRY_AFTER));
    }

    #[tokio::test]
    async fn admission_passes_through_when_unlimited() {
        // No limits + no providers: admission is a no-op, so we fall through to
        // the normal dispatch which fails to resolve the provider → 400. Proves
        // the limiter doesn't interfere with normal error handling (no network).
        let state = Arc::new(AppState {
            router: Router::new(),
            logger: None,
            limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default())),
            backpressure: Backpressure::new(256, Duration::from_millis(50)),
        });

        let resp = post_chat(app_with_state(state), "/v1/chat", "openai/gpt-5.5").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn health_endpoint_unaffected() {
        let state = Arc::new(AppState {
            router: Router::new(),
            logger: None,
            limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default())),
            backpressure: Backpressure::new(1, Duration::from_millis(50)),
        });
        let resp = app_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn browser_origins_are_admitted_before_preflight_or_provider_dispatch() {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_body(
                serde_json::json!({
                    "id": "mock-response",
                    "choices": [{
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop"
                    }],
                    "usage": {}
                })
                .to_string(),
            )
            .expect(2)
            .create_async()
            .await;
        let router = Router::new().register(
            "local",
            Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                "local",
                server.url(),
                None,
            )),
        );
        let state = Arc::new(AppState {
            router,
            logger: None,
            limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default())),
            backpressure: Backpressure::new(256, Duration::from_millis(50)),
        });
        let app = app_with_origin_policy(
            state,
            OriginPolicy::from_csv("https://trusted.example").expect("trusted origin"),
        );

        let denied_preflight = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/v1/chat")
                    .header("origin", "https://untrusted.example")
                    .header("access-control-request-method", "POST")
                    .header("access-control-request-headers", "content-type")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_preflight.status(), StatusCode::FORBIDDEN);
        assert!(!denied_preflight
            .headers()
            .contains_key("access-control-allow-origin"));

        let denied_simple_request = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat")
                    .header("host", "trusted.example")
                    .header("origin", "https://untrusted.example")
                    .header("content-type", "text/plain")
                    .body(Body::from("browser-simple-request"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_simple_request.status(), StatusCode::FORBIDDEN);

        let trusted_preflight = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/v1/chat")
                    .header("origin", "https://trusted.example")
                    .header("access-control-request-method", "POST")
                    .header("access-control-request-headers", "content-type")
                    .header("access-control-request-private-network", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(trusted_preflight.status().is_success());
        assert_eq!(
            trusted_preflight
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "https://trusted.example"
        );
        assert_eq!(
            trusted_preflight
                .headers()
                .get("access-control-allow-private-network")
                .unwrap(),
            "true"
        );

        let sdk_response = post_chat(app.clone(), "/v1/chat", "local/test").await;
        assert_eq!(sdk_response.status(), StatusCode::OK);

        let trusted_browser_response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat")
                    .header("origin", "https://trusted.example")
                    .header("content-type", "application/json")
                    .body(chat_body("local/test"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(trusted_browser_response.status(), StatusCode::OK);
        assert_eq!(
            trusted_browser_response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "https://trusted.example"
        );
        upstream.assert_async().await;
    }

    #[tokio::test]
    async fn proxy_rejects_provider_config_admission_overrides_before_dispatch() {
        let state = Arc::new(AppState {
            router: Router::new().register(
                "openai",
                Box::new(crate::providers::openai::OpenAi::new("test-key".into())),
            ),
            logger: None,
            limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default())),
            backpressure: Backpressure::new(256, Duration::from_millis(50)),
        });
        for (path, provider_config) in [
            (
                "/v1/chat",
                serde_json::json!({"model": "anthropic/claude-opus-5"}),
            ),
            (
                "/v1/chat/stream",
                serde_json::json!({"x-openai": {"input": "sensitive-prompt-material"}}),
            ),
        ] {
            let response = app_with_state(state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "model": "openai/gpt-5.6-luna",
                                "messages": [{"role": "user", "content": "canonical"}],
                                "provider_config": provider_config,
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let response_body = to_bytes(response.into_body(), 10_000).await.unwrap();
            let response_text = String::from_utf8_lossy(&response_body);
            assert!(response_text.contains("invalid_request"));
            assert!(!response_text.contains("sensitive-prompt-material"));
        }
    }

    #[tokio::test]
    async fn proxy_rejects_fallback_namespace_activation_before_primary_dispatch() {
        let mut server = mockito::Server::new_async().await;
        let openai_upstream = server
            .mock("POST", "/responses")
            .expect(0)
            .create_async()
            .await;
        let anthropic_upstream = server
            .mock("POST", "/messages")
            .expect(0)
            .create_async()
            .await;
        let router = Router::new()
            .register(
                "openai",
                Box::new(
                    crate::providers::openai::OpenAi::new("test-key".into())
                        .with_base_url(server.url()),
                ),
            )
            .register(
                "anthropic",
                Box::new(
                    crate::providers::anthropic::Anthropic::new("test-key".into())
                        .with_base_url(server.url()),
                ),
            );
        let state = Arc::new(AppState {
            router,
            logger: None,
            limiter: Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default())),
            backpressure: Backpressure::new(256, Duration::from_millis(50)),
        });
        for request_body in [
            serde_json::json!({
                "model":"openai/gpt-5.6-luna",
                "messages":[{"role":"user","content":"canonical"}],
                "fallback":["anthropic/claude-sonnet-5"],
                "provider_config":{"x-anthropic":{"model":"claude-opus-5","messages":[]}}
            }),
            serde_json::json!({
                "model":"anthropic/claude-sonnet-5",
                "messages":[{"role":"user","content":"canonical"}],
                "fallback":["openai/gpt-5.6-luna"],
                "provider_config":{"x-openai":{"input":"replacement"}}
            }),
        ] {
            let response = app_with_state(state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/chat")
                        .header("content-type", "application/json")
                        .body(Body::from(request_body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        openai_upstream.assert_async().await;
        anthropic_upstream.assert_async().await;
    }
}

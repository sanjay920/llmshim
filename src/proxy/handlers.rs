use super::convert;
use super::error::ApiError;
use super::ratelimit::{estimate_prepared_request_tokens, penalty_duration, RateKey, RetryAfter};
use super::types::*;
use super::AppState;
use crate::error::ShimError;
use crate::log::RequestTimer;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;

/// A granted admission: holds the concurrency permit (released on drop) and the
/// rate-limit key so a subsequent upstream 429 can penalize the right bucket.
struct Admission {
    /// Held for the request's (or stream's) lifetime; frees a slot when dropped.
    _permit: OwnedSemaphorePermit,
    key: RateKey,
}

/// Proactive admission control, run before every upstream dispatch:
///   1. Acquire an instance concurrency permit (bounded wait → 503 on timeout).
///   2. Acquire a rate-limit token for the provider (→ 429 with Retry-After).
async fn admit(
    state: &Arc<AppState>,
    prepared_request: &convert::PreparedRequest,
) -> Result<Admission, ApiError> {
    // (1) Backpressure: bounded queue for a concurrency slot.
    let permit = state
        .backpressure
        .acquire()
        .await
        .map_err(|_| ApiError::Overloaded(state.backpressure.queue_timeout()))?;

    let key = RateKey::provider(&prepared_request.target.provider_name);
    let permits = estimate_prepared_request_tokens(prepared_request);
    if let Err(RetryAfter(wait)) = state.limiter.acquire(&key, permits).await {
        return Err(ApiError::RateLimited(wait));
    }

    Ok(Admission {
        _permit: permit,
        key,
    })
}

/// After an upstream failure, back the bucket off if it was a 429 so the whole
/// fleet (when Redis-coordinated) slows down together.
async fn penalize_on_429(state: &AppState, key: &RateKey, err: &ShimError) {
    if let ShimError::ProviderError { status: 429, .. } = err {
        state.limiter.penalize(key, penalty_duration()).await;
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

    // Admission control: concurrency permit + rate-limit token. The permit is
    // held until `admission` drops at the end of this function.
    let admission = admit(&state, &prepared_request).await?;

    let timer = RequestTimer::start();
    let value = prepared_request.payload;

    let result = if let Some(fallback_models) = &req.fallback {
        // Build fallback chain: primary model + fallback models
        let mut models = vec![req.model.clone()];
        models.extend(fallback_models.iter().cloned());
        let config = crate::FallbackConfig::new(models);
        crate::completion_with_fallback(&state.router, &value, &config, state.logger.as_ref()).await
    } else {
        crate::completion_with_logger(&state.router, &value, state.logger.as_ref()).await
    };

    let resp = match result {
        Ok(resp) => resp,
        Err(e) => {
            penalize_on_429(&state, &admission.key, &e).await;
            return Err(e.into());
        }
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
    // Admission control up front: reject with 429/503 (+ Retry-After) before we
    // commit to a stream, rather than emitting a rejection as an SSE event.
    let admission = match admit(&state, &prepared_request).await {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };

    let value = prepared_request.payload;
    let stream_result = crate::stream(&state.router, &value).await;

    if let Err(e) = &stream_result {
        penalize_on_429(&state, &admission.key, e).await;
    }

    let event_stream = async_stream::stream! {
        // Hold the concurrency permit for the whole stream lifetime.
        let _admission = admission;
        match stream_result {
            Ok(mut stream) => {
                while let Some(chunk) = stream.next().await {
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
            }
            Err(e) => {
                let error_event = super::error::stream_error(&e.to_string());
                if let Ok(data) = serde_json::to_string(&error_event) {
                    yield Ok(Event::default().event("error").data(data));
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
}

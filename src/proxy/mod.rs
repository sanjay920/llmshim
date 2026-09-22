mod attempt;
pub(crate) mod convert;
pub(crate) mod error;
mod handlers;
pub mod health;
mod lifetime;
pub(crate) mod origin;
pub mod ratelimit;
pub mod types;
pub mod wire;

use crate::log::Logger;
use crate::router::Router;
use axum::routing::{get, post};
use origin::OriginPolicy;
use ratelimit::{build_limiter, Backpressure, RateLimiter};
use std::sync::Arc;
pub(crate) fn preparation_overload_response(
    path: &str,
    queue_timeout: std::time::Duration,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let mut response = match path {
        "/v1/chat/completions" => wire::fail(
            wire::Wire::Chat,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Proxy is at capacity; retry after the suggested delay",
        ),
        "/v1/messages" => wire::fail(
            wire::Wire::Messages,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Proxy is at capacity; retry after the suggested delay",
        ),
        _ => error::ApiError::Overloaded(queue_timeout).into_response(),
    };
    let retry_after_seconds =
        queue_timeout.as_secs() + u64::from(queue_timeout.subsec_millis() > 0);
    if let Ok(retry_after) =
        axum::http::HeaderValue::from_str(&retry_after_seconds.max(1).to_string())
    {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, retry_after);
    }
    response
}

/// Shared state for all proxy handlers.
pub struct AppState {
    pub router: Router,
    pub logger: Option<Logger>,
    /// Proactive rate-limit coordinator (in-memory by default, Redis opt-in).
    pub limiter: Arc<dyn RateLimiter>,
    /// Per-instance concurrency cap + bounded queue (load shedding).
    pub backpressure: Backpressure,
}

impl AppState {
    /// Construct proxy state, reading rate-limit + backpressure config from the
    /// environment (all optional with safe defaults).
    pub fn from_env(router: Router, logger: Option<Logger>) -> Self {
        // Provider health is coordinated the same way rate limits are, so a
        // fleet agrees on which providers are down.
        let router = router.with_breaker(health::build_breaker());
        Self {
            router,
            logger,
            limiter: build_limiter(),
            backpressure: Backpressure::from_env(),
        }
    }
}

/// Build the axum application with all routes.
pub fn app(router: Router, logger: Option<Logger>) -> axum::Router {
    app_with_state(Arc::new(AppState::from_env(router, logger)))
}

/// Build the axum application from a pre-constructed state. Lets tests inject a
/// custom limiter / backpressure without touching the environment.
pub fn app_with_state(state: Arc<AppState>) -> axum::Router {
    app_with_origin_policy(state, OriginPolicy::from_env())
}

pub(crate) fn app_with_origin_policy(
    state: Arc<AppState>,
    origin_policy: OriginPolicy,
) -> axum::Router {
    app_with_origin_policy_and_deadlines(
        state,
        origin_policy,
        lifetime::LogicalRequestDeadlines::from_env(),
    )
}

fn app_with_origin_policy_and_deadlines(
    state: Arc<AppState>,
    origin_policy: OriginPolicy,
    logical_deadlines: lifetime::LogicalRequestDeadlines,
) -> axum::Router {
    let default_receipt_store = wire::DefaultReceiptStore::from_env();
    let preparation_state =
        lifetime::PreparationAdmissionState::new(state.backpressure.clone(), logical_deadlines);
    axum::Router::new()
        .route("/v1/chat", post(handlers::chat))
        .route("/v1/chat/completions", post(handlers::chat))
        .route("/v1/messages", post(handlers::chat))
        .route("/v1/chat/stream", post(handlers::chat_stream))
        .route("/v1/models", get(handlers::list_models))
        .route("/health", get(handlers::health))
        .layer(axum::middleware::from_fn(wire::translate))
        .layer(axum::middleware::from_fn(
            wire::bound_inference_request_json,
        ))
        .layer(axum::middleware::from_fn_with_state(
            default_receipt_store,
            wire::install_default_receipt_store,
        ))
        .layer(axum::middleware::from_fn_with_state(
            preparation_state,
            lifetime::admit_preparation,
        ))
        .layer(origin_policy.cors_layer())
        .layer(axum::middleware::from_fn(move |request, next| {
            origin::admit_browser_origin(origin_policy.clone(), request, next)
        }))
        .with_state(state)
}

#[cfg(test)]
mod preparation_lifetime_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::sse::{Event, Sse};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::{Notify, Semaphore};
    use tower::ServiceExt;

    fn state() -> Arc<AppState> {
        Arc::new(AppState {
            router: Router::new(),
            logger: None,
            limiter: Arc::new(ratelimit::InMemoryRateLimiter::new(
                ratelimit::RateLimitConfig::default(),
            )),
            backpressure: Backpressure::new(1, Duration::from_millis(20)),
        })
    }

    fn native_request(path: &str, stream: bool) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": "local/test",
                    "messages": [{"role": "user", "content": "hi"}],
                    "stream": stream
                })
                .to_string(),
            ))
            .unwrap()
    }

    async fn accepting_server() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let server_accepts = accepts.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                server_accepts.fetch_add(1, Ordering::SeqCst);
                drop(socket);
            }
        });
        (format!("http://{address}"), accepts, task)
    }

    fn bounded_tools(messages_wire: bool, properties_per_tool: usize) -> Vec<serde_json::Value> {
        (0..10)
            .map(|tool_index| {
                let properties = (0..properties_per_tool)
                    .map(|property_index| {
                        (
                            format!("property_{tool_index}_{property_index}"),
                            serde_json::json!({
                                "type": "string",
                                "description": format!(
                                    "bounded synthetic property {tool_index} {property_index}"
                                )
                            }),
                        )
                    })
                    .collect::<serde_json::Map<_, _>>();
                let schema = serde_json::json!({
                    "type": "object",
                    "properties": properties,
                    "required": [],
                });
                if messages_wire {
                    serde_json::json!({
                        "name": format!("tool_{tool_index}"),
                        "description": format!("bounded synthetic tool {tool_index}"),
                        "input_schema": schema,
                    })
                } else {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": format!("tool_{tool_index}"),
                            "description": format!("bounded synthetic tool {tool_index}"),
                            "parameters": schema,
                        }
                    })
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn native_unary_response_translation_retains_outer_preparation_capacity() {
        let response_buffering_started = Arc::new(Notify::new());
        let first_response_buffering_started = response_buffering_started.notified();
        let release_response = Arc::new(Semaphore::new(0));
        let handler = {
            let response_buffering_started = response_buffering_started.clone();
            let release_response = release_response.clone();
            move || {
                let response_buffering_started = response_buffering_started.clone();
                let release_response = release_response.clone();
                async move {
                    let body = async_stream::stream! {
                        response_buffering_started.notify_one();
                        release_response
                            .acquire()
                            .await
                            .expect("test release semaphore remains open")
                            .forget();
                        yield Ok::<_, Infallible>(serde_json::json!({
                            "id": "response",
                            "model": "local/test",
                            "message": {"role": "assistant", "content": "ok"},
                            "usage": {},
                            "finish_reason": "stop"
                        }).to_string());
                    };
                    (
                        [((axum::http::header::CONTENT_TYPE), "application/json")],
                        Body::from_stream(body),
                    )
                }
            }
        };
        let state = state();
        let preparation_state = lifetime::PreparationAdmissionState::new(
            state.backpressure.clone(),
            lifetime::LogicalRequestDeadlines::new(
                Duration::from_secs(60),
                Duration::from_secs(60),
            ),
        );
        let application = axum::Router::new()
            .route("/v1/chat/completions", post(handler))
            .layer(axum::middleware::from_fn(wire::translate))
            .layer(axum::middleware::from_fn_with_state(
                preparation_state,
                lifetime::admit_preparation,
            ))
            .with_state(state);

        let first_request = tokio::spawn(
            application
                .clone()
                .oneshot(native_request("/v1/chat/completions", false)),
        );
        tokio::time::timeout(Duration::from_secs(1), first_response_buffering_started)
            .await
            .expect("native unary translation should begin buffering");
        let saturated = application
            .clone()
            .oneshot(native_request("/v1/chat/completions", false))
            .await
            .unwrap();
        assert_eq!(saturated.status(), StatusCode::SERVICE_UNAVAILABLE);
        release_response.add_permits(1);
        let completed = first_request.await.unwrap().unwrap();
        assert_eq!(completed.status(), StatusCode::OK);
        let body = axum::body::to_bytes(completed.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["content"], "ok");
    }

    #[tokio::test]
    async fn never_polled_native_sse_body_releases_preparation_capacity_at_deadline() {
        let handler = || async move {
            let stream = async_stream::stream! {
                std::future::pending::<()>().await;
                yield Ok::<Event, Infallible>(Event::default());
            };
            Sse::new(stream).into_response()
        };
        let state = state();
        let preparation_state = lifetime::PreparationAdmissionState::new(
            state.backpressure.clone(),
            lifetime::LogicalRequestDeadlines::new(
                Duration::from_millis(50),
                Duration::from_millis(50),
            ),
        );
        let application = axum::Router::new()
            .route("/v1/messages", post(handler))
            .layer(axum::middleware::from_fn(wire::translate))
            .layer(axum::middleware::from_fn_with_state(
                preparation_state,
                lifetime::admit_preparation,
            ))
            .with_state(state);

        let never_polled = application
            .clone()
            .oneshot(native_request("/v1/messages", true))
            .await
            .unwrap();
        assert_eq!(never_polled.status(), StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(60)).await;
        let admitted = tokio::time::timeout(
            Duration::from_secs(1),
            application.oneshot(native_request("/v1/messages", true)),
        )
        .await
        .expect("deadline should release capacity without polling the first body")
        .unwrap();
        assert_eq!(admitted.status(), StatusCode::OK);
        drop(never_polled);
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_stream_mode_during_body_read_uses_initial_unary_deadline() {
        for path in ["/v1/chat", "/v1/chat/completions", "/v1/messages"] {
            let state = state();
            let application = app_with_origin_policy_and_deadlines(
                state,
                OriginPolicy::default(),
                lifetime::LogicalRequestDeadlines::new(
                    Duration::from_secs(2),
                    Duration::from_secs(6),
                ),
            );
            let body = async_stream::stream! {
                yield Ok::<_, Infallible>("{\"model\":\"local/test\",\"stream\":");
                tokio::time::sleep(Duration::from_secs(3)).await;
                yield Ok("true,\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}");
            };
            let request = Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from_stream(body))
                .unwrap();
            let response = tokio::spawn(application.oneshot(request));
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(2)).await;
            let response = response.await.unwrap().unwrap();
            assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT, "{path}");
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            if path == "/v1/messages" {
                assert_eq!(body["type"], "error", "{path}");
            } else {
                assert!(body["error"].is_object(), "{path}");
            }
        }
    }

    #[tokio::test]
    async fn expired_shorter_stream_mode_releases_unpolled_504_capacity() {
        let state = Arc::new(AppState {
            router: Router::new(),
            logger: None,
            limiter: Arc::new(ratelimit::InMemoryRateLimiter::new(
                ratelimit::RateLimitConfig::default(),
            )),
            backpressure: Backpressure::new(1, Duration::from_millis(30)),
        });
        let application = app_with_origin_policy_and_deadlines(
            state,
            OriginPolicy::default(),
            lifetime::LogicalRequestDeadlines::new(
                Duration::from_millis(500),
                Duration::from_millis(10),
            ),
        );
        let delayed_stream_body = async_stream::stream! {
            tokio::time::sleep(Duration::from_millis(40)).await;
            yield Ok::<_, Infallible>(serde_json::json!({
                "model": "local/test",
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }).to_string());
        };
        let first = application
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat")
                    .header("content-type", "application/json")
                    .body(Body::from_stream(delayed_stream_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::GATEWAY_TIMEOUT);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let second = application
            .oneshot(native_request("/v1/chat", false))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::BAD_REQUEST);
        drop(first);
    }

    #[tokio::test]
    async fn built_in_preparation_crossing_deadline_starts_zero_sends() {
        let (server_url, accepts, server) = accepting_server().await;
        let provider =
            crate::providers::openai_compat::OpenAiCompatible::new("local", server_url, None);
        let tools = bounded_tools(false, 20);
        let policy_context = attempt::context(
            Arc::new(ratelimit::InMemoryRateLimiter::new(
                ratelimit::RateLimitConfig::default(),
            )),
            Backpressure::new(4, Duration::from_secs(1)),
            Some(tokio::time::Instant::now() + Duration::from_nanos(1)),
        );
        let error = crate::client::ShimClient::new()
            .completion_with_policy(
                &provider,
                "test",
                &serde_json::json!({
                    "model": "local/test",
                    "messages": [{"role": "user", "content": "hi"}],
                    "tools": tools,
                }),
                &policy_context,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::error::ShimError::ProviderError {
                status: 504,
                ref body,
                ..
            } if body == "proxy logical request timed out"
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(accepts.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn public_app_preparation_overrun_returns_readable_json_and_releases_capacity() {
        for path in ["/v1/chat", "/v1/chat/completions", "/v1/messages"] {
            let (primary_url, primary_accepts, primary_server) = accepting_server().await;
            let (fallback_url, fallback_accepts, fallback_server) = accepting_server().await;
            let router = Router::new()
                .register(
                    "primary",
                    Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                        "primary",
                        primary_url,
                        None,
                    )),
                )
                .register(
                    "fallback",
                    Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                        "fallback",
                        fallback_url,
                        None,
                    )),
                );
            let state = Arc::new(AppState {
                router,
                logger: None,
                limiter: Arc::new(ratelimit::InMemoryRateLimiter::new(
                    ratelimit::RateLimitConfig::default(),
                )),
                backpressure: Backpressure::new(1, Duration::from_millis(30)),
            });
            let application = app_with_origin_policy_and_deadlines(
                state,
                OriginPolicy::default(),
                lifetime::LogicalRequestDeadlines::new(
                    Duration::from_nanos(1),
                    Duration::from_nanos(1),
                ),
            );
            let request_body = serde_json::json!({
                "model": "primary/test",
                "fallback": ["fallback/test"],
                "messages": [{"role": "user", "content": "hi"}],
                "tools": bounded_tools(path == "/v1/messages", 500),
            });
            let response = application
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(request_body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT, "{path}");
            let response_body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("pre-header timeout body must remain readable");
            let response_body: serde_json::Value = serde_json::from_slice(&response_body).unwrap();
            match path {
                "/v1/chat" => {
                    assert_eq!(response_body["error"]["code"], "request_timeout")
                }
                "/v1/chat/completions" => {
                    assert_eq!(response_body["error"]["type"], "api_error")
                }
                "/v1/messages" => {
                    assert_eq!(response_body["type"], "error");
                    assert_eq!(response_body["error"]["type"], "api_error");
                }
                _ => unreachable!(),
            }
            assert!(response_body.to_string().contains("logical lifetime"));

            let second = application
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "model": "missing/test",
                                "messages": [{"role": "user", "content": "hi"}]
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(
                second.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{path} must reacquire preparation capacity"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(primary_accepts.load(Ordering::SeqCst), 0, "{path}");
            assert_eq!(fallback_accepts.load(Ordering::SeqCst), 0, "{path}");
            primary_server.abort();
            fallback_server.abort();
        }
    }

    #[tokio::test]
    async fn same_poll_overrun_returns_direct_json_without_installing_a_pump() {
        let handler = || async {
            std::thread::sleep(Duration::from_millis(20));
            (
                StatusCode::GATEWAY_TIMEOUT,
                axum::Json(serde_json::json!({"inner":"must be discarded"})),
            )
        };
        let state = state();
        let preparation_state = lifetime::PreparationAdmissionState::new(
            state.backpressure.clone(),
            lifetime::LogicalRequestDeadlines::new(
                Duration::from_millis(10),
                Duration::from_millis(10),
            ),
        );
        let application = axum::Router::new()
            .route("/v1/chat", post(handler))
            .layer(axum::middleware::from_fn_with_state(
                preparation_state,
                lifetime::admit_preparation,
            ))
            .with_state(state);
        let first = application
            .clone()
            .oneshot(native_request("/v1/chat", false))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::GATEWAY_TIMEOUT);
        let first_body = axum::body::to_bytes(first.into_body(), 4096)
            .await
            .expect("direct timeout body remains readable");
        let first_body: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
        assert_eq!(first_body["error"]["code"], "request_timeout");
        assert!(first_body.get("inner").is_none());

        let second = application
            .oneshot(native_request("/v1/chat", false))
            .await
            .unwrap();
        assert_ne!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn one_logical_deadline_stops_a_fallback_chain_before_another_send() {
        async fn stalled_server() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let accepts = Arc::new(AtomicUsize::new(0));
            let server_accepts = accepts.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        return;
                    };
                    server_accepts.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        let _socket = socket;
                        std::future::pending::<()>().await;
                    });
                }
            });
            (format!("http://{address}"), accepts, task)
        }

        let (primary_url, primary_accepts, primary_task) = stalled_server().await;
        let (fallback_url, fallback_accepts, fallback_task) = stalled_server().await;
        let router = Router::new()
            .register(
                "primary",
                Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                    "primary",
                    primary_url,
                    None,
                )),
            )
            .register(
                "fallback",
                Box::new(crate::providers::openai_compat::OpenAiCompatible::new(
                    "fallback",
                    fallback_url,
                    None,
                )),
            );
        let state = Arc::new(AppState {
            router,
            logger: None,
            limiter: Arc::new(ratelimit::InMemoryRateLimiter::new(
                ratelimit::RateLimitConfig::default(),
            )),
            backpressure: Backpressure::new(4, Duration::from_secs(1)),
        });
        let application = app_with_origin_policy_and_deadlines(
            state,
            OriginPolicy::default(),
            lifetime::LogicalRequestDeadlines::new(
                Duration::from_millis(500),
                Duration::from_secs(1),
            ),
        );
        let response = application
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "primary/test",
                            "fallback": ["fallback/test"],
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(primary_accepts.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_accepts.load(Ordering::SeqCst), 0);
        primary_task.abort();
        fallback_task.abort();
    }
}

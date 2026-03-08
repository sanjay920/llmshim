use super::convert;
use super::error::ApiError;
use super::types::*;
use crate::log::RequestTimer;
use crate::models;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;

use super::AppState;

/// POST /v1/chat — non-streaming completion (or streaming if stream=true)
pub async fn chat(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ChatRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if req.stream {
        // Delegate to streaming
        return Ok(chat_stream_inner(state, req).await.into_response());
    }

    let timer = RequestTimer::start();
    let value = convert::request_to_value(&req);

    let resp = if let Some(fallback_models) = &req.fallback {
        // Build fallback chain: primary model + fallback models
        let mut models = vec![req.model.clone()];
        models.extend(fallback_models.iter().cloned());
        let config = crate::FallbackConfig::new(models);
        crate::completion_with_fallback(&state.router, &value, &config, state.logger.as_ref())
            .await?
    } else {
        crate::completion_with_logger(&state.router, &value, state.logger.as_ref()).await?
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
) -> impl IntoResponse {
    chat_stream_inner(state, req).await
}

async fn chat_stream_inner(
    state: Arc<AppState>,
    req: ChatRequest,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let value = convert::request_to_value(&req);

    let stream_result = crate::stream(&state.router, &value).await;

    let event_stream = async_stream::stream! {
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
                            let error_event = StreamEvent::Error {
                                message: e.to_string(),
                            };
                            if let Ok(data) = serde_json::to_string(&error_event) {
                                yield Ok(Event::default().event("error").data(data));
                            }
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                let error_event = StreamEvent::Error {
                    message: e.to_string(),
                };
                if let Ok(data) = serde_json::to_string(&error_event) {
                    yield Ok(Event::default().event("error").data(data));
                }
            }
        }
    };

    Sse::new(event_stream)
}

/// GET /v1/models — list available models
pub async fn list_models(State(state): State<Arc<AppState>>) -> Json<ModelsResponse> {
    let provider_keys = state.router.provider_keys();
    let available = models::available_models(&provider_keys);

    let entries = available
        .into_iter()
        .map(|m| ModelEntry {
            id: m.id.to_string(),
            provider: m.provider.to_string(),
            name: m.name.to_string(),
        })
        .collect();

    Json(ModelsResponse { models: entries })
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

//! Incremental Responses events from the shared engine's bounded stream.
use super::super::{response_from_chat, ReceiptExecutor, ReceiptWorkKind, Receipts, Wire};
use super::events::Events;
use axum::{
    body::Body,
    response::sse::{Event, Sse},
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::{convert::Infallible, sync::Arc};

pub(crate) fn stream_identity(chunk: &str) -> Option<Event> {
    let value: Value = serde_json::from_str(chunk).ok()?;
    Some(
        Event::default().event("response_identity").data(
            json!({
                "type": "response_identity",
                "id": value["id"],
                "created": value["created"],
                "model": value["model"]
            })
            .to_string(),
        ),
    )
}

pub(in crate::proxy::wire) struct StreamOptions {
    pub metadata: Value,
    pub include: bool,
    pub redactor: Option<super::super::ResponseRedactor>,
}

/// Accepts the shared engine stream and include selection; fails on upstream or receipt errors.
pub(in crate::proxy::wire) fn stream_response(
    body: Body,
    model: Value,
    receipts: Arc<Receipts>,
    scope: String,
    executor: ReceiptExecutor,
    options: StreamOptions,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let StreamOptions {
        metadata,
        include,
        redactor,
    } = options;
    Sse::new(async_stream::stream! {
        let mut canonical = json!({
            "id": format!("msg_{}",
            uuid::Uuid::new_v4().simple()),
            "model": model,
            "created_at": chrono::Utc::now().timestamp(),
            "message": {
                "role": "assistant",
                "content": ""
            },
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "total_tokens": 0,
                "cache_read_tokens": 0,
                "reasoning_tokens": 0
            },
            "finish_reason": "stop"
        });
        let mut upstream = Box::pin(crate::sse::data(body.into_data_stream()));
        let mut pending = upstream.next().await;
        if let Some(Ok(frame)) = &pending {
            if let Ok(identity) = serde_json::from_str::<Value>(frame) {
                if identity["type"] == "response_identity" {
                    if let Some(id) = identity["id"].as_str().filter(|id| !id.is_empty()) {
                        canonical["id"] = json!(id);
                    }
                    if identity["created"].is_number() {
                        canonical["created_at"] = identity["created"].clone();
                    }
                    if identity["model"].is_string() {
                        canonical["model"] = identity["model"].clone();
                    }
                    pending = None;
                }
            }
        }
        let mut state = Events::new(super::response(&canonical, &json!({}), "stop"));
        if let Err(message) = super::output_options(
            &mut state.response, metadata.clone(), include,
        ) {
            yield Ok(state.failed(&message));
            return;
        }
        for kind in ["response.created", "response.in_progress"] {
            yield Ok(state.event(kind, json!({"response": state.response.clone()})));
        }
        // Hosted discovery can precede text, so its terminal order determines event indices.
        let hosted = metadata["x-responses-tools"].as_array().is_some_and(|tools| {
            tools.iter().any(|tool| matches!(
                tool["type"].as_str(), Some("tool_search" | "web_search")
            ))
        });
        let mut reasoning = crate::reasoning::ReasoningAccumulator::default();
        let mut calls = Vec::new();
        let mut done = false;
        let mut failure = None;
        while let Some(frame) = match pending.take() {
            Some(frame) => Some(frame),
            None => upstream.next().await,
        } {
            let frame = match frame {
                Ok(frame) => frame,
                Err(_) => {
                    failure = Some("upstream stream failed");
                    break;
                }
            };
            let data: Value = match serde_json::from_str(&frame) {
                Ok(data) => data,
                Err(_) => continue,
            };
            match data["type"].as_str() {
                Some("content") => {
                    let text = data["text"].as_str().unwrap_or("");
                    crate::streaming::append_string_fragment(
                        &mut canonical["message"]["content"],
                        text,
                    );
                    if !hosted {
                        for event in state.text(text) {
                            yield Ok(event);
                        }
                    }
                }
                Some("reasoning") => {
                    if reasoning
                        .push(&json!({
                            "reasoning": data["blocks"]
                        }))
                        .is_err()
                    {
                        failure = Some(crate::stream_retention::RETENTION_ERROR);
                        break;
                    }
                }
                Some("tool_call") => {
                    let mut call = json!({
                        "id": data["id"],
                        "type": "function",
                        "function": {
                            "name": data["name"],
                            "arguments": data["arguments"]
                        },
                        "wire_ids": data["wire_ids"]
                    });
                    if let Some(signature) = data.get("thought_signature") {
                        call["thought_signature"] = signature.clone();
                    }
                    calls.push(call);
                }
                Some("usage") => canonical["usage"] = data,
                Some("done") => {
                    if let Some(output) = data.get("responses_output") {
                        canonical["message"]["responses_output"] = output.clone();
                    }
                    done = true;
                    canonical["finish_reason"] =
                        data["finish_reason"].as_str().unwrap_or("stop").into();
                    if let Some(served) = data.get("x-llmshim-served-model") {
                        canonical["x-llmshim-served-model"] = served.clone();
                    }
                    break;
                }
                Some("error") => {
                    failure = Some("upstream stream failed");
                    break;
                }
                _ => {}
            }
        }
        if !calls.is_empty() {
            canonical["message"]["tool_calls"] = json!(calls);
        }
        if !done {
            canonical["message"]["reasoning"] = json!(reasoning.blocks());
            state.response = super::response(&canonical, &canonical["usage"], "stop");
            let _ = super::output_options(&mut state.response, metadata.clone(), include);
            yield Ok(state.failed(failure.unwrap_or("stream ended before completion")));
            return;
        }
        canonical["message"]["reasoning"] = json!(reasoning.blocks());
        if let Some(redactor) = &redactor {
            canonical = (redactor.0)(&canonical);
        }
        let failure_snapshot = super::response(&canonical, &canonical["usage"], "stop");
        let native = executor
            .run(ReceiptWorkKind::Egress, move || {
                response_from_chat(&canonical, Wire::Responses, &receipts, &scope)
            })
            .await;
        match native {
            Ok(native) => match serde_json::from_value::<super::Response>(native) {
                Ok(mut native) => {
                    match super::output_options(&mut native, metadata, include) {
                        Ok(()) => for event in state.finish(native) {
                            yield Ok(event);
                        },
                        Err(message) => yield Ok(state.failed(&message)),
                    }
                }
                Err(_) => {
                    state.response = failure_snapshot;
                    let _ = super::output_options(&mut state.response, metadata, include);
                    yield Ok(state.failed("malformed Responses output"));
                }
            },
            Err(_) => {
                state.response = failure_snapshot;
                let _ = super::output_options(&mut state.response, metadata, include);
                yield Ok(state.failed("native replay metadata unavailable"));
            }
        }
    })
    .keep_alive(axum::response::sse::KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn aggregate_stream_bound_accepts_the_near_miss_and_fails_overflow() {
        let padding = format!(
            "data: {}\n\n",
            json!({"type": "ignored","padding": "x".repeat(512*1024)})
        );
        for (count, terminal) in [(63, "response.completed"), (64, "response.failed")] {
            let padding = bytes::Bytes::from(padding.clone());
            let frames = futures::stream::iter(
                (0..count)
                    .map(move |_| Ok::<_, Infallible>(padding.clone()))
                    .chain(std::iter::once(Ok(bytes::Bytes::from(
                        "data: {\"type\":\"done\",\"finish_reason\":\"stop\"}\n\n",
                    )))),
            );
            let receipts = Arc::new(Receipts::new(std::path::PathBuf::from(format!(
                "target/responses-stream-bound-{}",
                uuid::Uuid::new_v4()
            ))));
            let response = stream_response(
                Body::from_stream(frames),
                json!("local/test"),
                receipts,
                "scope".into(),
                ReceiptExecutor::new(),
                StreamOptions {
                    metadata: json!({}),
                    include: false,
                    redactor: None,
                },
            )
            .into_response();
            let body = axum::body::to_bytes(response.into_body(), 100_000)
                .await
                .unwrap();
            let text = std::str::from_utf8(&body).unwrap();
            let final_frame: Value = serde_json::from_str(
                text.split("\n\n")
                    .filter_map(|frame| frame.lines().find_map(|line| line.strip_prefix("data: ")))
                    .last()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(final_frame["type"], terminal);
            if count == 64 {
                assert_eq!(
                    final_frame["response"]["error"]["message"],
                    "upstream stream failed"
                );
            }
        }
    }
}

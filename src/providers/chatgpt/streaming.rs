use super::{auth::auth_error, transform_response, translator};
use crate::{
    error::{Result, ShimError},
    provider::Provider,
};
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::{collections::BTreeMap, pin::Pin};

type NativeStream = Pin<Box<dyn Stream<Item = Result<Value>> + Send>>;

fn stream_error(message: &str) -> ShimError {
    ShimError::Stream(format!("ChatGPT: {message}"))
}

fn terminal(event: &Value) -> Result<bool> {
    let kind = event["type"].as_str().unwrap_or("");
    match kind {
        "error" | "response.failed" => Err(auth_error(502, "upstream response failed")),
        "response.completed" | "response.incomplete" => {
            let expected = if kind == "response.completed" {
                "completed"
            } else {
                "incomplete"
            };
            if event["response"]["status"] != expected || !event["response"]["error"].is_null() {
                return Err(stream_error("invalid terminal response"));
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn native_stream(response: reqwest::Response) -> NativeStream {
    // This parser handles multiline SSE, CRLF, comments, and UTF-8 split across
    // network chunks. Never treat EOF/[DONE] alone as a successful completion.
    let events = Box::pin(crate::sse::data(response.bytes_stream()));
    Box::pin(futures::stream::unfold(
        (events, false),
        |(mut events, finished)| async move {
            if finished {
                return None;
            }
            loop {
                let value = match events.next().await {
                    Some(Ok(event)) if event.trim().is_empty() => continue,
                    Some(Ok(event)) if event.trim() == "[DONE]" => {
                        Err(stream_error("stream ended before a terminal response"))
                    }
                    Some(Ok(event)) => serde_json::from_str::<Value>(&event)
                        .map_err(|_| stream_error("invalid SSE JSON")),
                    Some(Err(_)) => Err(stream_error("could not read upstream SSE")),
                    None => Err(stream_error("stream ended before a terminal response")),
                };
                let (value, finished) = match value {
                    Ok(value) => {
                        let finished = matches!(
                            value["type"].as_str(),
                            Some(
                                "error"
                                    | "response.failed"
                                    | "response.completed"
                                    | "response.incomplete"
                            )
                        );
                        (Ok(value), finished)
                    }
                    Err(e) => (Err(e), true),
                };
                return Some((value, (events, finished)));
            }
        },
    ))
}

pub(crate) struct CollectedResponse {
    pub(crate) result: Result<Value>,
    pub(crate) native_terminal: Option<Value>,
}

pub(crate) async fn collect_response_with_terminal(
    _model: &str,
    response: reqwest::Response,
    semantic_idle_timeout: std::time::Duration,
    attempt_deadline: tokio::time::Instant,
) -> CollectedResponse {
    let mut events = native_stream(response);
    let mut items = BTreeMap::new();
    let mut texts: BTreeMap<u64, BTreeMap<u64, Value>> = BTreeMap::new();
    loop {
        let Some(semantic_idle_deadline) =
            tokio::time::Instant::now().checked_add(semantic_idle_timeout)
        else {
            return CollectedResponse {
                result: Err(timeout_error()),
                native_terminal: None,
            };
        };
        let event = tokio::select! {
            event = events.next() => event,
            _ = tokio::time::sleep_until(semantic_idle_deadline) => {
                return CollectedResponse { result: Err(timeout_error()), native_terminal: None };
            }
            _ = tokio::time::sleep_until(attempt_deadline) => {
                return CollectedResponse { result: Err(timeout_error()), native_terminal: None };
            }
        };
        let Some(event) = event else { break };
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                return CollectedResponse {
                    result: Err(error),
                    native_terminal: None,
                }
            }
        };
        match event["type"].as_str() {
            Some("error" | "response.failed") => {
                return CollectedResponse {
                    result: Err(auth_error(502, "upstream response failed")),
                    native_terminal: event
                        .get("response")
                        .filter(|value| value.is_object())
                        .cloned(),
                }
            }
            Some("response.output_item.done") if event["item"].is_object() => {
                if let Some(index) = event["output_index"].as_u64() {
                    items.insert(index, event["item"].clone());
                }
            }
            Some("response.output_text.done") if event["text"].is_string() => {
                if let (Some(output), Some(content)) = (
                    event["output_index"].as_u64(),
                    event["content_index"].as_u64(),
                ) {
                    texts.entry(output).or_default().insert(
                        content,
                        json!({"type": "output_text", "text": event["text"]}),
                    );
                }
            }
            Some("response.completed" | "response.incomplete") => {
                let mut response = event["response"].clone();
                let terminal_validation = terminal(&event);
                // Some backend versions put output only in output_item.done.
                if response["output"].as_array().is_none_or(Vec::is_empty) {
                    for (index, parts) in texts {
                        items.entry(index).or_insert_with(|| json!({"type": "message", "role": "assistant", "content": parts.into_values().collect::<Vec<_>>() }));
                    }
                    response["output"] = json!(items.into_values().collect::<Vec<_>>());
                }
                return CollectedResponse {
                    result: terminal_validation.map(|_| response.clone()),
                    native_terminal: Some(response),
                };
            }
            _ => {}
        }
    }
    CollectedResponse {
        result: Err(stream_error("missing terminal response")),
        native_terminal: None,
    }
}

fn timeout_error() -> ShimError {
    ShimError::ProviderError {
        status: 504,
        body: "upstream response body timed out".into(),
        retry_after: None,
    }
}

pub(crate) fn transform_chunk(model: &str, chunk: &str) -> Result<Option<String>> {
    if chunk.trim().is_empty() || chunk.trim() == "[DONE]" {
        return Ok(None);
    }
    let event: Value = serde_json::from_str(chunk).map_err(|_| stream_error("invalid SSE JSON"))?;
    if terminal(&event)? {
        let mut response = event["response"].clone();
        if !response["output"].is_array() {
            response["output"] = json!([]);
        }
        let completion = transform_response(model, response)?;
        return Ok(Some(json!({
            "id": completion["id"], "object": "chat.completion.chunk", "model": completion["model"],
            "choices": [{"index": 0, "delta": {}, "finish_reason": completion["choices"][0]["finish_reason"]}],
            "usage": completion["usage"]
        }).to_string()));
    }
    translator().transform_stream_chunk(&format!("chatgpt/{model}"), chunk)
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn comments_and_silence_expire_without_a_terminal_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\nd\r\n: keepalive\n\n\r\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}/responses"))
            .send()
            .await
            .unwrap();
        let collected = collect_response_with_terminal(
            "test-model",
            response,
            std::time::Duration::from_millis(20),
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await;
        assert!(matches!(
            collected.result,
            Err(ShimError::ProviderError { status: 504, .. })
        ));
        assert!(collected.native_terminal.is_none());
    }
}

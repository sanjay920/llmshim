use super::{auth::auth_error, transform_response, translator};
use crate::{
    error::{Result, ShimError},
    provider::Provider,
};
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::{collections::BTreeMap, mem::size_of, pin::Pin};

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
    pub(crate) native_usage: Option<Value>,
}

struct RetainedValue {
    value: Value,
    footprint: crate::stream_retention::RetainedFootprint,
}

struct RetainedTextParts {
    parts: BTreeMap<u64, RetainedValue>,
    map_footprint: crate::stream_retention::RetainedFootprint,
}

struct CollectorState {
    items: BTreeMap<u64, RetainedValue>,
    texts: BTreeMap<u64, RetainedTextParts>,
    budget: crate::stream_retention::RetainedBudget,
    retained: crate::stream_retention::RetainedFootprint,
}

impl CollectorState {
    fn new(limits: crate::streaming::StreamRetentionLimits) -> Self {
        Self {
            items: BTreeMap::new(),
            texts: BTreeMap::new(),
            budget: crate::stream_retention::RetainedBudget::new(
                limits.normalizer_bytes,
                limits.normalizer_entries,
            ),
            retained: Default::default(),
        }
    }

    fn retain_item(&mut self, output_index: u64, item: Value) -> Result<()> {
        if let Some(discarded_text) = self.texts.remove(&output_index) {
            self.release(text_parts_footprint(&discarded_text)?);
        }
        let previous = self
            .items
            .get(&output_index)
            .map(|retained| retained.footprint)
            .unwrap_or_default();
        let mut replacement = crate::stream_retention::estimate_value(&item)?;
        if previous.entries == 0 {
            replacement = replacement
                .checked_add(crate::stream_retention::RetainedFootprint::record(
                    size_of::<u64>(),
                ))
                .ok_or_else(crate::stream_retention::retention_error)?;
        }
        self.replace(previous, replacement)?;
        self.items.insert(
            output_index,
            RetainedValue {
                value: item,
                footprint: replacement,
            },
        );
        Ok(())
    }

    fn retain_text(&mut self, output_index: u64, content_index: u64, text: Value) -> Result<()> {
        if self.items.contains_key(&output_index) {
            return Ok(());
        }
        let output_text = json!({"type": "output_text", "text": text});
        let previous = self
            .texts
            .get(&output_index)
            .and_then(|retained| retained.parts.get(&content_index))
            .map(|retained| retained.footprint)
            .unwrap_or_default();
        let mut replacement = crate::stream_retention::estimate_value(&output_text)?;
        if previous.entries == 0 {
            replacement = replacement
                .checked_add(crate::stream_retention::RetainedFootprint::record(
                    size_of::<u64>(),
                ))
                .ok_or_else(crate::stream_retention::retention_error)?;
        }

        let newly_seen_output = !self.texts.contains_key(&output_index);
        let map_footprint = crate::stream_retention::RetainedFootprint::record(
            size_of::<u64>() + size_of::<BTreeMap<u64, RetainedValue>>(),
        );
        if newly_seen_output {
            self.reserve(map_footprint)?;
        }
        if let Err(error) = self.replace(previous, replacement) {
            if newly_seen_output {
                self.release(map_footprint);
            }
            return Err(error);
        }
        let retained_parts = self
            .texts
            .entry(output_index)
            .or_insert_with(|| RetainedTextParts {
                parts: BTreeMap::new(),
                map_footprint,
            });
        retained_parts.parts.insert(
            content_index,
            RetainedValue {
                value: output_text,
                footprint: replacement,
            },
        );
        Ok(())
    }

    fn finish(mut self, mut response: Value) -> Result<Value> {
        if response["output"].as_array().is_none_or(Vec::is_empty) {
            for (output_index, retained_text) in std::mem::take(&mut self.texts) {
                let content = retained_text
                    .parts
                    .into_values()
                    .map(|retained| retained.value)
                    .collect::<Vec<_>>();
                self.items
                    .entry(output_index)
                    .or_insert_with(|| RetainedValue {
                        value: json!({
                            "type": "message",
                            "role": "assistant",
                            "content": content,
                        }),
                        footprint: Default::default(),
                    });
            }
            response["output"] = json!(std::mem::take(&mut self.items)
                .into_values()
                .map(|retained| retained.value)
                .collect::<Vec<_>>());
        }
        let terminal_footprint = crate::stream_retention::estimate_value(&response)?;
        self.budget.replace(self.retained, terminal_footprint)?;
        Ok(response)
    }

    fn reserve(&mut self, footprint: crate::stream_retention::RetainedFootprint) -> Result<()> {
        self.budget.reserve(footprint)?;
        self.retained = self
            .retained
            .checked_add(footprint)
            .ok_or_else(crate::stream_retention::retention_error)?;
        Ok(())
    }

    fn replace(
        &mut self,
        previous: crate::stream_retention::RetainedFootprint,
        replacement: crate::stream_retention::RetainedFootprint,
    ) -> Result<()> {
        self.budget.replace(previous, replacement)?;
        self.retained.bytes = self
            .retained
            .bytes
            .saturating_sub(previous.bytes)
            .saturating_add(replacement.bytes);
        self.retained.entries = self
            .retained
            .entries
            .saturating_sub(previous.entries)
            .saturating_add(replacement.entries);
        Ok(())
    }

    fn release(&mut self, footprint: crate::stream_retention::RetainedFootprint) {
        self.budget.release(footprint);
        self.retained.bytes = self.retained.bytes.saturating_sub(footprint.bytes);
        self.retained.entries = self.retained.entries.saturating_sub(footprint.entries);
    }
}

fn text_parts_footprint(
    retained_text: &RetainedTextParts,
) -> Result<crate::stream_retention::RetainedFootprint> {
    retained_text
        .parts
        .values()
        .try_fold(retained_text.map_footprint, |total, retained| {
            total
                .checked_add(retained.footprint)
                .ok_or_else(crate::stream_retention::retention_error)
        })
}

pub(crate) async fn collect_response_with_terminal(
    _model: &str,
    response: reqwest::Response,
    semantic_idle_timeout: std::time::Duration,
    attempt_deadline: tokio::time::Instant,
    retention_limits: crate::streaming::StreamRetentionLimits,
) -> CollectedResponse {
    collect_events_with_terminal(
        native_stream(response),
        semantic_idle_timeout,
        attempt_deadline,
        retention_limits,
    )
    .await
}

async fn collect_events_with_terminal(
    mut events: NativeStream,
    semantic_idle_timeout: std::time::Duration,
    attempt_deadline: tokio::time::Instant,
    retention_limits: crate::streaming::StreamRetentionLimits,
) -> CollectedResponse {
    let mut collector = CollectorState::new(retention_limits);
    loop {
        let Some(semantic_idle_deadline) =
            tokio::time::Instant::now().checked_add(semantic_idle_timeout)
        else {
            return CollectedResponse {
                result: Err(timeout_error()),
                native_usage: None,
            };
        };
        let event = tokio::select! {
            event = events.next() => event,
            _ = tokio::time::sleep_until(semantic_idle_deadline) => {
                return CollectedResponse { result: Err(timeout_error()), native_usage: None };
            }
            _ = tokio::time::sleep_until(attempt_deadline) => {
                return CollectedResponse { result: Err(timeout_error()), native_usage: None };
            }
        };
        let Some(event) = event else { break };
        let mut event = match event {
            Ok(event) => event,
            Err(error) => {
                return CollectedResponse {
                    result: Err(error),
                    native_usage: None,
                }
            }
        };
        match event["type"].as_str() {
            Some("error" | "response.failed") => {
                return CollectedResponse {
                    result: Err(auth_error(502, "upstream response failed")),
                    native_usage: event
                        .get("response")
                        .filter(|value| value.is_object())
                        .and_then(|response| {
                            crate::usage::compact_native_response_usage(
                                crate::reasoning::WireFormat::OpenAiResponses,
                                response,
                            )
                        }),
                }
            }
            Some("response.output_item.done") if event["item"].is_object() => {
                if let Some(index) = event["output_index"].as_u64() {
                    let item = event["item"].take();
                    if let Err(error) = collector.retain_item(index, item) {
                        return CollectedResponse {
                            result: Err(error),
                            native_usage: None,
                        };
                    }
                }
            }
            Some("response.output_text.done") if event["text"].is_string() => {
                if let (Some(output), Some(content)) = (
                    event["output_index"].as_u64(),
                    event["content_index"].as_u64(),
                ) {
                    let text = event["text"].take();
                    if let Err(error) = collector.retain_text(output, content, text) {
                        return CollectedResponse {
                            result: Err(error),
                            native_usage: None,
                        };
                    }
                }
            }
            Some("response.completed" | "response.incomplete") => {
                let terminal_validation = terminal(&event);
                let response = event["response"].take();
                let native_usage = crate::usage::compact_native_response_usage(
                    crate::reasoning::WireFormat::OpenAiResponses,
                    &response,
                );
                let result = terminal_validation.and_then(|_| collector.finish(response));
                return CollectedResponse {
                    result,
                    native_usage,
                };
            }
            _ => {}
        }
    }
    CollectedResponse {
        result: Err(stream_error("missing terminal response")),
        native_usage: None,
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
    use mockito::Server;
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        task::{Context, Poll},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct DropTrackedEvents {
        events: VecDeque<Result<Value>>,
        dropped: Arc<AtomicBool>,
    }

    impl Stream for DropTrackedEvents {
        type Item = Result<Value>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.events.pop_front())
        }
    }

    impl Drop for DropTrackedEvents {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn retention_limits(bytes: usize, entries: usize) -> crate::streaming::StreamRetentionLimits {
        crate::streaming::StreamRetentionLimits::new(bytes, entries, 4 * 1024, 64).unwrap()
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| format!("data:{event}\n\n"))
            .collect()
    }

    async fn collect(
        events: Vec<Value>,
        limits: crate::streaming::StreamRetentionLimits,
    ) -> CollectedResponse {
        let mut server = Server::new_async().await;
        let upstream = server
            .mock("GET", "/responses")
            .with_header("content-type", "text/event-stream")
            .with_body(sse(&events))
            .create_async()
            .await;
        let response = reqwest::Client::new()
            .get(format!("{}/responses", server.url()))
            .send()
            .await
            .unwrap();
        let collected = collect_response_with_terminal(
            "test-model",
            response,
            std::time::Duration::from_secs(1),
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            limits,
        )
        .await;
        upstream.assert_async().await;
        collected
    }

    fn completed(response: Value) -> Value {
        json!({"type": "response.completed", "response": response})
    }

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
            crate::streaming::StreamRetentionLimits::default(),
        )
        .await;
        assert!(matches!(
            collected.result,
            Err(ShimError::ProviderError { status: 504, .. })
        ));
        assert!(collected.native_usage.is_none());
    }

    #[tokio::test]
    async fn many_sparse_output_indexes_stop_at_the_entry_limit() {
        let collected = collect(
            vec![
                json!({"type":"response.output_item.done", "output_index":0, "item":{"type":"function_call"}}),
                json!({"type":"response.output_item.done", "output_index":u64::MAX / 2, "item":{"type":"function_call"}}),
                json!({"type":"response.output_item.done", "output_index":u64::MAX, "item":{"type":"function_call"}}),
            ],
            retention_limits(8 * 1024, 8),
        )
        .await;
        assert!(matches!(
            collected.result,
            Err(ShimError::Stream(ref message))
                if message == crate::stream_retention::RETENTION_ERROR
        ));
        assert!(collected.native_usage.is_none());
    }

    #[tokio::test]
    async fn retention_failure_drops_the_event_source() {
        let dropped = Arc::new(AtomicBool::new(false));
        let events = (0..3)
            .map(|output_index| {
                Ok(json!({
                    "type":"response.output_item.done",
                    "output_index":output_index,
                    "item":{"type":"function_call"}
                }))
            })
            .collect();
        let source = DropTrackedEvents {
            events,
            dropped: dropped.clone(),
        };
        let collected = collect_events_with_terminal(
            Box::pin(source),
            std::time::Duration::from_secs(1),
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            retention_limits(8 * 1024, 8),
        )
        .await;
        assert!(collected.result.is_err());
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn replacement_shrink_reclaims_retained_bytes() {
        let large_item = json!({
            "type":"message",
            "role":"assistant",
            "content":[{"type":"output_text", "text":"x".repeat(4_096)}]
        });
        let small_item =
            json!({"type":"function_call", "call_id":"call", "name":"lookup", "arguments":"{}"});
        let expected_response = json!({
            "status":"completed",
            "output":[small_item.clone()],
            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
        });
        let retained_large = crate::stream_retention::estimate_value(&large_item)
            .unwrap()
            .checked_add(crate::stream_retention::RetainedFootprint::record(
                size_of::<u64>(),
            ))
            .unwrap();
        let retained_terminal =
            crate::stream_retention::estimate_value(&expected_response).unwrap();
        let byte_limit = retained_large.bytes.max(retained_terminal.bytes) + 512;
        assert!(retained_large.bytes + retained_terminal.bytes > byte_limit);

        let collected = collect(
            vec![
                json!({"type":"response.output_item.done", "output_index":0, "item":large_item}),
                json!({"type":"response.output_item.done", "output_index":0, "item":small_item}),
                completed(json!({
                    "status":"completed",
                    "output":[],
                    "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
                })),
            ],
            retention_limits(byte_limit, 128),
        )
        .await;
        let response = collected.result.unwrap();
        assert_eq!(response, expected_response);
    }

    #[tokio::test]
    async fn completed_items_replace_text_fallback_without_changing_precedence() {
        let collected = collect(
            vec![
                json!({"type":"response.output_text.done", "output_index":u64::MAX, "content_index":9, "text":"discarded-before"}),
                json!({"type":"response.output_item.done", "output_index":u64::MAX, "item":{"type":"function_call", "call_id":"call", "name":"lookup", "arguments":"{}"}}),
                json!({"type":"response.output_text.done", "output_index":u64::MAX, "content_index":10, "text":"discarded-after"}),
                json!({"type":"response.output_text.done", "output_index":0, "content_index":2, "text":"second"}),
                json!({"type":"response.output_text.done", "output_index":0, "content_index":1, "text":"first"}),
                completed(json!({"status":"completed", "output":[]})),
            ],
            crate::streaming::StreamRetentionLimits::default(),
        )
        .await;
        let response = collected.result.unwrap();
        assert_eq!(response["output"][0]["content"][0]["text"], "first");
        assert_eq!(response["output"][0]["content"][1]["text"], "second");
        assert_eq!(response["output"][1]["type"], "function_call");
        assert_eq!(response.to_string().contains("discarded"), false);
    }

    #[tokio::test]
    async fn over_budget_terminal_preserves_only_compact_usage_evidence() {
        let collected = collect(
            vec![completed(json!({
                "status":"completed",
                "output":[{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"x".repeat(4_096)}]}],
                "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}
            }))],
            retention_limits(512, 64),
        )
        .await;
        assert!(matches!(
            collected.result,
            Err(ShimError::Stream(ref message))
                if message == crate::stream_retention::RETENTION_ERROR
        ));
        let native_usage = collected.native_usage.unwrap();
        assert_eq!(native_usage["usage"]["total_tokens"], 10);
        assert!(!native_usage.to_string().contains(&"x".repeat(128)));
    }
}

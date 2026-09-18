use super::{auth::auth_error, transform_response, translator};
use crate::{
    error::{Result, ShimError},
    provider::Provider,
};
use eventsource_stream::Eventsource;
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
    let events = Box::pin(response.bytes_stream().eventsource());
    Box::pin(futures::stream::unfold(
        (events, false),
        |(mut events, finished)| async move {
            if finished {
                return None;
            }
            loop {
                let value = match events.next().await {
                    Some(Ok(event)) if event.data.trim().is_empty() => continue,
                    Some(Ok(event)) if event.data.trim() == "[DONE]" => {
                        Err(stream_error("stream ended before a terminal response"))
                    }
                    Some(Ok(event)) => serde_json::from_str(&event.data)
                        .map_err(|_| stream_error("invalid SSE JSON")),
                    Some(Err(_)) => Err(stream_error("could not read upstream SSE")),
                    None => Err(stream_error("stream ended before a terminal response")),
                };
                let (value, finished) = match value {
                    Ok(value) => match terminal(&value) {
                        Ok(done) => (Ok(value), done),
                        Err(e) => (Err(e), true),
                    },
                    Err(e) => (Err(e), true),
                };
                return Some((value, (events, finished)));
            }
        },
    ))
}

pub(crate) async fn collect_response(model: &str, response: reqwest::Response) -> Result<Value> {
    let mut events = native_stream(response);
    let mut items = BTreeMap::new();
    let mut texts: BTreeMap<u64, BTreeMap<u64, Value>> = BTreeMap::new();
    while let Some(event) = events.next().await {
        let event = event?;
        match event["type"].as_str() {
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
                // Some backend versions put output only in output_item.done.
                if response["output"].as_array().is_none_or(Vec::is_empty) {
                    for (index, parts) in texts {
                        items.entry(index).or_insert_with(|| json!({"type": "message", "role": "assistant", "content": parts.into_values().collect::<Vec<_>>() }));
                    }
                    response["output"] = json!(items.into_values().collect::<Vec<_>>());
                }
                return transform_response(model, response);
            }
            _ => {}
        }
    }
    Err(stream_error("missing terminal response"))
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

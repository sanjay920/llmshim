//! `AttemptPolicy::observe_native`: an observer is handed each provider-native
//! response frame before llmshim normalizes it, and a policy that does not
//! implement it is unaffected.

use futures::StreamExt;
use llmshim::{
    client::ShimClient,
    policy::{
        AttemptEvent, AttemptIdentity, AttemptOutcome, AttemptPolicy, AttemptPolicyError,
        AttemptPolicyFuture, AttemptPolicyRefusal, DispatchPolicyContext, NativeFrame,
        PreparedAttempt,
    },
    providers::anthropic::Anthropic,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Debug, PartialEq)]
enum Seen {
    StreamData(String),
    ResponseBody(Value),
}

/// Admits everything and records every native frame it is shown.
#[derive(Default)]
struct FrameRecorder {
    frames: Mutex<Vec<Seen>>,
}

impl FrameRecorder {
    fn seen(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.frames.lock().unwrap())
    }
}

impl AttemptPolicy for FrameRecorder {
    fn acquire<'a>(
        &'a self,
        _attempt: &'a PreparedAttempt<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
        Box::pin(async { Ok(()) })
    }

    fn observe<'a>(
        &'a self,
        _attempt: &'a AttemptIdentity,
        _event: AttemptEvent<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
        Box::pin(async { Ok(()) })
    }

    fn observe_abandoned(
        &self,
        _attempt: &AttemptIdentity,
        _outcome: AttemptOutcome,
    ) -> Result<(), AttemptPolicyError> {
        Ok(())
    }

    fn observe_native(&self, _attempt: &AttemptIdentity, frame: NativeFrame<'_>) {
        let seen = match frame {
            NativeFrame::StreamData { data } => Seen::StreamData(data.to_string()),
            NativeFrame::ResponseBody { body } => Seen::ResponseBody(body.clone()),
            _ => return,
        };
        self.frames.lock().unwrap().push(seen);
    }
}

/// Admits everything and implements none of the optional methods.
struct DefaultsOnly;

impl AttemptPolicy for DefaultsOnly {
    fn acquire<'a>(
        &'a self,
        _attempt: &'a PreparedAttempt<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
        Box::pin(async { Ok(()) })
    }

    fn observe<'a>(
        &'a self,
        _attempt: &'a AttemptIdentity,
        _event: AttemptEvent<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
        Box::pin(async { Ok(()) })
    }

    fn observe_abandoned(
        &self,
        _attempt: &AttemptIdentity,
        _outcome: AttemptOutcome,
    ) -> Result<(), AttemptPolicyError> {
        Ok(())
    }
}

fn request() -> Value {
    json!({
        "model": "anthropic/claude-sonnet-5",
        "messages": [{"role": "user", "content": "answer"}],
    })
}

fn native_stream_events() -> Vec<String> {
    vec![
        json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":3}}})
            .to_string(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
            .to_string(),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}})
            .to_string(),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}})
            .to_string(),
        json!({"type":"message_stop"}).to_string(),
    ]
}

/// A stream's frames are the provider's own `data` payloads, every one and in
/// arrival order — Anthropic's `message_start`/`content_block_delta` events, not
/// the `chat.completion.chunk`s the caller is handed.
#[tokio::test]
async fn a_stream_hands_the_observer_every_native_data_payload_in_order() {
    let mut server = mockito::Server::new_async().await;
    let events = native_stream_events();
    let body: String = events
        .iter()
        .map(|event| format!("event: native\ndata: {event}\n\n"))
        .collect();
    let upstream = server
        .mock("POST", "/messages")
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .expect(1)
        .create_async()
        .await;
    let provider = Anthropic::new("test-key".into()).with_base_url(server.url());
    let recorder = Arc::new(FrameRecorder::default());
    let chunks: Vec<_> = ShimClient::new()
        .stream_with_policy(
            &provider,
            "claude-sonnet-5",
            &request(),
            &DispatchPolicyContext::new(recorder.clone()),
        )
        .await
        .unwrap()
        .collect()
        .await;

    upstream.assert_async().await;
    let normalized: Value = serde_json::from_str(chunks[0].as_ref().unwrap()).unwrap();
    assert_eq!(normalized["object"], "chat.completion.chunk");
    assert_eq!(
        recorder.seen(),
        events.into_iter().map(Seen::StreamData).collect::<Vec<_>>()
    );
}

/// A non-streaming response's frame is its native body, before the translation
/// into a chat completion the caller receives.
#[tokio::test]
async fn a_completion_hands_the_observer_its_native_body() {
    let mut server = mockito::Server::new_async().await;
    let native = json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-5",
        "content": [{"type": "text", "text": "hi"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 1},
    });
    let upstream = server
        .mock("POST", "/messages")
        .with_header("content-type", "application/json")
        .with_body(native.to_string())
        .expect(1)
        .create_async()
        .await;
    let provider = Anthropic::new("test-key".into()).with_base_url(server.url());
    let recorder = Arc::new(FrameRecorder::default());
    let answer = ShimClient::new()
        .completion_with_policy(
            &provider,
            "claude-sonnet-5",
            &request(),
            &DispatchPolicyContext::new(recorder.clone()),
        )
        .await
        .unwrap();

    upstream.assert_async().await;
    assert!(answer.get("choices").is_some(), "{answer}");
    assert_eq!(recorder.seen(), vec![Seen::ResponseBody(native)]);
}

/// The method is optional: a policy written before it existed dispatches the
/// same stream to the same chunks.
#[tokio::test]
async fn a_policy_without_the_method_sees_the_same_stream() {
    let mut chunks_by_policy = Vec::new();
    for policy in [
        Arc::new(FrameRecorder::default()) as Arc<dyn AttemptPolicy>,
        Arc::new(DefaultsOnly),
    ] {
        let mut server = mockito::Server::new_async().await;
        let body: String = native_stream_events()
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect();
        server
            .mock("POST", "/messages")
            .with_header("content-type", "text/event-stream")
            .with_body(body)
            .create_async()
            .await;
        let provider = Anthropic::new("test-key".into()).with_base_url(server.url());
        let chunks: Vec<String> = ShimClient::new()
            .stream_with_policy(
                &provider,
                "claude-sonnet-5",
                &request(),
                &DispatchPolicyContext::new(policy),
            )
            .await
            .unwrap()
            .map(|chunk| chunk.unwrap())
            .collect()
            .await;
        chunks_by_policy.push(chunks);
    }
    assert_eq!(chunks_by_policy[0], chunks_by_policy[1]);
}

/// `Debug` never prints a frame's contents: a prompt or an answer is not a log
/// line.
#[test]
fn a_frame_debugs_as_its_kind_alone() {
    let body = json!({"secret": "answer"});
    let printed = format!(
        "{:?} {:?}",
        NativeFrame::StreamData { data: "answer" },
        NativeFrame::ResponseBody { body: &body }
    );
    assert!(!printed.contains("answer"), "{printed}");
    assert!(printed.contains("StreamData") && printed.contains("ResponseBody"));
}

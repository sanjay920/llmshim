#![cfg(feature = "proxy")]

use futures::StreamExt;
use llmshim::{
    policy::{
        AttemptAccounting, AttemptEvent, AttemptIdentity, AttemptOutcome, AttemptPolicy,
        AttemptPolicyError, AttemptPolicyFuture, AttemptPolicyRefusal, AttemptUsageObservation,
        DispatchPolicyContext, PreparedAttempt,
    },
    providers::openai_compat::OpenAiCompatible,
    router::Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const DERIVED_RESPONSE_ERROR: &str = "upstream derived response metadata exceeds limit";

#[derive(Clone, Debug)]
enum PolicyEvent {
    Usage(Value),
    Finished(AttemptOutcome),
}

#[derive(Default)]
struct RecordingPolicy {
    events: Mutex<Vec<PolicyEvent>>,
}

impl AttemptPolicy for RecordingPolicy {
    fn acquire<'a>(
        &'a self,
        _attempt: &'a PreparedAttempt<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
        Box::pin(async { Ok(()) })
    }

    fn observe<'a>(
        &'a self,
        _attempt: &'a AttemptIdentity,
        event: AttemptEvent<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
        Box::pin(async move {
            match event {
                AttemptEvent::Usage { usage } => {
                    self.events
                        .lock()
                        .unwrap()
                        .push(PolicyEvent::Usage(usage.clone()));
                }
                AttemptEvent::Finished(outcome) => {
                    self.events
                        .lock()
                        .unwrap()
                        .push(PolicyEvent::Finished(outcome));
                }
                AttemptEvent::ResponseHeaders { .. } => {}
            }
            Ok(())
        })
    }

    fn observe_usage<'a>(
        &'a self,
        _attempt: &'a AttemptIdentity,
        observation: AttemptUsageObservation<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
        Box::pin(async move {
            self.events
                .lock()
                .unwrap()
                .push(PolicyEvent::Usage(observation.usage().clone()));
            Ok(())
        })
    }

    fn observe_abandoned(
        &self,
        _attempt: &AttemptIdentity,
        outcome: AttemptOutcome,
    ) -> Result<(), AttemptPolicyError> {
        self.events
            .lock()
            .unwrap()
            .push(PolicyEvent::Finished(outcome));
        Ok(())
    }
}

fn provider_response(blocks: usize) -> Value {
    json!({
        "id": "synthetic-response",
        "model": "synthetic-served-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "ok",
                "reasoning_details": (0..blocks).map(|index| json!({
                    "type": "reasoning.text",
                    "text": "r",
                    "id": format!("r{index}")
                })).collect::<Vec<_>>()
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
    })
}

fn router(base_url: &str) -> Router {
    Router::new().register(
        "vllm",
        Box::new(OpenAiCompatible::new("vllm", base_url, None)),
    )
}

#[tokio::test]
async fn direct_and_proxy_paths_refuse_amplification_after_one_dispatch() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_body(provider_response(130).to_string())
        .expect(2)
        .create_async()
        .await;
    let model = format!("vllm/{}", "m".repeat(65_536));
    let request = json!({"model":model,"messages":[{"role":"user","content":"test"}]});
    assert!(serde_json::to_vec(&request).unwrap().len() < 256 * 1024);
    assert!(serde_json::to_vec(&provider_response(130)).unwrap().len() < 256 * 1024);
    let policy = Arc::new(RecordingPolicy::default());
    let direct_router = router(&server.url());

    let direct_error = llmshim::completion_with_policy(
        &direct_router,
        &request,
        &DispatchPolicyContext::new(policy.clone()),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        direct_error,
        llmshim::error::ShimError::ProviderError { status: 502, ref body, .. }
            if body == DERIVED_RESPONSE_ERROR
    ));
    let events = policy.events.lock().unwrap().clone();
    assert!(matches!(
        events.as_slice(),
        [PolicyEvent::Usage(usage), PolicyEvent::Finished(AttemptOutcome::InvalidResponse {
            accounting: AttemptAccounting::UsageObserved
        })] if usage["prompt_tokens"] == 7 && usage["completion_tokens"] == 3
    ));

    let application = llmshim::proxy::app(router(&server.url()), None);
    let response = application
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
    let response_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let response_text = String::from_utf8(response_bytes.to_vec()).unwrap();
    assert!(response_text.contains(DERIVED_RESPONSE_ERROR));
    assert!(!response_text.contains(&"m".repeat(128)));
    upstream.assert_async().await;
}

#[tokio::test]
async fn public_direct_path_preserves_one_exact_long_origin_below_the_budget() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_body(provider_response(1).to_string())
        .expect(1)
        .create_async()
        .await;
    let native_model = "opaque/".to_owned() + &"m".repeat(16_384);
    let request = json!({
        "model": format!("vllm/{native_model}"),
        "messages":[{"role":"user","content":"test"}]
    });

    let response = llmshim::completion(&router(&server.url()), &request)
        .await
        .unwrap();

    assert_eq!(
        response["choices"][0]["message"]["reasoning"][0]["origin"]["model"],
        native_model
    );
    upstream.assert_async().await;
}

#[tokio::test]
async fn stream_usage_is_observed_before_a_derived_frame_refusal() {
    let mut server = mockito::Server::new_async().await;
    let terminal = json!({
        "id":"stream-response",
        "choices":[{
            "index":0,
            "delta":{
                "reasoning_details":(0..130).map(|index| json!({
                    "type":"reasoning.text",
                    "text":"r",
                    "id":format!("r{index}")
                })).collect::<Vec<_>>()
            },
            "finish_reason":"stop"
        }],
        "usage":{"prompt_tokens":11,"completion_tokens":5,"total_tokens":16}
    });
    let sse_body = format!("data: {terminal}\n\ndata: [DONE]\n\n");
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(sse_body)
        .expect(1)
        .create_async()
        .await;
    let model = format!("vllm/{}", "m".repeat(65_536));
    let request = json!({
        "model":model,
        "messages":[{"role":"user","content":"test"}],
        "stream":true
    });
    let policy = Arc::new(RecordingPolicy::default());

    let mut stream = llmshim::stream_with_policy(
        &router(&server.url()),
        &request,
        &DispatchPolicyContext::new(policy.clone()),
    )
    .await
    .unwrap();
    let error = stream.next().await.unwrap().unwrap_err();

    assert!(matches!(
        error,
        llmshim::error::ShimError::Stream(ref message) if message == DERIVED_RESPONSE_ERROR
    ));
    let events = policy.events.lock().unwrap().clone();
    assert!(matches!(
        events.as_slice(),
        [PolicyEvent::Usage(usage), PolicyEvent::Finished(AttemptOutcome::StreamFailure {
            accounting: AttemptAccounting::UsageObserved
        })] if usage["prompt_tokens"] == 11 && usage["completion_tokens"] == 5
    ));
    upstream.assert_async().await;
}

#[tokio::test]
async fn derived_refusal_does_not_dispatch_a_structured_output_repair() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_body(provider_response(130).to_string())
        .expect(1)
        .create_async()
        .await;
    let request = json!({
        "model":format!("vllm/{}", "m".repeat(65_536)),
        "messages":[{"role":"user","content":"test"}],
        "response_format":{
            "type":"json_schema",
            "json_schema":{
                "name":"answer",
                "schema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}
            }
        },
        "x-shim":{"structured_output":"prompt"}
    });

    let error = llmshim::completion(&router(&server.url()), &request)
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            llmshim::error::ShimError::ProviderError { status: 502, ref body, .. }
                if body == "provider could not complete the requested output contract"
        ),
        "{error:?}"
    );
    upstream.assert_async().await;
}

#[tokio::test]
async fn public_completion_rejects_a_native_signature_object_with_malformed_origin() {
    let mut server = mockito::Server::new_async().await;
    let body = json!({
        "id":"malformed-signature",
        "choices":[{
            "index":0,
            "message":{
                "role":"assistant",
                "content":null,
                "tool_calls":[{
                    "id":"native-call",
                    "type":"function",
                    "function":{"name":"read","arguments":"{}"},
                    "thought_signature":{"data":"opaque","origin":[]}
                }]
            },
            "finish_reason":"tool_calls"
        }],
        "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}
    });
    let upstream = server
        .mock("POST", "/chat/completions")
        .with_body(body.to_string())
        .expect(1)
        .create_async()
        .await;
    let request = json!({
        "model":"vllm/model",
        "messages":[{"role":"user","content":"test"}]
    });

    let error = llmshim::completion(&router(&server.url()), &request)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        llmshim::error::ShimError::ProviderError { status: 502, ref body, .. }
            if body == DERIVED_RESPONSE_ERROR
    ));
    upstream.assert_async().await;
}

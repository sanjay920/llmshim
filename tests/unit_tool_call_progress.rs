//! `AttemptPolicy::observe_tool_call_progress`: a streamed tool call is reported
//! while it is being written, in one normalized shape for every provider wire,
//! before the completed call reaches the stream's reader.

use futures::StreamExt;
use llmshim::{
    client::ShimClient,
    policy::{
        AttemptEvent, AttemptIdentity, AttemptOutcome, AttemptPolicy, AttemptPolicyError,
        AttemptPolicyFuture, AttemptPolicyRefusal, DispatchPolicyContext, PreparedAttempt,
    },
    provider::Provider,
    providers::{anthropic::Anthropic, openai_compat::OpenAiCompatible, openrouter::OpenRouter},
    reasoning::WireFormat,
    toolcall::{ToolCallProgress, ToolCallProgressEvent},
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
enum Step {
    Started(String),
    Arguments {
        text: String,
        appended: Option<String>,
    },
    Ended,
    Abandoned,
}

#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Progress {
        attempt: Uuid,
        index: u64,
        id: String,
        step: Step,
    },
    StreamFailed(Uuid),
    /// Pushed by the test when a chunk carrying this completed call arrives.
    Completed(String),
}

/// Admits everything and records progress, attempt failures and completed
/// calls on one timeline.
#[derive(Default)]
struct Recorder {
    timeline: Mutex<Vec<Seen>>,
}

impl Recorder {
    fn push(&self, seen: Seen) {
        self.timeline.lock().unwrap().push(seen);
    }

    fn timeline(&self) -> Vec<Seen> {
        self.timeline.lock().unwrap().clone()
    }

    fn steps(&self) -> Vec<(u64, Step)> {
        self.timeline()
            .into_iter()
            .filter_map(|seen| match seen {
                Seen::Progress { index, step, .. } => Some((index, step)),
                _ => None,
            })
            .collect()
    }
}

impl AttemptPolicy for Recorder {
    fn acquire<'a>(
        &'a self,
        _attempt: &'a PreparedAttempt<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyRefusal>> {
        Box::pin(async { Ok(()) })
    }

    fn observe<'a>(
        &'a self,
        attempt: &'a AttemptIdentity,
        event: AttemptEvent<'a>,
    ) -> AttemptPolicyFuture<'a, Result<(), AttemptPolicyError>> {
        if let AttemptEvent::Finished(AttemptOutcome::StreamFailure { .. }) = event {
            self.push(Seen::StreamFailed(attempt.id()));
        }
        Box::pin(async { Ok(()) })
    }

    fn observe_abandoned(
        &self,
        _attempt: &AttemptIdentity,
        _outcome: AttemptOutcome,
    ) -> Result<(), AttemptPolicyError> {
        Ok(())
    }

    fn observe_tool_call_progress(&self, attempt: &AttemptIdentity, progress: ToolCallProgress) {
        let step = match progress.event {
            ToolCallProgressEvent::Started { name } => Step::Started(name.into()),
            ToolCallProgressEvent::Arguments { text, appended } => Step::Arguments {
                text: text.into(),
                appended: appended.map(str::to_owned),
            },
            ToolCallProgressEvent::Ended => Step::Ended,
            ToolCallProgressEvent::Abandoned => Step::Abandoned,
            _ => return,
        };
        self.push(Seen::Progress {
            attempt: attempt.id(),
            index: progress.index,
            id: progress.id.into(),
            step,
        });
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

/// One provider wire: its scripted native stream and how to reach it.
struct Wire {
    path: &'static str,
    model: &'static str,
    events: Vec<Value>,
    provider: fn(String) -> Box<dyn Provider>,
}

fn anthropic() -> Wire {
    Wire {
        path: "/messages",
        model: "claude-sonnet-5",
        events: vec![
            json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":3}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"write_file","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a.txt\"}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
            json!({"type":"message_stop"}),
        ],
        provider: |url| Box::new(Anthropic::new("test-key".into()).with_base_url(url)),
    }
}

fn chat_events() -> Vec<Value> {
    vec![
        json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":""}}]},"finish_reason":null}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]},"finish_reason":null}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]},"finish_reason":null}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"id":"c","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":9,"total_tokens":12}}),
    ]
}

fn chat_completions() -> Wire {
    Wire {
        path: "/chat/completions",
        model: "served-model",
        events: chat_events(),
        provider: |url| Box::new(OpenAiCompatible::new("vllm", url, None)),
    }
}

fn openrouter() -> Wire {
    Wire {
        path: "/chat/completions",
        model: "vendor/model",
        events: chat_events(),
        provider: |url| Box::new(OpenRouter::new("test-key".into()).with_base_url(url)),
    }
}

fn responses() -> Wire {
    Wire {
        path: "/responses",
        model: "served-model",
        events: vec![
            json!({"type":"response.created","response":{"id":"resp_1"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":"}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"\"a.txt\"}"}),
            json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"path\":\"a.txt\"}"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file","arguments":"{\"path\":\"a.txt\"}"}}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file","arguments":"{\"path\":\"a.txt\"}"}],"usage":{"input_tokens":3,"output_tokens":9,"total_tokens":12}}}),
        ],
        provider: |url| {
            Box::new(
                OpenAiCompatible::new("vllm", url, None).with_wire(WireFormat::OpenAiResponses),
            )
        },
    }
}

fn sse(events: &[Value]) -> String {
    let mut body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn request(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "write a.txt"}],
        "tools": [{"type": "function", "function": {
            "name": "write_file",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
        }}],
    })
}

/// Streams `body` from a mock of `wire`, with `policy` if one is given,
/// recording each completed call on the recorder's timeline as its chunk
/// arrives. Returns every item.
async fn run(
    wire: &Wire,
    body: String,
    request: &Value,
    policy: Option<Arc<dyn AttemptPolicy>>,
    recorder: Option<&Recorder>,
) -> Vec<Result<Value, String>> {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", wire.path)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;
    let provider = (wire.provider)(server.url());
    let client = ShimClient::new();
    let mut stream = match policy {
        Some(policy) => {
            client
                .stream_with_policy(
                    provider.as_ref(),
                    wire.model,
                    request,
                    &DispatchPolicyContext::new(policy),
                )
                .await
        }
        None => client.stream(provider.as_ref(), wire.model, request).await,
    }
    .unwrap();
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        let item = item
            .map(|chunk| serde_json::from_str::<Value>(&chunk).unwrap())
            .map_err(|error| error.to_string());
        if let (Ok(chunk), Some(recorder)) = (&item, recorder) {
            for call in chunk["choices"][0]["delta"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
            {
                recorder.push(Seen::Completed(call["id"].as_str().unwrap().into()));
            }
        }
        items.push(item);
    }
    items
}

fn written(text: &str, appended: &str) -> Step {
    Step::Arguments {
        text: text.into(),
        appended: Some(appended.into()),
    }
}

/// Every wire reports the same steps for the same call: its name first, its
/// arguments as they grow, then its end, all before the completed call with
/// the same id reaches the reader, and all under one attempt.
#[tokio::test]
async fn each_wire_reports_a_call_being_written_before_the_completed_call() {
    for (label, wire) in [
        ("anthropic", anthropic()),
        ("chat completions", chat_completions()),
        ("openrouter", openrouter()),
        ("responses", responses()),
    ] {
        let recorder = Arc::new(Recorder::default());
        let items = run(
            &wire,
            sse(&wire.events),
            &request(wire.model),
            Some(recorder.clone()),
            Some(&recorder),
        )
        .await;
        assert!(items.iter().all(Result::is_ok), "{label}: {items:?}");

        assert_eq!(
            recorder.steps(),
            vec![
                (0, Step::Started("write_file".into())),
                (0, written("{\"path\":", "{\"path\":")),
                (0, written("{\"path\":\"a.txt\"}", "\"a.txt\"}")),
                (0, Step::Ended),
            ],
            "{label}"
        );
        let timeline = recorder.timeline();
        let Some(Seen::Completed(completed)) = timeline.last() else {
            panic!("{label}: the completed call must come last: {timeline:?}");
        };
        let mut attempts = std::collections::BTreeSet::new();
        for seen in &timeline[..timeline.len() - 1] {
            let Seen::Progress { attempt, id, .. } = seen else {
                panic!("{label}: unexpected {seen:?}");
            };
            assert_eq!(id, completed, "{label}: progress names the completed id");
            attempts.insert(*attempt);
        }
        assert_eq!(attempts.len(), 1, "{label}");
    }
}

/// Removes what is minted fresh for every response, so two runs compare.
fn without_minted_ids(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("id");
            object.remove("scope");
            object.values_mut().for_each(without_minted_ids);
        }
        Value::Array(items) => items.iter_mut().for_each(without_minted_ids),
        _ => {}
    }
}

/// Reporting is opt-in through the observer: a stream with no policy, a policy
/// that does not implement it, and one that does see the same chunks on every
/// wire.
#[tokio::test]
async fn the_stream_is_the_same_whether_or_not_progress_is_observed() {
    for wire in [anthropic(), chat_completions(), openrouter(), responses()] {
        let mut outputs = Vec::new();
        for policy in [
            None,
            Some(Arc::new(DefaultsOnly) as Arc<dyn AttemptPolicy>),
            Some(Arc::new(Recorder::default())),
        ] {
            let mut items: Vec<Value> =
                run(&wire, sse(&wire.events), &request(wire.model), policy, None)
                    .await
                    .into_iter()
                    .map(Result::unwrap)
                    .collect();
            items.iter_mut().for_each(without_minted_ids);
            outputs.push(items);
        }
        assert!(
            outputs[0]
                .iter()
                .any(|c| c.to_string().contains("tool_calls")),
            "{:?}",
            outputs[0]
        );
        assert_eq!(outputs[0], outputs[1]);
        assert_eq!(outputs[0], outputs[2]);
    }
}

/// A stream that fails mid-call abandons the call it was writing, under the
/// same attempt the failure is reported for, and no completed call follows.
#[tokio::test]
async fn a_failed_attempt_abandons_the_call_it_was_writing() {
    for (label, wire, cut) in [
        ("anthropic", anthropic(), 3),
        ("chat completions", chat_completions(), 2),
        ("openrouter", openrouter(), 2),
    ] {
        let mut events = wire.events[..cut].to_vec();
        events.push(json!({"error": {"message": "overloaded"}}));
        let recorder = Arc::new(Recorder::default());
        let items = run(
            &wire,
            sse(&events),
            &request(wire.model),
            Some(recorder.clone()),
            Some(&recorder),
        )
        .await;
        assert!(items.last().unwrap().is_err(), "{label}: {items:?}");

        let steps = recorder.steps();
        assert_eq!(
            steps.first(),
            Some(&(0, Step::Started("write_file".into()))),
            "{label}"
        );
        assert_eq!(steps.last(), Some(&(0, Step::Abandoned)), "{label}");
        let timeline = recorder.timeline();
        assert!(
            !timeline
                .iter()
                .any(|seen| matches!(seen, Seen::Completed(_))),
            "{label}"
        );
        let Some(Seen::Progress {
            attempt: abandoned_in,
            ..
        }) = timeline.iter().rev().find(|seen| {
            matches!(
                seen,
                Seen::Progress {
                    step: Step::Abandoned,
                    ..
                }
            )
        })
        else {
            unreachable!()
        };
        assert!(
            timeline.contains(&Seen::StreamFailed(*abandoned_in)),
            "{label}: {timeline:?}"
        );
    }
}

/// A forced answer tool is llmshim's own protocol for a structured answer, not
/// a call of the caller's, so it is never reported.
#[tokio::test]
async fn a_forced_answer_call_is_never_reported() {
    let wire = chat_completions();
    let events = vec![
        json!({"id":"c","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"final_output","arguments":"{\"n\":"}}]},"finish_reason":null}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"4}"}}]},"finish_reason":null}]}),
        json!({"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ];
    let request = json!({
        "model": wire.model,
        "messages": [{"role": "user", "content": "answer"}],
        "response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": {
            "type": "object",
            "properties": {"n": {"type": "integer"}},
            "required": ["n"],
            "additionalProperties": false
        }}},
        "x-shim": {"structured_output": "forced_tool"},
    });
    let recorder = Arc::new(Recorder::default());
    let items = run(&wire, sse(&events), &request, Some(recorder.clone()), None).await;

    let answer = items.last().unwrap().as_ref().unwrap();
    assert_eq!(answer["choices"][0]["delta"]["content"], "{\"n\":4}");
    assert_eq!(recorder.timeline(), vec![]);
}

/// `Debug` never prints a call's argument text: it is the model's output.
#[test]
fn progress_debugs_without_its_argument_text() {
    let printed = format!(
        "{:?}",
        ToolCallProgressEvent::Arguments {
            text: "{\"secret\":1}",
            appended: Some("{\"secret\":1}"),
        }
    );
    assert!(!printed.contains("secret"), "{printed}");
    assert!(printed.contains("Arguments"), "{printed}");
}

//! Provisional tool-call progress: what a stream has said about a call while
//! the call is still being written. The completed call, validated and emitted
//! once at termination, stays the only authoritative record.
use std::collections::BTreeSet;

/// One step of a tool call being written, reported while its stream is open.
///
/// Provisional by contract. A call's steps arrive in order: `Started`, then
/// zero or more `Arguments`, then `Ended` or `Abandoned`. `Ended` means the
/// completed call follows in the stream's output under the same `id`; it is
/// reported before that output is produced. `Abandoned` means it never will.
/// A stream that ends in an error delivers nothing further, so discard every
/// call of that attempt whose completed form did not arrive.
///
/// Non-exhaustive so it can grow: read its fields, and match
/// [`ToolCallProgressEvent`] with a wildcard arm.
#[non_exhaustive]
#[derive(Clone, Copy)]
pub struct ToolCallProgress<'a> {
    /// The choice the call belongs to; 0 for a single-choice stream.
    pub choice: u64,
    /// The call's position in its choice, as the completed call's `index`.
    pub index: u64,
    /// The `id` the completed call will carry, known from the first step.
    pub id: &'a str,
    pub event: ToolCallProgressEvent<'a>,
}

#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToolCallProgressEvent<'a> {
    /// The tool's name is known. Always a call's first step.
    Started { name: &'a str },
    /// The argument text received so far, which is partial JSON until the call
    /// ends. `appended` is what this step added to the previous step's `text`,
    /// or `None` when the provider replaced the text instead of extending it.
    Arguments {
        text: &'a str,
        appended: Option<&'a str>,
    },
    /// No more arguments: the completed call follows.
    Ended,
    /// The call will not complete; discard what was shown for it.
    Abandoned,
}

impl std::fmt::Debug for ToolCallProgress<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolCallProgress")
            .field("choice", &self.choice)
            .field("index", &self.index)
            .field("id", &self.id)
            .field("event", &self.event)
            .finish()
    }
}

/// Argument text is a model's output, so it is never printed.
impl std::fmt::Debug for ToolCallProgressEvent<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Started { name } => formatter
                .debug_struct("Started")
                .field("name", name)
                .finish(),
            Self::Arguments { text, .. } => formatter
                .debug_struct("Arguments")
                .field("bytes", &text.len())
                .finish_non_exhaustive(),
            Self::Ended => formatter.write_str("Ended"),
            Self::Abandoned => formatter.write_str("Abandoned"),
        }
    }
}

pub(crate) type ProgressSink = Box<dyn for<'a> FnMut(ToolCallProgress<'a>) + Send>;

/// What a stream knows about one call, borrowed from its assembly state.
/// `arguments` is the text a reader may see: empty while it is a placeholder.
pub(crate) struct CallView<'a> {
    pub key: &'a str,
    pub choice: u64,
    pub index: u64,
    pub id: &'a str,
    pub name: &'a str,
    pub arguments: &'a str,
}

/// The visible argument text before an update: its length, and whether the
/// update keeps it as a prefix of the text after.
#[derive(Clone, Copy)]
pub(crate) struct ArgumentsBefore {
    pub len: usize,
    pub kept: bool,
}

/// Turns assembly updates into progress steps for one sink.
pub(crate) struct ProgressReporter {
    sink: ProgressSink,
    started: BTreeSet<String>,
}

impl ProgressReporter {
    pub(crate) fn new(sink: ProgressSink) -> Self {
        Self {
            sink,
            started: BTreeSet::new(),
        }
    }

    /// After one update to `call`. A name still arriving in fragments is not
    /// settled, so the call starts on its first other update.
    pub(crate) fn updated(&mut self, call: &CallView, name_settled: bool, before: ArgumentsBefore) {
        if !self.started.contains(call.key) {
            if name_settled && !call.name.is_empty() {
                self.start(call);
            }
            return;
        }
        if before.kept && call.arguments.len() == before.len {
            return;
        }
        let appended = before
            .kept
            .then(|| call.arguments.get(before.len..))
            .flatten();
        self.emit(
            call,
            ToolCallProgressEvent::Arguments {
                text: call.arguments,
                appended,
            },
        );
    }

    pub(crate) fn ended(&mut self, call: &CallView) {
        if !self.started.contains(call.key) {
            self.start(call);
        }
        self.started.remove(call.key);
        self.emit(call, ToolCallProgressEvent::Ended);
    }

    pub(crate) fn abandoned(&mut self, call: &CallView) {
        if self.started.remove(call.key) {
            self.emit(call, ToolCallProgressEvent::Abandoned);
        }
    }

    fn start(&mut self, call: &CallView) {
        self.started.insert(call.key.to_owned());
        self.emit(call, ToolCallProgressEvent::Started { name: call.name });
        if !call.arguments.is_empty() {
            self.emit(
                call,
                ToolCallProgressEvent::Arguments {
                    text: call.arguments,
                    appended: Some(call.arguments),
                },
            );
        }
    }

    fn emit(&mut self, call: &CallView, event: ToolCallProgressEvent) {
        (self.sink)(ToolCallProgress {
            choice: call.choice,
            index: call.index,
            id: call.id,
            event,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reasoning::{ReplayTarget, WireFormat};
    use crate::toolcall::{ToolDelta, ToolStream, ToolUpdate};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<(u64, String, String)>>>;

    /// A stream on `wire` whose progress is logged as (index, id, step).
    fn recorded(wire: WireFormat) -> (ToolStream, Log) {
        let log: Log = Arc::default();
        let mut stream = ToolStream::new(ReplayTarget::new("test", "model", wire));
        let sink = log.clone();
        stream.report_progress(Box::new(move |progress| {
            let step = match progress.event {
                ToolCallProgressEvent::Started { name } => format!("started {name}"),
                ToolCallProgressEvent::Arguments { text, appended } => {
                    format!("arguments {text} appended {appended:?}")
                }
                ToolCallProgressEvent::Ended => "ended".into(),
                ToolCallProgressEvent::Abandoned => "abandoned".into(),
            };
            let entry = (progress.index, progress.id.to_owned(), step);
            sink.lock().unwrap().push(entry);
        }));
        (stream, log)
    }

    fn steps(log: &Log) -> Vec<(u64, String)> {
        log.lock()
            .unwrap()
            .iter()
            .map(|(index, _, step)| (*index, step.clone()))
            .collect()
    }

    /// A provider that resends a call's whole arguments replaces the text, so
    /// the step carries no appended part; dropping the stream abandons it.
    #[test]
    fn replaced_arguments_have_no_appended_part_and_a_dropped_stream_abandons() {
        let (mut stream, log) = recorded(WireFormat::GoogleGenerateContent);
        for args in [json!({"a": 1}), json!({"b": 2})] {
            let frame = json!({"candidates":[{"index":0,"content":{"parts":[
                {"functionCall":{"id":"f1","name":"read","args":args}}
            ]}}]});
            stream.push(&frame, None).unwrap();
        }
        drop(stream);
        assert_eq!(
            steps(&log),
            [
                "started read",
                r#"arguments {"a":1} appended Some("{\"a\":1}")"#,
                r#"arguments {"b":2} appended None"#,
                "abandoned",
            ]
            .map(|step| (0, step.to_owned()))
        );
    }

    /// A name that arrives in fragments starts the call once, whole.
    #[test]
    fn a_fragmented_name_starts_the_call_once_it_is_whole() {
        let (mut stream, log) = recorded(WireFormat::OpenAiChat);
        for update in [
            ToolUpdate::WireIdFragment("call_1".into()),
            ToolUpdate::NameFragment("write_".into()),
            ToolUpdate::NameFragment("file".into()),
            ToolUpdate::ArgumentsFragment("{}".into()),
        ] {
            stream
                .apply(ToolDelta {
                    part_id: "chat:0:0".into(),
                    choice: 0,
                    index: 0,
                    update,
                })
                .unwrap();
        }
        assert_eq!(
            steps(&log),
            ["started write_file", r#"arguments {} appended Some("{}")"#]
                .map(|step| (0, step.to_owned()))
        );
    }

    /// Parallel calls end in index order, each under the id its completed call
    /// carries, in the frame that completes them.
    #[test]
    fn parallel_calls_end_in_order_under_their_completed_ids() {
        let (mut stream, log) = recorded(WireFormat::OpenAiChat);
        let call = |index: u64, id: Option<&str>, name: Option<&str>, args: &str| {
            json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":index,"id":id,"function":{"name":name,"arguments":args}}
            ]}}]})
        };
        for frame in [
            call(0, Some("a"), Some("read"), "{\"p\":"),
            call(1, Some("b"), Some("list"), "{}"),
            call(0, None, None, "1}"),
        ] {
            stream.push(&frame, None).unwrap();
        }
        let terminal = json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
        let chunk = stream
            .push(&terminal, Some(terminal.to_string()))
            .unwrap()
            .unwrap();
        let chunk: Value = serde_json::from_str(&chunk).unwrap();

        let log = log.lock().unwrap().clone();
        let ended: Vec<_> = log.iter().filter(|(_, _, step)| step == "ended").collect();
        assert_eq!(ended.len(), 2, "{log:?}");
        for (position, (index, id, _)) in ended.into_iter().enumerate() {
            assert_eq!(*index, position as u64);
            assert_eq!(
                chunk["choices"][0]["delta"]["tool_calls"][position]["id"],
                *id
            );
        }
        assert_eq!(
            log.iter()
                .filter(|(index, _, _)| *index == 0)
                .map(|(_, _, step)| step.as_str())
                .collect::<Vec<_>>(),
            [
                "started read",
                r#"arguments {"p": appended Some("{\"p\":")"#,
                r#"arguments {"p":1} appended Some("1}")"#,
                "ended"
            ]
        );
    }
}

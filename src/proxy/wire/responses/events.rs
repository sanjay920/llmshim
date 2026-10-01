//! Numbered Responses events retain typed output until serialization.
use super::{OutputItem, Response};
use axum::response::sse::Event;
use serde_json::{json, Map, Value};

pub(super) struct Events {
    pub response: Response,
    sequence: u64,
    text: Option<usize>,
}
impl Events {
    pub(super) fn new(mut response: Response) -> Self {
        response
            .fields
            .insert("status".into(), json!("in_progress"));
        response.fields.insert("usage".into(), Value::Null);
        Self {
            response,
            sequence: 0,
            text: None,
        }
    }
    pub(super) fn event(&mut self, kind: &str, mut data: Value) -> Event {
        data["type"] = json!(kind);
        data["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        Event::default().event(kind).data(data.to_string())
    }
    pub(super) fn text(&mut self, text: &str) -> Vec<Event> {
        let mut events = Vec::new();
        let index = match self.text {
            Some(index) => index,
            None => {
                let index = self.response.output.len();
                let suffix = self.response.id.trim_start_matches("resp_");
                let item = OutputItem {
                    id: format!("msg_{suffix}_{index}"),
                    kind: "message".into(),
                    content: Some(Vec::new()),
                    fields: Map::from_iter([
                        ("status".into(), json!("in_progress")),
                        ("role".into(), json!("assistant")),
                    ]),
                };
                self.response.output.push(item.clone());
                self.text = Some(index);
                events.push(self.event(
                    "response.output_item.added",
                    json!({"output_index": index, "item": item}),
                ));
                events.push(self.event(
                    "response.content_part.added",
                    json!({
                        "output_index": index,
                        "item_id": item.id,
                        "content_index": 0,
                        "part": {"type": "output_text", "text": "", "annotations": []},
                    }),
                ));
                index
            }
        };
        events.push(self.event(
            "response.output_text.delta",
            json!({
                "output_index": index,
                "item_id": self.response.output[index].id,
                "content_index": 0,
                "delta": text,
                "logprobs": [],
            }),
        ));
        events
    }
    pub(super) fn finish(&mut self, native: Response) -> Vec<Event> {
        let mut events = Vec::new();
        for (index, item) in native.output.iter().enumerate() {
            if Some(index) != self.text {
                let mut added = item.clone();
                added.fields.insert("status".into(), json!("in_progress"));
                if item.kind == "function_call" {
                    added.fields.insert("arguments".into(), json!(""));
                }
                events.push(self.event(
                    "response.output_item.added",
                    json!({"output_index": index, "item": added}),
                ));
            }
            match item.kind.as_str() {
                "message" => {
                    for (part_index, part) in item.content.iter().flatten().enumerate() {
                        if part["type"] == "output_text" {
                            events.push(self.event(
                                "response.output_text.done",
                                json!({
                                    "output_index": index,
                                    "item_id": item.id,
                                    "content_index": part_index,
                                    "text": part["text"],
                                    "logprobs": [],
                                }),
                            ));
                        }
                        events.push(self.event(
                            "response.content_part.done",
                            json!({
                                "output_index": index,
                                "item_id": item.id,
                                "content_index": part_index,
                                "part": part,
                            }),
                        ));
                    }
                }
                "function_call" => {
                    events.push(self.event(
                        "response.function_call_arguments.delta",
                        json!({
                            "output_index": index,
                            "item_id": item.id,
                            "delta": item.fields.get("arguments"),
                        }),
                    ));
                    events.push(self.event(
                        "response.function_call_arguments.done",
                        json!({
                            "output_index": index,
                            "item_id": item.id,
                            "arguments": item.fields.get("arguments"),
                            "name": item.fields.get("name"),
                        }),
                    ));
                }
                _ => {}
            }
            events.push(self.event(
                "response.output_item.done",
                json!({"output_index": index, "item": item}),
            ));
        }
        let kind = if native.fields.get("status") == Some(&json!("incomplete")) {
            "response.incomplete"
        } else {
            "response.completed"
        };
        events.push(self.event(kind, json!({"response": native})));
        events
    }
    pub(super) fn failed(&mut self, message: &str) -> Event {
        self.response
            .fields
            .insert("status".into(), json!("failed"));
        self.response.fields.insert(
            "error".into(),
            json!({"code": "server_error", "message": message}),
        );
        self.event("response.failed", json!({"response": self.response}))
    }
}

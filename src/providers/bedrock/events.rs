//! Stateful Converse events projected into the shared Messages vocabulary.
use super::{response, BedrockError};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const MAX_BLOCKS: usize = 4096;

pub(super) struct Projection {
    blocks: BTreeMap<u64, &'static str>,
    model: String,
    pub(super) request_id: String,
    redacted: BTreeMap<u64, Vec<u8>>,
    redacted_bytes: usize,
    started: bool,
    stop: Option<String>,
    pub completed: bool,
}

impl Projection {
    pub fn new(model: &str) -> Self {
        Self {
            blocks: BTreeMap::new(),
            model: model.into(),
            request_id: format!("bedrock-{}", uuid::Uuid::new_v4()),
            redacted: BTreeMap::new(),
            redacted_bytes: 0,
            started: false,
            stop: None,
            completed: false,
        }
    }
    pub fn event(
        &mut self,
        kind: &str,
        value: Value,
        out: &mut Vec<String>,
    ) -> Result<(), BedrockError> {
        if self.completed || (!self.started && kind != "messageStart") {
            return Err(BedrockError::protocol());
        }
        let event = match kind {
            "messageStart" if !self.started && value["role"] == "assistant" => {
                self.started = true;
                json!({"type":"message_start","message":{"id":self.request_id,"model":self.model,
                    "role":"assistant","content":[]}})
            }
            "contentBlockStart" if self.stop.is_none() => {
                let index = value["contentBlockIndex"]
                    .as_u64()
                    .ok_or_else(BedrockError::protocol)?;
                if self.blocks.len() >= MAX_BLOCKS || self.blocks.contains_key(&index) {
                    return Err(BedrockError::protocol());
                }
                let tool = &value["start"]["toolUse"];
                if !tool["toolUseId"].is_string() || !tool["name"].is_string() {
                    return Err(BedrockError::protocol());
                }
                self.blocks.insert(index, "tool");
                json!({"type":"content_block_start","index":index,"content_block":{
                    "type":"tool_use","id":tool["toolUseId"],"name":tool["name"],"input":{}}})
            }
            "contentBlockDelta" if self.stop.is_none() => {
                let index = value["contentBlockIndex"]
                    .as_u64()
                    .ok_or_else(BedrockError::protocol)?;
                let delta = &value["delta"];
                if let Some(data) = delta
                    .pointer("/reasoningContent/redactedContent")
                    .and_then(Value::as_str)
                {
                    self.redacted_delta(index, data)?;
                    return Ok(());
                }
                let (kind, delta) = if let Some(text) = delta.get("text").filter(|v| v.is_string())
                {
                    ("text", json!({"type":"text_delta","text":text}))
                } else if let Some(input) =
                    delta.pointer("/toolUse/input").filter(|v| v.is_string())
                {
                    (
                        "tool",
                        json!({"type":"input_json_delta","partial_json":input}),
                    )
                } else if let Some(reasoning) = delta.get("reasoningContent") {
                    if let Some(text) = reasoning.get("text").filter(|v| v.is_string()) {
                        ("thinking", json!({"type":"thinking_delta","thinking":text}))
                    } else if let Some(signature) =
                        reasoning.get("signature").filter(|v| v.is_string())
                    {
                        (
                            "thinking",
                            json!({"type":"signature_delta","signature":signature}),
                        )
                    } else {
                        return Err(BedrockError::protocol());
                    }
                } else {
                    return Err(BedrockError::protocol());
                };
                match self.blocks.get(&index) {
                    Some(previous) if *previous != kind => return Err(BedrockError::protocol()),
                    Some(_) => {}
                    None if kind == "tool" || self.blocks.len() >= MAX_BLOCKS => {
                        return Err(BedrockError::protocol())
                    }
                    None => {
                        self.blocks.insert(index, kind);
                        let block = if kind == "thinking" {
                            json!({"type":"thinking","thinking":""})
                        } else {
                            json!({"type":"text","text":""})
                        };
                        self.emit(
                            json!({"type":"content_block_start","index":index,
                            "content_block":block}),
                            out,
                        );
                    }
                }
                json!({"type":"content_block_delta","index":index,"delta":delta})
            }
            "contentBlockStop" if self.stop.is_none() => {
                let index = value["contentBlockIndex"]
                    .as_u64()
                    .ok_or_else(BedrockError::protocol)?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(BedrockError::protocol)?;
                if *block == "closed" {
                    return Err(BedrockError::protocol());
                }
                let redacted = *block == "redacted";
                *block = "closed";
                if redacted {
                    let data = self
                        .redacted
                        .remove(&index)
                        .ok_or_else(BedrockError::protocol)?;
                    self.redacted_bytes -= data.len();
                    let data = base64::engine::general_purpose::STANDARD.encode(data);
                    self.emit(
                        json!({"type":"content_block_start","index":index,
                        "content_block":{"type":"redacted_thinking","data":data}}),
                        out,
                    );
                }
                json!({"type":"content_block_stop","index":index})
            }
            "messageStop"
                if self.stop.is_none() && self.blocks.values().all(|kind| *kind == "closed") =>
            {
                response::stop_reason(&value["stopReason"])?;
                self.stop = value["stopReason"].as_str().map(str::to_owned);
                return Ok(());
            }
            "metadata" if self.stop.is_some() => {
                if !value["usage"].is_object() {
                    return Err(BedrockError::protocol());
                }
                let stop = match self.stop.as_deref() {
                    Some("max_tokens" | "model_context_window_exceeded") => "max_tokens",
                    Some("tool_use") => "tool_use",
                    Some(
                        "end_turn" | "stop_sequence" | "guardrail_intervened" | "content_filtered",
                    ) => "end_turn",
                    _ => return Err(BedrockError::protocol()),
                };
                self.emit(
                    json!({"type":"message_delta",
                    "delta":{"stop_reason":stop,"bedrock_stop":self.stop},
                    "usage":response::usage(&value["usage"])}),
                    out,
                );
                self.completed = true;
                json!({"type":"message_stop"})
            }
            _ => return Err(BedrockError::protocol()),
        };
        self.emit(event, out);
        Ok(())
    }
    fn emit(&self, mut event: Value, out: &mut Vec<String>) {
        event["id"] = json!(self.request_id);
        out.push(event.to_string());
    }

    fn redacted_delta(&mut self, index: u64, data: &str) -> Result<(), BedrockError> {
        match self.blocks.get(&index) {
            Some(kind) if *kind != "redacted" => return Err(BedrockError::protocol()),
            None if self.blocks.len() >= MAX_BLOCKS => return Err(BedrockError::protocol()),
            _ => {}
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| BedrockError::protocol())?;
        if decoded.len() > (4 * 1024 * 1024usize).saturating_sub(self.redacted_bytes) {
            return Err(BedrockError::protocol());
        }
        self.redacted_bytes += decoded.len();
        self.blocks.insert(index, "redacted");
        self.redacted
            .entry(index)
            .or_default()
            .extend_from_slice(&decoded);
        Ok(())
    }
}

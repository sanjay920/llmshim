use super::*;
use crate::reasoning::ThoughtSignature;

/// One internal delta vocabulary. Transport parsers only select a stable part
/// and emit these operations; JSON argument assembly is implemented once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDelta {
    pub part_id: String,
    pub choice: u64,
    pub index: u64,
    pub update: ToolUpdate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ToolUpdate {
    Name(String),
    NameFragment(String),
    WireId(String),
    WireIdFragment(String),
    ItemId(String),
    ArgumentsStart(String),
    ArgumentsFragment(String),
    Arguments(String),
    Signature(String),
    SignatureField(String),
    End,
}

struct Part {
    id: String,
    choice: u64,
    index: u64,
    name: String,
    wire_id: Option<String>,
    item_id: Option<String>,
    arguments: String,
    initial: bool,
    signature: Option<ThoughtSignature>,
    signature_field: Option<String>,
    ended: bool,
}

/// Per-response stream state. Complete calls are exposed only after terminal
/// metadata, so no caller executes partial JSON or misses a trailing signature.
pub struct ToolStream {
    target: ReplayTarget,
    scope: String,
    parts: BTreeMap<String, Part>,
    gemini_last: BTreeMap<u64, (String, u64, usize)>,
    gemini_by_id: BTreeMap<(u64, String), String>,
    frame: u64,
    gemini_next: BTreeMap<u64, u64>,
    emitted: BTreeSet<u64>,
}
impl ToolStream {
    pub fn new(target: ReplayTarget) -> Self {
        Self {
            target,
            scope: uuid::Uuid::new_v4().to_string(),
            parts: BTreeMap::new(),
            gemini_last: BTreeMap::new(),
            gemini_by_id: BTreeMap::new(),
            frame: 0,
            gemini_next: BTreeMap::new(),
            emitted: BTreeSet::new(),
        }
    }

    pub fn apply(&mut self, delta: ToolDelta) -> Result<()> {
        if self.emitted.contains(&delta.choice) {
            return Err(upstream("tool data arrived after completion"));
        }
        if !self.parts.contains_key(&delta.part_id) {
            self.parts.insert(
                delta.part_id.clone(),
                Part {
                    id: mint(),
                    choice: delta.choice,
                    index: delta.index,
                    name: String::new(),
                    wire_id: None,
                    item_id: None,
                    arguments: String::new(),
                    initial: false,
                    signature: None,
                    signature_field: None,
                    ended: false,
                },
            );
        }
        let part = self.parts.get_mut(&delta.part_id).unwrap();
        match delta.update {
            ToolUpdate::Name(v) => part.name = v,
            ToolUpdate::NameFragment(v) => part.name.push_str(&v),
            ToolUpdate::WireId(v) => part.wire_id = Some(v),
            ToolUpdate::WireIdFragment(v) => {
                part.wire_id.get_or_insert_with(String::new).push_str(&v)
            }
            ToolUpdate::ItemId(v) => part.item_id = Some(v),
            ToolUpdate::ArgumentsStart(v) => {
                part.arguments = v;
                part.initial = true;
            }
            ToolUpdate::ArgumentsFragment(v) => {
                if part.ended {
                    return Err(upstream("argument delta followed a completed tool part"));
                }
                if part.initial {
                    part.arguments.clear();
                    part.initial = false;
                }
                part.arguments.push_str(&v);
            }
            ToolUpdate::Arguments(v) => {
                if part.arguments != v {
                    part.ended = false;
                }
                part.arguments = v;
                part.initial = false;
            }
            ToolUpdate::Signature(v) => {
                part.signature = Some(ThoughtSignature {
                    data: v,
                    origin: self.target.origin(),
                })
            }
            ToolUpdate::SignatureField(v) => part.signature_field = Some(v),
            ToolUpdate::End => {
                validate_arguments(&part.arguments, true)?;
                part.ended = true;
            }
        }
        Ok(())
    }

    fn update(&mut self, part: &str, choice: u64, index: u64, update: ToolUpdate) -> Result<()> {
        self.apply(ToolDelta {
            part_id: part.into(),
            choice,
            index,
            update,
        })
    }

    /// Parse transport-specific fields into ToolDelta operations.
    fn parse(&mut self, event: &Value) -> Result<()> {
        self.frame += 1;
        match self.target.wire {
            WireFormat::AnthropicMessages => {
                let index = event["index"].as_u64().unwrap_or(0);
                let part = format!("anthropic:{index}");
                match event["type"].as_str() {
                    Some("content_block_start") if event["content_block"]["type"] == "tool_use" => {
                        let b = &event["content_block"];
                        if let Some(id) = b["id"].as_str() {
                            self.update(&part, 0, index, ToolUpdate::WireId(id.into()))?;
                        }
                        if let Some(name) = b["name"].as_str() {
                            self.update(&part, 0, index, ToolUpdate::Name(name.into()))?;
                        }
                        self.update(
                            &part,
                            0,
                            index,
                            ToolUpdate::ArgumentsStart(
                                b.get("input").cloned().unwrap_or(json!({})).to_string(),
                            ),
                        )?;
                    }
                    Some("content_block_delta") if event["delta"]["type"] == "input_json_delta" => {
                        let text = event["delta"]["partial_json"]
                            .as_str()
                            .ok_or_else(|| upstream("argument fragment is not text"))?;
                        self.update(&part, 0, index, ToolUpdate::ArgumentsFragment(text.into()))?;
                    }
                    Some("content_block_stop") if self.parts.contains_key(&part) => {
                        self.update(&part, 0, index, ToolUpdate::End)?
                    }
                    _ => {}
                }
            }
            WireFormat::OpenAiResponses => {
                let index = event["output_index"].as_u64().unwrap_or(0);
                let part = format!("responses:{index}");
                match event["type"].as_str() {
                    Some("response.output_item.added" | "response.output_item.done")
                        if event["item"]["type"] == "function_call" =>
                    {
                        self.response_item(
                            &part,
                            index,
                            &event["item"],
                            event["type"] == "response.output_item.done",
                        )?;
                    }
                    Some("response.function_call_arguments.delta") => {
                        let text = event["delta"]
                            .as_str()
                            .ok_or_else(|| upstream("argument fragment is not text"))?;
                        self.update(&part, 0, index, ToolUpdate::ArgumentsFragment(text.into()))?;
                    }
                    Some("response.function_call_arguments.done") => {
                        if let Some(text) = event["arguments"].as_str() {
                            self.update(&part, 0, index, ToolUpdate::Arguments(text.into()))?;
                        }
                    }
                    Some("response.completed" | "response.incomplete") => {
                        for (i, item) in event["response"]["output"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .enumerate()
                        {
                            if item["type"] == "function_call" {
                                self.response_item(
                                    &format!("responses:{i}"),
                                    i as u64,
                                    item,
                                    true,
                                )?;
                            }
                        }
                    }
                    _ => {}
                }
            }
            WireFormat::OpenAiChat => {
                for (ci, choice) in event["choices"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let choice_index = choice["index"].as_u64().unwrap_or(ci as u64);
                    for call in choice["delta"]["tool_calls"]
                        .as_array()
                        .into_iter()
                        .flatten()
                    {
                        let explicit = call["index"].as_u64();
                        let known = call["id"].as_str().and_then(|id| {
                            self.parts
                                .values()
                                .find(|p| {
                                    p.choice == choice_index && p.wire_id.as_deref() == Some(id)
                                })
                                .map(|p| p.index)
                        });
                        let existing: Vec<_> = self
                            .parts
                            .values()
                            .filter(|p| p.choice == choice_index)
                            .map(|p| p.index)
                            .collect();
                        let index = if let Some(index) = explicit.or(known) {
                            index
                        } else if call["id"].is_string() {
                            existing.iter().max().map(|i| i + 1).unwrap_or(0)
                        } else if existing.len() == 1 {
                            existing[0]
                        } else {
                            return Err(upstream("tool fragment has no unambiguous part index"));
                        };
                        let part = format!("chat:{choice_index}:{index}");
                        let atomic = explicit.is_none()
                            && call["id"].is_string()
                            && call["function"]["name"].is_string()
                            && call["function"]["arguments"]
                                .as_str()
                                .is_some_and(|a| serde_json::from_str::<Value>(a).is_ok());
                        if let Some(id) = call["id"].as_str() {
                            self.update(
                                &part,
                                choice_index,
                                index,
                                if explicit.is_some() {
                                    ToolUpdate::WireIdFragment(id.into())
                                } else {
                                    ToolUpdate::WireId(id.into())
                                },
                            )?;
                        }
                        if let Some(name) = call["function"]["name"].as_str() {
                            self.update(
                                &part,
                                choice_index,
                                index,
                                if atomic {
                                    ToolUpdate::Name(name.into())
                                } else {
                                    ToolUpdate::NameFragment(name.into())
                                },
                            )?;
                        }
                        if let Some(args) = call["function"]["arguments"].as_str() {
                            self.update(
                                &part,
                                choice_index,
                                index,
                                if atomic {
                                    ToolUpdate::Arguments(args.into())
                                } else {
                                    ToolUpdate::ArgumentsFragment(args.into())
                                },
                            )?;
                        }
                        if call
                            .pointer("/extra_content/google/thought_signature")
                            .is_some()
                        {
                            self.update(
                                &part,
                                choice_index,
                                index,
                                ToolUpdate::SignatureField(
                                    "extra_content.google.thought_signature".into(),
                                ),
                            )?;
                        }
                        if let Some(sig) = call["thought_signature"].as_str().or_else(|| {
                            call.pointer("/extra_content/google/thought_signature")
                                .and_then(Value::as_str)
                        }) {
                            self.update(
                                &part,
                                choice_index,
                                index,
                                ToolUpdate::Signature(sig.into()),
                            )?;
                        }
                    }
                }
            }
            WireFormat::GoogleGenerateContent => {
                for (ci, candidate) in event["candidates"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let choice = candidate["index"].as_u64().unwrap_or(ci as u64);
                    for (position, part) in candidate["content"]["parts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .enumerate()
                    {
                        let Some(call) = part.get("functionCall") else {
                            continue;
                        };
                        let named = call["name"].as_str().is_some_and(|n| !n.is_empty());
                        let prior = self
                            .gemini_last
                            .get(&choice)
                            .cloned()
                            .filter(|(_, frame, slot)| *frame != self.frame || *slot == position);
                        let known = call["id"].as_str().and_then(|id| {
                            self.gemini_by_id.get(&(choice, id.to_owned())).cloned()
                        });
                        let key = known.or_else(|| {
                            prior.as_ref().and_then(|(key, _, _)| {
                                self.parts
                                    .get(key)
                                    .filter(|p| !named || p.name.is_empty())
                                    .map(|_| key.clone())
                            })
                        });
                        let (key, index) = if let Some(key) = key {
                            (key.clone(), self.parts[&key].index)
                        } else {
                            let index = self.next_gemini(choice);
                            (format!("gemini:{choice}:{index}"), index)
                        };
                        if let Some(id) = call["id"].as_str() {
                            self.gemini_by_id.insert((choice, id.into()), key.clone());
                        }
                        self.gemini_last
                            .insert(choice, (key.clone(), self.frame, position));
                        if let Some(id) = call["id"].as_str() {
                            self.update(&key, choice, index, ToolUpdate::WireId(id.into()))?;
                        }
                        if let Some(name) = call["name"].as_str().filter(|n| !n.is_empty()) {
                            self.update(&key, choice, index, ToolUpdate::Name(name.into()))?;
                        }
                        if let Some(args) = call.get("args").filter(|a| !a.is_null()) {
                            self.update(
                                &key,
                                choice,
                                index,
                                ToolUpdate::Arguments(args.to_string()),
                            )?;
                        } else if named || !self.parts.contains_key(&key) {
                            self.update(
                                &key,
                                choice,
                                index,
                                ToolUpdate::ArgumentsStart("{}".into()),
                            )?;
                        }
                        if let Some(sig) = part["thoughtSignature"].as_str() {
                            self.update(&key, choice, index, ToolUpdate::Signature(sig.into()))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn next_gemini(&mut self, choice: u64) -> u64 {
        let next = self.gemini_next.entry(choice).or_default();
        let n = *next;
        *next += 1;
        n
    }
    fn response_item(&mut self, key: &str, index: u64, item: &Value, done: bool) -> Result<()> {
        if let Some(id) = item["call_id"].as_str() {
            self.update(key, 0, index, ToolUpdate::WireId(id.into()))?;
        }
        if let Some(id) = item["id"].as_str() {
            self.update(key, 0, index, ToolUpdate::ItemId(id.into()))?;
        }
        if let Some(name) = item["name"].as_str() {
            self.update(key, 0, index, ToolUpdate::Name(name.into()))?;
        }
        if let Some(args) = item["arguments"].as_str() {
            self.update(
                key,
                0,
                index,
                if done {
                    ToolUpdate::Arguments(args.into())
                } else {
                    ToolUpdate::ArgumentsStart(args.into())
                },
            )?;
        }
        if done {
            self.update(key, 0, index, ToolUpdate::End)?;
        }
        Ok(())
    }

    fn complete(&mut self, choice: u64) -> Result<BTreeMap<u64, Vec<Value>>> {
        if self.emitted.contains(&choice) {
            return Ok(BTreeMap::new());
        }
        let mut result: BTreeMap<u64, Vec<Value>> = BTreeMap::new();
        let mut map = ToolCallMap::default();
        let mut ids = BTreeSet::new();
        let mut ordered: Vec<_> = self
            .parts
            .iter()
            .filter(|(_, part)| part.choice == choice)
            .collect();
        ordered.sort_by_key(|(_, part)| (part.choice, part.index));
        for (key, part) in ordered {
            if part.name.is_empty() {
                return Err(upstream("completed call has no name"));
            }
            validate_arguments(&part.arguments, true)?;
            if self.target.wire != WireFormat::GoogleGenerateContent
                && part.wire_id.as_deref().is_none_or(str::is_empty)
            {
                return Err(upstream("completed call has no correlation id"));
            }
            if let Some(id) = &part.wire_id {
                if !ids.insert((part.choice, id.clone())) {
                    return Err(upstream("duplicate correlation id"));
                }
            }
            let mut binding = binding(
                &self.target,
                &self.scope,
                part.index as usize,
                part.wire_id.as_deref(),
                part.item_id.as_deref(),
            );
            binding.part_id = key.clone();
            if matches!(
                self.target.wire,
                WireFormat::OpenAiChat | WireFormat::GoogleGenerateContent
            ) {
                binding.scope = format!("{}/choice:{}", self.scope, part.choice);
            }
            binding.signature_field = part.signature_field.clone();
            map.insert(&part.id, vec![binding.clone()])?;
            let mut call = json!({"id":part.id,"index":part.index,"type":"function","function":{"name":part.name,"arguments":part.arguments},"wire_ids":[binding]});
            if let Some(signature) = &part.signature {
                call["thought_signature"] = json!(signature);
            }
            result.entry(part.choice).or_default().push(call);
        }
        self.emitted.insert(choice);
        Ok(result)
    }

    pub(crate) fn finish(&self) -> Result<()> {
        if self
            .parts
            .values()
            .any(|p| !self.emitted.contains(&p.choice))
        {
            return Err(upstream("stream ended before all tool choices completed"));
        }
        Ok(())
    }

    /// Consume one native event plus its already-normalized ordinary fields.
    /// Provider tool conversions are discarded: this is their sole replacement.
    pub fn push(&mut self, event: &Value, chunk: Option<String>) -> Result<Option<String>> {
        self.parse(event)?;
        let had_chunk = chunk.is_some();
        let mut value: Value = match chunk {
            Some(s) => serde_json::from_str(&s)?,
            None => {
                json!({"object":"chat.completion.chunk","model":self.target.model,"choices":[{"index":0,"delta":{},"finish_reason":null}]})
            }
        };
        if let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) {
            for choice in choices {
                if let Some(d) = choice.get_mut("delta").and_then(Value::as_object_mut) {
                    d.remove("tool_calls");
                }
            }
        }
        if self.target.wire == WireFormat::GoogleGenerateContent {
            if let Some(index) = event["candidates"][0]["index"].as_u64() {
                if let Some(first) = value["choices"].as_array_mut().and_then(|c| c.first_mut()) {
                    first["index"] = json!(index);
                }
            }
            for (i, candidate) in event["candidates"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let Some(reason) = candidate["finishReason"].as_str() else {
                    continue;
                };
                let index = candidate["index"].as_u64().unwrap_or(i as u64);
                let choices = value["choices"].as_array_mut().unwrap();
                if !choices
                    .iter()
                    .any(|c| c["index"].as_u64().unwrap_or(0) == index)
                {
                    let reason = match reason {
                        "STOP" => "stop",
                        "MAX_TOKENS" => "length",
                        "SAFETY" => "content_filter",
                        _ => return Err(upstream("unsupported terminal reason")),
                    };
                    choices.push(json!({"index":index,"delta":{},"finish_reason":reason}));
                }
            }
        }
        let terminals: Vec<_> = value["choices"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(_, c)| c["finish_reason"].is_string())
            .map(|(i, c)| c["index"].as_u64().unwrap_or(i as u64))
            .collect();
        let terminal = !terminals.is_empty();
        for done in terminals {
            for (index, calls) in self.complete(done)? {
                let choices = value["choices"].as_array_mut().unwrap();
                let position = choices
                    .iter()
                    .position(|c| c["index"].as_u64().unwrap_or(0) == index)
                    .unwrap_or_else(|| {
                        choices
                            .push(json!({"index":index,"delta":{},"finish_reason":"tool_calls"}));
                        choices.len() - 1
                    });
                choices[position]["delta"]["tool_calls"] = json!(calls);
                if choices[position]["finish_reason"] == "stop" {
                    choices[position]["finish_reason"] = json!("tool_calls");
                }
            }
        }
        let useful = value.get("usage").is_some()
            || value["choices"].as_array().is_some_and(|choices| {
                choices.iter().any(|c| {
                    c["finish_reason"].is_string()
                        || c["delta"].as_object().is_some_and(|d| !d.is_empty())
                })
            });
        Ok((useful && (had_chunk || terminal)).then(|| value.to_string()))
    }
}

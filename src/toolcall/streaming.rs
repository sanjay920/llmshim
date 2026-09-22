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

#[derive(Clone, Copy, PartialEq, Eq)]
enum PartStringField {
    Name,
    WireId,
    Arguments,
}

/// Per-response stream state. Complete calls are exposed only after terminal
/// metadata, so no caller executes partial JSON or misses a trailing signature.
pub struct ToolStream {
    target: ReplayTarget,
    scope: String,
    parts: BTreeMap<String, Part>,
    gemini_last: BTreeMap<u64, (String, u64, usize)>,
    gemini_by_id: BTreeMap<u64, BTreeMap<String, String>>,
    frame: u64,
    gemini_next: BTreeMap<u64, u64>,
    emitted: BTreeSet<u64>,
    budget: crate::stream_retention::RetainedBudget,
    retained: crate::stream_retention::RetainedFootprint,
    #[cfg(test)]
    gemini_id_index_operations: usize,
}
impl ToolStream {
    pub fn new(target: ReplayTarget) -> Self {
        let limits = crate::stream_retention::StreamRetentionLimits::default();
        Self::with_budget(
            target,
            crate::stream_retention::RetainedBudget::new(
                limits.normalizer_bytes,
                limits.normalizer_entries,
            ),
        )
    }

    pub(crate) fn with_budget(
        target: ReplayTarget,
        budget: crate::stream_retention::RetainedBudget,
    ) -> Self {
        Self {
            target,
            scope: uuid::Uuid::new_v4().to_string(),
            parts: BTreeMap::new(),
            gemini_last: BTreeMap::new(),
            gemini_by_id: BTreeMap::new(),
            frame: 0,
            gemini_next: BTreeMap::new(),
            emitted: BTreeSet::new(),
            budget,
            retained: crate::stream_retention::RetainedFootprint::default(),
            #[cfg(test)]
            gemini_id_index_operations: 0,
        }
    }

    pub fn apply(&mut self, delta: ToolDelta) -> Result<()> {
        let result = self.apply_inner(delta);
        if result.is_err() {
            self.clear();
        }
        result
    }

    fn apply_inner(&mut self, delta: ToolDelta) -> Result<()> {
        if self.emitted.contains(&delta.choice) {
            return Err(upstream("tool data arrived after completion"));
        }
        if !self.parts.contains_key(&delta.part_id) {
            let new_part = Part {
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
            };
            let footprint = part_footprint(&delta.part_id, &new_part);
            self.reserve(footprint)?;
            self.parts.insert(delta.part_id.clone(), new_part);
        }
        match &delta.update {
            ToolUpdate::NameFragment(fragment) => {
                return self.append_fragment(&delta.part_id, PartStringField::Name, fragment, false)
            }
            ToolUpdate::WireIdFragment(fragment) => {
                return self.append_fragment(
                    &delta.part_id,
                    PartStringField::WireId,
                    fragment,
                    false,
                )
            }
            ToolUpdate::ArgumentsFragment(fragment) => {
                let part = self.parts.get(&delta.part_id).unwrap();
                if part.ended {
                    return Err(upstream("argument delta followed a completed tool part"));
                }
                return self.append_fragment(
                    &delta.part_id,
                    PartStringField::Arguments,
                    fragment,
                    part.initial,
                );
            }
            _ => {}
        }
        if let ToolUpdate::Signature(data) = &delta.update {
            let previous_footprint =
                part_footprint(&delta.part_id, self.parts.get(&delta.part_id).unwrap());
            let projected_footprint = part_footprint_with_signature(
                &delta.part_id,
                self.parts.get(&delta.part_id).unwrap(),
                &self.target,
                data.len(),
            );
            self.replace(previous_footprint, projected_footprint)?;
            self.parts.get_mut(&delta.part_id).unwrap().signature = Some(ThoughtSignature {
                data: fallible_string(data)?,
                origin: self.target.origin(),
            });
            let actual_footprint =
                part_footprint(&delta.part_id, self.parts.get(&delta.part_id).unwrap());
            self.replace(projected_footprint, actual_footprint)?;
            return Ok(());
        }
        let part = self.parts.get_mut(&delta.part_id).unwrap();
        let previous_footprint = part_footprint(&delta.part_id, part);
        match delta.update {
            ToolUpdate::Name(v) => part.name = v,
            ToolUpdate::NameFragment(_)
            | ToolUpdate::WireIdFragment(_)
            | ToolUpdate::ArgumentsFragment(_) => unreachable!("fragments handled above"),
            ToolUpdate::WireId(v) => part.wire_id = Some(v),
            ToolUpdate::ItemId(v) => part.item_id = Some(v),
            ToolUpdate::ArgumentsStart(v) => {
                part.arguments = v;
                part.initial = true;
            }
            ToolUpdate::Arguments(v) => {
                if part.arguments != v {
                    part.ended = false;
                }
                part.arguments = v;
                part.initial = false;
            }
            ToolUpdate::Signature(_) => unreachable!("signature handled before allocation"),
            ToolUpdate::SignatureField(v) => part.signature_field = Some(v),
            ToolUpdate::End => {
                validate_arguments(&part.arguments, true)?;
                part.ended = true;
            }
        }
        let replacement_footprint = part_footprint(&delta.part_id, part);
        self.budget
            .replace(previous_footprint, replacement_footprint)?;
        self.retained.bytes = self
            .retained
            .bytes
            .saturating_sub(previous_footprint.bytes)
            .saturating_add(replacement_footprint.bytes);
        self.retained.entries = self
            .retained
            .entries
            .saturating_sub(previous_footprint.entries)
            .saturating_add(replacement_footprint.entries);
        Ok(())
    }

    fn append_fragment(
        &mut self,
        part_id: &str,
        field: PartStringField,
        fragment: &str,
        replace_initial: bool,
    ) -> Result<()> {
        let part = self.parts.get(part_id).unwrap();
        let current_length = part_string_length(part, field);
        let current_capacity = part_string_capacity(part, field);
        let base_length = if replace_initial { 0 } else { current_length };
        let final_length = base_length
            .checked_add(fragment.len())
            .ok_or_else(crate::stream_retention::retention_error)?;
        let projected_capacity = if replace_initial {
            final_length
        } else {
            current_capacity.max(final_length)
        };
        let previous_footprint = part_footprint(part_id, part);
        let projected_footprint =
            part_footprint_with_capacity(part_id, part, field, projected_capacity);
        self.replace(previous_footprint, projected_footprint)?;

        if replace_initial {
            let mut replacement = String::new();
            if replacement.try_reserve_exact(fragment.len()).is_err() {
                self.replace(projected_footprint, previous_footprint)?;
                return Err(crate::stream_retention::retention_error());
            }
            let actual_footprint = part_footprint_with_capacity(
                part_id,
                self.parts.get(part_id).unwrap(),
                field,
                replacement.capacity(),
            );
            self.replace(projected_footprint, actual_footprint)?;
            replacement.push_str(fragment);
            let part = self.parts.get_mut(part_id).unwrap();
            part.arguments = replacement;
            part.initial = false;
            return Ok(());
        }

        let allocation_result = {
            let part = self.parts.get_mut(part_id).unwrap();
            part_string_mut(part, field).try_reserve_exact(fragment.len())
        };
        if allocation_result.is_err() {
            self.replace(projected_footprint, previous_footprint)?;
            return Err(crate::stream_retention::retention_error());
        }
        let actual_footprint = part_footprint(part_id, self.parts.get(part_id).unwrap());
        self.replace(projected_footprint, actual_footprint)?;
        part_string_mut(self.parts.get_mut(part_id).unwrap(), field).push_str(fragment);
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
                        let atomic = if explicit.is_none()
                            && call["id"].is_string()
                            && call["function"]["name"].is_string()
                        {
                            match call["function"]["arguments"].as_str() {
                                Some(arguments) => crate::json_bounds::bounded_json_complete(
                                    arguments,
                                    crate::json_bounds::Limits::SSE,
                                )?,
                                None => false,
                            }
                        } else {
                            false
                        };
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
                        let known = call["id"]
                            .as_str()
                            .and_then(|id| self.gemini_id_mapping(choice, id));
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
                            let index = self.next_gemini(choice)?;
                            (format!("gemini:{choice}:{index}"), index)
                        };
                        if let Some(id) = call["id"].as_str() {
                            #[cfg(test)]
                            {
                                self.gemini_id_index_operations += 1;
                            }
                            let existing_mapping = self
                                .gemini_by_id
                                .get(&choice)
                                .and_then(|mappings| mappings.get_key_value(id));
                            let stored_id_capacity =
                                existing_mapping.map(|(stored_id, _)| stored_id.capacity());
                            let previous_mapping = existing_mapping
                                .map(|(stored_id, value)| {
                                    crate::stream_retention::RetainedFootprint::record(
                                        stored_id.capacity().saturating_add(value.capacity()),
                                    )
                                })
                                .unwrap_or_default();
                            let mapping_exists = previous_mapping.entries != 0;
                            let choice_exists = self.gemini_by_id.contains_key(&choice);
                            if !choice_exists {
                                self.reserve(crate::stream_retention::RetainedFootprint::record(
                                    0,
                                ))?;
                            }
                            let replacement_mapping =
                                crate::stream_retention::RetainedFootprint::record(
                                    id.len().saturating_add(key.len()),
                                );
                            if let Err(error) = self.replace(previous_mapping, replacement_mapping)
                            {
                                if !choice_exists {
                                    self.release(
                                        crate::stream_retention::RetainedFootprint::record(0),
                                    );
                                }
                                return Err(error);
                            }
                            let owned_value = match fallible_string(&key) {
                                Ok(value) => value,
                                Err(error) => {
                                    self.replace(replacement_mapping, previous_mapping)?;
                                    if !choice_exists {
                                        self.release(
                                            crate::stream_retention::RetainedFootprint::record(0),
                                        );
                                    }
                                    return Err(error);
                                }
                            };
                            if mapping_exists {
                                let actual_mapping =
                                    crate::stream_retention::RetainedFootprint::record(
                                        stored_id_capacity
                                            .unwrap()
                                            .saturating_add(owned_value.capacity()),
                                    );
                                self.replace(replacement_mapping, actual_mapping)?;
                                #[cfg(test)]
                                {
                                    self.gemini_id_index_operations += 1;
                                }
                                *self
                                    .gemini_by_id
                                    .get_mut(&choice)
                                    .unwrap()
                                    .get_mut(id)
                                    .unwrap() = owned_value;
                            } else {
                                let owned_id = match fallible_string(id) {
                                    Ok(value) => value,
                                    Err(error) => {
                                        self.replace(replacement_mapping, previous_mapping)?;
                                        if !choice_exists {
                                            self.release(
                                                crate::stream_retention::RetainedFootprint::record(
                                                    0,
                                                ),
                                            );
                                        }
                                        return Err(error);
                                    }
                                };
                                let actual_mapping =
                                    crate::stream_retention::RetainedFootprint::record(
                                        owned_id.capacity().saturating_add(owned_value.capacity()),
                                    );
                                self.replace(replacement_mapping, actual_mapping)?;
                                #[cfg(test)]
                                {
                                    self.gemini_id_index_operations += 1;
                                }
                                self.gemini_by_id
                                    .entry(choice)
                                    .or_default()
                                    .insert(owned_id, owned_value);
                            }
                        }
                        let previous_last = self
                            .gemini_last
                            .get(&choice)
                            .map(|value| {
                                crate::stream_retention::RetainedFootprint::record(value.0.len())
                            })
                            .unwrap_or_default();
                        let replacement_last =
                            crate::stream_retention::RetainedFootprint::record(key.len());
                        self.replace(previous_last, replacement_last)?;
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

    fn next_gemini(&mut self, choice: u64) -> Result<u64> {
        if !self.gemini_next.contains_key(&choice) {
            self.reserve(crate::stream_retention::RetainedFootprint::record(0))?;
        }
        let next = self.gemini_next.entry(choice).or_default();
        let n = *next;
        *next += 1;
        Ok(n)
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

    fn complete(
        &mut self,
        choice: u64,
        frame_budget: &mut crate::derived_response::DerivedResponseBudget,
    ) -> Result<BTreeMap<u64, Vec<Value>>> {
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
            let projected_scope_bytes = self
                .scope
                .len()
                .checked_add(32)
                .ok_or_else(|| frame_budget.error())?;
            let binding_string_bytes = self
                .target
                .provider
                .len()
                .checked_add(projected_scope_bytes)
                .and_then(|bytes| bytes.checked_add(key.len()))
                .and_then(|bytes| bytes.checked_add(part.wire_id.as_deref().map_or(0, str::len)))
                .and_then(|bytes| bytes.checked_add(part.item_id.as_deref().map_or(0, str::len)))
                .and_then(|bytes| {
                    bytes.checked_add(part.signature_field.as_deref().map_or(0, str::len))
                })
                .and_then(|bytes| bytes.checked_add(40))
                .ok_or_else(|| frame_budget.error())?;
            let binding_footprint = crate::derived_response::DerivedFootprint::record(
                std::mem::size_of::<WireToolId>(),
            )
            .ok_or_else(|| frame_budget.error())?
            .checked_add(crate::derived_response::DerivedFootprint::strings(
                binding_string_bytes,
            ))
            .and_then(|footprint| footprint.checked_multiply(2))
            .ok_or_else(|| frame_budget.error())?;
            frame_budget.reserve(binding_footprint)?;
            if let Some(signature) = &part.signature {
                let signature_string_bytes = signature
                    .data
                    .len()
                    .checked_add(signature.origin.provider.len())
                    .and_then(|bytes| bytes.checked_add(signature.origin.model.len()))
                    .and_then(|bytes| {
                        bytes.checked_add(signature.origin.account.as_deref().map_or(0, str::len))
                    })
                    .ok_or_else(|| frame_budget.error())?;
                let signature_footprint =
                    crate::derived_response::DerivedFootprint::record(std::mem::size_of::<
                        ThoughtSignature,
                    >())
                    .ok_or_else(|| frame_budget.error())?
                    .checked_add(crate::derived_response::DerivedFootprint::strings(
                        signature_string_bytes,
                    ))
                    .and_then(|footprint| footprint.checked_multiply(2))
                    .ok_or_else(|| frame_budget.error())?;
                frame_budget.reserve(signature_footprint)?;
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
        if !self.emitted.contains(&choice) {
            self.reserve(crate::stream_retention::RetainedFootprint::record(0))?;
            self.emitted.insert(choice);
        }
        let completed_keys: Vec<_> = self
            .parts
            .iter()
            .filter(|(_, part)| part.choice == choice)
            .map(|(key, _)| key.clone())
            .collect();
        for key in completed_keys {
            if let Some(part) = self.parts.remove(&key) {
                self.release(part_footprint(&key, &part));
            }
        }
        if let Some(last) = self.gemini_last.remove(&choice) {
            self.release(crate::stream_retention::RetainedFootprint::record(
                last.0.len(),
            ));
        }
        if let Some(mappings) = self.gemini_by_id.remove(&choice) {
            for (id, value) in mappings {
                self.release(crate::stream_retention::RetainedFootprint::record(
                    id.capacity().saturating_add(value.capacity()),
                ));
            }
            self.release(crate::stream_retention::RetainedFootprint::record(0));
        }
        if self.gemini_next.remove(&choice).is_some() {
            self.release(crate::stream_retention::RetainedFootprint::record(0));
        }
        Ok(result)
    }

    pub(crate) fn finish(&mut self) -> Result<()> {
        let incomplete = self
            .parts
            .values()
            .any(|p| !self.emitted.contains(&p.choice));
        self.clear();
        if incomplete {
            Err(upstream("stream ended before all tool choices completed"))
        } else {
            Ok(())
        }
    }

    /// Consume one native event plus its already-normalized ordinary fields.
    /// Provider tool conversions are discarded: this is their sole replacement.
    pub fn push(&mut self, event: &Value, chunk: Option<String>) -> Result<Option<String>> {
        let result = self.push_inner(event, chunk);
        if result.is_err() {
            self.clear();
        }
        result
    }

    fn push_inner(&mut self, event: &Value, chunk: Option<String>) -> Result<Option<String>> {
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
        let mut frame_budget = crate::derived_response::DerivedResponseBudget::stream();
        crate::derived_response::reserve_existing_metadata(&value, &mut frame_budget)?;
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
            for (index, calls) in self.complete(done, &mut frame_budget)? {
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

    pub(crate) fn clear(&mut self) {
        self.parts.clear();
        self.gemini_last.clear();
        self.gemini_by_id.clear();
        self.gemini_next.clear();
        self.emitted.clear();
        self.budget.release(self.retained);
        self.retained = crate::stream_retention::RetainedFootprint::default();
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

    fn gemini_id_mapping(&mut self, choice: u64, id: &str) -> Option<String> {
        #[cfg(test)]
        {
            self.gemini_id_index_operations += 1;
        }
        self.gemini_by_id
            .get(&choice)
            .and_then(|mappings| mappings.get(id))
            .cloned()
    }
}

fn part_footprint(key: &str, part: &Part) -> crate::stream_retention::RetainedFootprint {
    part_footprint_with_capacity(key, part, PartStringField::Name, part.name.capacity())
}

fn part_footprint_with_capacity(
    key: &str,
    part: &Part,
    field: PartStringField,
    replacement_capacity: usize,
) -> crate::stream_retention::RetainedFootprint {
    let capacity = |candidate: PartStringField, actual: usize| {
        if candidate == field {
            replacement_capacity
        } else {
            actual
        }
    };
    let mut bytes = std::mem::size_of::<Part>()
        .saturating_add(key.len())
        .saturating_add(part.id.capacity())
        .saturating_add(capacity(PartStringField::Name, part.name.capacity()))
        .saturating_add(capacity(
            PartStringField::Arguments,
            part.arguments.capacity(),
        ))
        .saturating_add(capacity(
            PartStringField::WireId,
            part.wire_id.as_ref().map_or(0, String::capacity),
        ));
    for value in [part.item_id.as_ref(), part.signature_field.as_ref()]
        .into_iter()
        .flatten()
    {
        bytes = bytes.saturating_add(value.capacity());
    }
    if let Some(signature) = &part.signature {
        bytes = bytes
            .saturating_add(std::mem::size_of::<ThoughtSignature>())
            .saturating_add(signature.data.capacity())
            .saturating_add(signature.origin.provider.capacity())
            .saturating_add(signature.origin.model.capacity())
            .saturating_add(
                signature
                    .origin
                    .account
                    .as_ref()
                    .map_or(0, String::capacity),
            );
    }
    crate::stream_retention::RetainedFootprint::record(bytes)
}

fn part_footprint_with_signature(
    key: &str,
    part: &Part,
    target: &ReplayTarget,
    signature_bytes: usize,
) -> crate::stream_retention::RetainedFootprint {
    let mut projected = part_footprint(key, part);
    let previous_signature_bytes = part.signature.as_ref().map_or(0, |signature| {
        std::mem::size_of::<ThoughtSignature>()
            .saturating_add(signature.data.capacity())
            .saturating_add(signature.origin.provider.capacity())
            .saturating_add(signature.origin.model.capacity())
            .saturating_add(
                signature
                    .origin
                    .account
                    .as_ref()
                    .map_or(0, String::capacity),
            )
    });
    let replacement_signature_bytes = std::mem::size_of::<ThoughtSignature>()
        .saturating_add(signature_bytes)
        .saturating_add(target.provider.len())
        .saturating_add(target.model.len())
        .saturating_add(target.account.as_deref().map_or(0, str::len));
    projected.bytes = projected
        .bytes
        .saturating_sub(previous_signature_bytes)
        .saturating_add(replacement_signature_bytes);
    projected
}

fn part_string_length(part: &Part, field: PartStringField) -> usize {
    match field {
        PartStringField::Name => part.name.len(),
        PartStringField::WireId => part.wire_id.as_ref().map_or(0, String::len),
        PartStringField::Arguments => part.arguments.len(),
    }
}

fn part_string_capacity(part: &Part, field: PartStringField) -> usize {
    match field {
        PartStringField::Name => part.name.capacity(),
        PartStringField::WireId => part.wire_id.as_ref().map_or(0, String::capacity),
        PartStringField::Arguments => part.arguments.capacity(),
    }
}

fn part_string_mut(part: &mut Part, field: PartStringField) -> &mut String {
    match field {
        PartStringField::Name => &mut part.name,
        PartStringField::WireId => part.wire_id.get_or_insert_with(String::new),
        PartStringField::Arguments => &mut part.arguments,
    }
}

fn fallible_string(value: &str) -> Result<String> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| crate::stream_retention::retention_error())?;
    owned.push_str(value);
    Ok(owned)
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn fragment_refusal_drops_every_partial_string() {
        for update in [
            ToolUpdate::NameFragment("x".repeat(64)),
            ToolUpdate::WireIdFragment("x".repeat(64)),
            ToolUpdate::ArgumentsFragment("x".repeat(64)),
        ] {
            let budget = crate::stream_retention::RetainedBudget::new(768, 64);
            let mut stream = ToolStream::with_budget(
                ReplayTarget::new("custom", "model", WireFormat::OpenAiChat),
                budget.clone(),
            );
            let error = (0..32)
                .find_map(|_| {
                    stream
                        .apply(ToolDelta {
                            part_id: "part".into(),
                            choice: 0,
                            index: 0,
                            update: update.clone(),
                        })
                        .err()
                })
                .expect("capacity growth must reach the injected limit");
            assert_eq!(
                error.to_string(),
                format!("stream error: {}", crate::stream_retention::RETENTION_ERROR)
            );
            assert!(stream.parts.is_empty());
            assert_eq!(budget.retained(), Default::default());
        }
    }

    #[test]
    fn gemini_id_index_uses_a_fixed_number_of_logarithmic_operations() {
        let mut stream = ToolStream::new(ReplayTarget::new(
            "gemini",
            "gemini-2.5-pro",
            WireFormat::GoogleGenerateContent,
        ));
        let identifiers = 256_u64;
        for index in 0..identifiers {
            stream
                .parse(&json!({"candidates":[{"index":0,"content":{"parts":[{"functionCall":{"id":format!("id-{index}"),"name":"read","args":{}}}]}}]}))
                .unwrap();
        }
        stream
            .parse(&json!({"candidates":[{"index":0,"content":{"parts":[{"functionCall":{"id":"id-128","args":{"replacement":true}}}]}}]}))
            .unwrap();

        assert_eq!(stream.gemini_by_id[&0].len(), identifiers as usize);
        assert!(stream.gemini_id_index_operations <= 3 * (identifiers as usize + 1));
        assert_eq!(stream.gemini_by_id[&0]["id-128"], "gemini:0:128");
    }
}

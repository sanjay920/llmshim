//! Shared cache accounting. Native counters are read once at the transport
//! boundary; consumers never need to recognize a provider's usage dialect.
use serde_json::{json, Value};

use crate::reasoning::{ReplayTarget, WireFormat};

fn counter(usage: &Value, paths: &[&str]) -> u64 {
    paths
        .iter()
        .find_map(|path| usage.pointer(path).and_then(Value::as_u64))
        .unwrap_or(0)
}

/// One cache-read dialect: where the count lives, which field carries the prompt
/// total it belongs to, and whether that total already counts it.
///
/// The dialects disagree and the disagreement is load-bearing for pricing.
/// Anthropic's `input_tokens` excludes `cache_read_input_tokens`; the OpenAI
/// Responses, Chat Completions and Gemini totals all include their cached
/// counts. Reading the convention off the field that actually matched keeps the
/// decision on wire evidence rather than a provider-name table.
struct CacheReadDialect {
    path: &'static str,
    prompt: &'static str,
    prompt_includes_read: bool,
}

const CACHE_READ_DIALECTS: &[CacheReadDialect] = &[
    // An already-normalized body re-entering (OpenRouter / OpenAI-compatible
    // servers normalize their own Chat Completions bodies).
    CacheReadDialect {
        path: "/cache_read_tokens",
        prompt: "/prompt_tokens",
        prompt_includes_read: true,
    },
    CacheReadDialect {
        path: "/cache_read_input_tokens",
        prompt: "/input_tokens",
        prompt_includes_read: false,
    },
    CacheReadDialect {
        path: "/input_tokens_details/cached_tokens",
        prompt: "/input_tokens",
        prompt_includes_read: true,
    },
    CacheReadDialect {
        path: "/prompt_tokens_details/cached_tokens",
        prompt: "/prompt_tokens",
        prompt_includes_read: true,
    },
    CacheReadDialect {
        path: "/cachedContentTokenCount",
        prompt: "/promptTokenCount",
        prompt_includes_read: true,
    },
    CacheReadDialect {
        path: "/prompt_cache_hit_tokens",
        prompt: "/prompt_tokens",
        prompt_includes_read: true,
    },
];

/// Prompt totals to fall back on when no cache-read field was reported at all.
/// With a zero cache read the two conventions agree, so the order is arbitrary.
const PROMPT_TOTALS: &[&str] = &["/input_tokens", "/prompt_tokens", "/promptTokenCount"];

/// Tokens billed at the full input rate: the prompt total minus whatever cache
/// read the provider already counted inside it. Never negative, and never
/// silently dialect-guessed — an unreported cache read leaves the prompt whole.
fn uncached_input(native: &Value) -> u64 {
    match CACHE_READ_DIALECTS
        .iter()
        .find_map(|d| Some((d, native.pointer(d.path)?.as_u64()?)))
    {
        Some((dialect, read)) => {
            let prompt = counter(native, &[dialect.prompt]);
            if dialect.prompt_includes_read {
                prompt.saturating_sub(read)
            } else {
                prompt
            }
        }
        None => counter(native, PROMPT_TOTALS),
    }
}

/// Add normalized read/write counts without changing other token semantics.
/// Zero means no cache tokens were reported. It is not a pricing assertion.
///
/// `uncached_input_tokens` is added alongside them so cost accounting has one
/// unambiguous input count: the providers disagree on whether their prompt
/// total already includes the cache read, and only the transport boundary still
/// knows which convention the body used.
pub fn normalize_cache(native: &Value, normalized: &mut Value) {
    if !normalized.is_object() {
        *normalized = json!({});
    }
    normalized["cache_read_tokens"] = json!(counter(
        native,
        &[
            "/cache_read_tokens",
            "/cache_read_input_tokens",
            "/input_tokens_details/cached_tokens",
            "/prompt_tokens_details/cached_tokens",
            "/cachedContentTokenCount",
            "/prompt_cache_hit_tokens",
        ]
    ));
    normalized["cache_write_tokens"] = json!(counter(
        native,
        &[
            "/cache_write_tokens",
            "/cache_creation_input_tokens",
            "/prompt_tokens_details/cache_write_tokens",
        ]
    ));
    normalized["uncached_input_tokens"] = json!(uncached_input(native));
}

#[derive(Clone)]
pub(crate) struct NativeUsageObservation {
    pub(crate) usage: Value,
    pub(crate) terminal: bool,
    pub(crate) counters_complete: bool,
    pub(crate) explicit_zero: bool,
}

pub(crate) fn normalize_native_response_usage_observation(
    target: &ReplayTarget,
    native_response: &Value,
) -> Option<NativeUsageObservation> {
    let native_usage = native_usage_object(target.wire, native_response)?;
    usage_observation(target.wire, native_usage, true)
}

fn native_usage_object(wire: WireFormat, native_response: &Value) -> Option<&Value> {
    match wire {
        WireFormat::AnthropicMessages | WireFormat::OpenAiChat | WireFormat::OpenAiResponses => {
            native_response.get("usage")?
        }
        WireFormat::GoogleGenerateContent => native_response.get("usageMetadata")?,
    }
    .into()
}

fn usage_observation(
    wire: WireFormat,
    native_usage: &Value,
    terminal: bool,
) -> Option<NativeUsageObservation> {
    let counters_complete = match wire {
        WireFormat::OpenAiChat => {
            has_counter(native_usage, "/prompt_tokens")
                && has_counter(native_usage, "/completion_tokens")
        }
        WireFormat::OpenAiResponses => {
            has_counter(native_usage, "/input_tokens")
                && has_counter(native_usage, "/output_tokens")
        }
        WireFormat::AnthropicMessages => {
            has_counter(native_usage, "/input_tokens")
                && has_counter(native_usage, "/output_tokens")
        }
        WireFormat::GoogleGenerateContent => {
            has_counter(native_usage, "/promptTokenCount")
                && has_counter(native_usage, "/candidatesTokenCount")
        }
    };
    let usage = normalize_native_usage(wire, native_usage)?;
    let explicit_zero = counters_complete
        && [
            "prompt_tokens",
            "completion_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
        ]
        .into_iter()
        .all(|field| usage.get(field).and_then(Value::as_u64) == Some(0));
    Some(NativeUsageObservation {
        usage,
        terminal,
        counters_complete,
        explicit_zero,
    })
}

fn has_counter(value: &Value, pointer: &str) -> bool {
    value.pointer(pointer).and_then(Value::as_u64).is_some()
}

fn normalize_native_usage(wire: WireFormat, native_usage: &Value) -> Option<Value> {
    if !native_usage.is_object() {
        return None;
    }
    let mut normalized = native_usage.clone();
    match wire {
        WireFormat::OpenAiResponses => {
            normalized["prompt_tokens"] = native_usage
                .get("input_tokens")
                .cloned()
                .unwrap_or(json!(0));
            normalized["completion_tokens"] = native_usage
                .get("output_tokens")
                .cloned()
                .unwrap_or(json!(0));
            normalized["total_tokens"] = native_usage
                .get("total_tokens")
                .cloned()
                .unwrap_or(json!(0));
            if let Some(details) = native_usage.get("output_tokens_details") {
                normalized["completion_tokens_details"] = details.clone();
                if let Some(reasoning_tokens) = details.get("reasoning_tokens") {
                    normalized["reasoning_tokens"] = reasoning_tokens.clone();
                }
            }
        }
        WireFormat::AnthropicMessages => {
            let input = counter(native_usage, &["/input_tokens"]);
            let output = counter(native_usage, &["/output_tokens"]);
            let cache_read = counter(native_usage, &["/cache_read_input_tokens"]);
            let cache_write = counter(native_usage, &["/cache_creation_input_tokens"]);
            normalized["prompt_tokens"] = json!(input);
            normalized["completion_tokens"] = json!(output);
            normalized["total_tokens"] = json!(input
                .saturating_add(output)
                .saturating_add(cache_read)
                .saturating_add(cache_write));
        }
        WireFormat::GoogleGenerateContent => {
            normalized["prompt_tokens"] = native_usage
                .get("promptTokenCount")
                .cloned()
                .unwrap_or(json!(0));
            normalized["completion_tokens"] = native_usage
                .get("candidatesTokenCount")
                .cloned()
                .unwrap_or(json!(0));
            normalized["total_tokens"] = native_usage
                .get("totalTokenCount")
                .cloned()
                .unwrap_or(json!(0));
        }
        WireFormat::OpenAiChat => {}
    }
    normalize_cache(native_usage, &mut normalized);
    Some(normalized)
}

pub(crate) struct NativeStreamUsage {
    target: ReplayTarget,
    anthropic_usage: Value,
    known_chat_choices: std::collections::BTreeSet<u64>,
    finished_chat_choices: std::collections::BTreeSet<u64>,
    known_gemini_candidates: std::collections::BTreeSet<u64>,
    finished_gemini_candidates: std::collections::BTreeSet<u64>,
    terminal_candidate: Option<NativeUsageObservation>,
}

impl NativeStreamUsage {
    pub(crate) fn new(target: ReplayTarget) -> Self {
        Self {
            target,
            anthropic_usage: json!({}),
            known_chat_choices: std::collections::BTreeSet::new(),
            finished_chat_choices: std::collections::BTreeSet::new(),
            known_gemini_candidates: std::collections::BTreeSet::new(),
            finished_gemini_candidates: std::collections::BTreeSet::new(),
            terminal_candidate: None,
        }
    }

    pub(crate) fn take_terminal_candidate(&mut self) -> Option<NativeUsageObservation> {
        self.terminal_candidate.take().map(|mut observation| {
            observation.terminal = true;
            observation
        })
    }

    pub(crate) fn ingest(&mut self, native_event_text: &str) -> Option<NativeUsageObservation> {
        let native_event: Value = serde_json::from_str(native_event_text).ok()?;
        match self.target.wire {
            WireFormat::AnthropicMessages => {
                let usage_path = match native_event["type"].as_str() {
                    Some("message_start") => "/message/usage",
                    Some("message_delta") => "/usage",
                    _ if native_event["usage"].is_object() => "/usage",
                    _ => return None,
                };
                let incoming_usage = native_event.pointer(usage_path)?;
                let incoming = incoming_usage.as_object()?;
                for (key, value) in incoming {
                    self.anthropic_usage[key] = value.clone();
                }
                let terminal = native_event["type"] == "message_delta"
                    && native_event
                        .pointer("/delta/stop_reason")
                        .is_some_and(Value::is_string);
                let mut observation =
                    usage_observation(self.target.wire, &self.anthropic_usage, false)?;
                if terminal && !has_counter(incoming_usage, "/output_tokens") {
                    observation.counters_complete = false;
                    observation.explicit_zero = false;
                }
                if terminal {
                    self.terminal_candidate = Some(observation.clone());
                }
                Some(observation)
            }
            WireFormat::OpenAiResponses => {
                let terminal = native_event["type"] == "response.completed";
                let observation = native_event.get("response").and_then(|response| {
                    native_usage_object(self.target.wire, response)
                        .and_then(|usage| usage_observation(self.target.wire, usage, false))
                });
                if terminal {
                    self.terminal_candidate = observation.clone();
                }
                observation
            }
            WireFormat::OpenAiChat => {
                let choices = native_event.get("choices").and_then(Value::as_array);
                let mut invalidated_by_activity = false;
                if let Some(choices) = choices {
                    for (position, choice) in choices.iter().enumerate() {
                        let index = choice["index"].as_u64().unwrap_or(position as u64);
                        let newly_seen = self.known_chat_choices.insert(index);
                        let finished = choice["finish_reason"].is_string();
                        let has_output = choice
                            .get("delta")
                            .and_then(Value::as_object)
                            .is_some_and(|delta| !delta.is_empty());
                        if newly_seen || !finished || has_output {
                            self.terminal_candidate = None;
                            invalidated_by_activity = true;
                        }
                        if finished {
                            self.finished_chat_choices.insert(index);
                        } else {
                            self.finished_chat_choices.remove(&index);
                        }
                    }
                }
                let observation = native_event
                    .get("usage")
                    .and_then(|usage| usage_observation(self.target.wire, usage, false));
                if let Some(observation) = observation.as_ref() {
                    let all_known_finished = !self.known_chat_choices.is_empty()
                        && self.finished_chat_choices == self.known_chat_choices;
                    if all_known_finished {
                        let incoming_provider = crate::cost::reported(&observation.usage).is_some();
                        let retained_provider =
                            self.terminal_candidate.as_ref().is_some_and(|candidate| {
                                crate::cost::reported(&candidate.usage).is_some()
                            });
                        if incoming_provider || invalidated_by_activity || !retained_provider {
                            self.terminal_candidate = Some(observation.clone());
                        }
                    } else {
                        self.terminal_candidate = None;
                    }
                }
                observation
            }
            WireFormat::GoogleGenerateContent => {
                let candidates = native_event.get("candidates").and_then(Value::as_array);
                let mut invalidated_by_activity = false;
                if let Some(candidates) = candidates {
                    for (position, candidate) in candidates.iter().enumerate() {
                        let index = candidate["index"].as_u64().unwrap_or(position as u64);
                        let newly_seen = self.known_gemini_candidates.insert(index);
                        let finished = candidate["finishReason"].is_string();
                        let has_output = candidate
                            .pointer("/content/parts")
                            .and_then(Value::as_array)
                            .is_some_and(|parts| !parts.is_empty());
                        if newly_seen || !finished || has_output {
                            self.terminal_candidate = None;
                            invalidated_by_activity = true;
                        }
                        if finished {
                            self.finished_gemini_candidates.insert(index);
                        } else {
                            self.finished_gemini_candidates.remove(&index);
                        }
                    }
                }
                let observation = native_event
                    .get("usageMetadata")
                    .and_then(|usage| usage_observation(self.target.wire, usage, false));
                if let Some(observation) = observation.as_ref() {
                    let all_known_finished = !self.known_gemini_candidates.is_empty()
                        && self.finished_gemini_candidates == self.known_gemini_candidates;
                    if all_known_finished {
                        let incoming_provider = crate::cost::reported(&observation.usage).is_some();
                        let retained_provider =
                            self.terminal_candidate.as_ref().is_some_and(|candidate| {
                                crate::cost::reported(&candidate.usage).is_some()
                            });
                        if incoming_provider || invalidated_by_activity || !retained_provider {
                            self.terminal_candidate = Some(observation.clone());
                        }
                    } else {
                        self.terminal_candidate = None;
                    }
                }
                observation
            }
        }
    }
}

pub(crate) fn normalize_response(response: &mut Value) {
    let native = response.get("usage").cloned().unwrap_or(json!({}));
    normalize_cache(&native, &mut response["usage"]);
}

/// Anthropic splits input/cache counters at message_start and cumulative output
/// counters at message_delta. Merge raw counters before normalizing the final
/// chunk, so absent input fields cannot reset an earlier cache read to zero.
#[derive(Default)]
pub(crate) struct StreamUsage {
    anthropic: Value,
    chat_usage: Option<Value>,
    chat_terminal: Option<Value>,
    chat_choices: std::collections::BTreeMap<u64, Value>,
    known_chat_choices: std::collections::BTreeSet<u64>,
    finished_chat_choices: std::collections::BTreeSet<u64>,
    chat_provider_cost_floor: Option<f64>,
    terminal_chat_provider_cost: Option<f64>,
}

impl StreamUsage {
    /// Chat Completions may send usage after the finish-reason chunk. Delay the
    /// terminal marker so proxy clients do not stop before receiving accounting.
    pub(crate) fn defer_chat_terminal(&mut self, data: String) -> crate::error::Result<String> {
        let mut chunk: Value = serde_json::from_str(&data)?;
        let usage = chunk
            .get("usage")
            .filter(|value| value.is_object())
            .cloned();
        let provider_cost = usage.as_ref().and_then(crate::cost::reported);
        if let Some(provider_cost) = provider_cost {
            self.chat_provider_cost_floor = Some(
                self.chat_provider_cost_floor
                    .map_or(provider_cost, |current| current.max(provider_cost)),
            );
        }
        let terminal = chunk.clone();
        let mut invalidated_by_activity = false;
        if let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut) {
            for (i, choice) in choices.iter_mut().enumerate() {
                let index = choice["index"].as_u64().unwrap_or(i as u64);
                let newly_seen = self.known_chat_choices.insert(index);
                let finished = choice["finish_reason"].is_string();
                let has_output = choice
                    .get("delta")
                    .and_then(Value::as_object)
                    .is_some_and(|delta| !delta.is_empty());
                if newly_seen || !finished || has_output {
                    self.terminal_chat_provider_cost = None;
                    invalidated_by_activity = true;
                }
                if finished {
                    self.finished_chat_choices.insert(index);
                } else {
                    self.finished_chat_choices.remove(&index);
                }
                if finished {
                    let mut done = choice.clone();
                    done["delta"] = json!({});
                    self.chat_choices.insert(index, done);
                    choice["finish_reason"] = Value::Null;
                }
            }
        }
        if let Some(usage) = usage {
            let all_known_finished = !self.known_chat_choices.is_empty()
                && self.finished_chat_choices == self.known_chat_choices;
            if all_known_finished {
                if provider_cost.is_some()
                    || invalidated_by_activity
                    || self.terminal_chat_provider_cost.is_none()
                {
                    self.terminal_chat_provider_cost = provider_cost;
                }
            } else {
                self.terminal_chat_provider_cost = None;
            }
            self.chat_usage = Some(usage);
        }
        if !self.chat_choices.is_empty() {
            let pending = self.chat_terminal.get_or_insert(terminal);
            pending["choices"] = json!(self.chat_choices.values().collect::<Vec<_>>());
        }
        Ok(chunk.to_string())
    }

    pub(crate) fn take_terminal(&mut self) -> Option<String> {
        let mut terminal = self.chat_terminal.take()?;
        if let Some(mut usage) = self.chat_usage.take() {
            if let Some(provider_cost) = self.terminal_chat_provider_cost.take() {
                usage["cost"] = json!(provider_cost);
            } else if let Some(provider_floor) = self.chat_provider_cost_floor.take() {
                if let Some(object) = usage.as_object_mut() {
                    object.remove("cost");
                    object.remove("cost_usd");
                    object.remove("cost_source");
                }
                usage["provider_cost_floor_usd"] = json!(provider_floor);
            }
            terminal["usage"] = usage;
        }
        Some(terminal.to_string())
    }

    pub(crate) fn ingest(&mut self, provider: &str, event: &mut Value) {
        if provider != "anthropic" {
            return;
        }
        let path = match event["type"].as_str() {
            Some("message_start") => "/message/usage",
            Some("message_delta") => "/usage",
            _ => return,
        };
        if !self.anthropic.is_object() {
            self.anthropic = json!({});
        }
        if let Some(incoming) = event.pointer(path).and_then(Value::as_object) {
            for (key, value) in incoming {
                self.anthropic[key] = value.clone();
            }
        }
        if path == "/usage" {
            event["usage"] = self.anthropic.clone();
        }
    }
}

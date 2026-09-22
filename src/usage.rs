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

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn native_usage_returns_known_spend_before_latched_retention_error() {
        let target = ReplayTarget::new("openrouter", "vendor/model", WireFormat::OpenAiChat);
        let first = json!({
            "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"cost":0.25}
        });
        let mut probe = NativeStreamUsage::with_limits(
            target.clone(),
            crate::stream_retention::StreamRetentionLimits::default(),
        );
        assert!(probe.ingest(&first.to_string()).is_some());
        let first_entries = probe.budget.retained().entries;
        let limits = crate::stream_retention::StreamRetentionLimits::new(
            16 * 1024,
            64,
            16 * 1024,
            first_entries + 1,
        )
        .unwrap();
        let mut usage = NativeStreamUsage::with_limits(target, limits);

        assert!(usage.ingest(&first.to_string()).is_some());
        assert!(usage.take_retention_error().is_none());

        let over_limit = json!({
            "choices":[{"index":1,"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":2,"completion_tokens":2,"cost":0.75}
        });
        let observation = usage
            .ingest(&over_limit.to_string())
            .expect("the current known usage must still be returned");
        assert_eq!(crate::cost::reported(&observation.usage), Some(0.75));
        assert_eq!(
            usage.take_retention_error().unwrap().to_string(),
            "stream error: upstream stream retained state exceeds limit"
        );
        assert!(usage.take_terminal_candidate().is_none());
    }

    #[test]
    fn anthropic_retention_failure_keeps_compact_current_billing() {
        let limits =
            crate::stream_retention::StreamRetentionLimits::new(16 * 1024, 64, 16 * 1024, 4)
                .unwrap();
        let target = ReplayTarget::new(
            "anthropic",
            "claude-sonnet-5",
            WireFormat::AnthropicMessages,
        );
        let mut usage = NativeStreamUsage::with_limits(target, limits);
        let start = json!({
            "type":"message_start",
            "message":{"usage":{"input_tokens":1,"cost":0.25}}
        });
        let first = usage.ingest(&start.to_string()).unwrap();
        assert_eq!(first.usage["prompt_tokens"], 1);
        assert_eq!(crate::cost::reported(&first.usage), Some(0.25));

        let terminal = json!({
            "type":"message_delta",
            "delta":{"stop_reason":"end_turn"},
            "usage":{"output_tokens":2,"cost":0.75}
        });
        let current = usage.ingest(&terminal.to_string()).unwrap();
        assert_eq!(current.usage["prompt_tokens"], 1);
        assert_eq!(current.usage["completion_tokens"], 2);
        assert_eq!(crate::cost::reported(&current.usage), Some(0.75));
        assert!(usage.retention_error.is_some());
        assert!(usage.terminal_candidate.is_none());
    }

    #[test]
    fn choice_retention_failure_cannot_repopulate_state_in_the_same_frame() {
        for wire in [WireFormat::OpenAiChat, WireFormat::GoogleGenerateContent] {
            let limits =
                crate::stream_retention::StreamRetentionLimits::new(16 * 1024, 64, 16 * 1024, 1)
                    .unwrap();
            let target = ReplayTarget::new("test", "model", wire);
            let mut usage = NativeStreamUsage::with_limits(target, limits);
            let event = match wire {
                WireFormat::OpenAiChat => json!({
                    "choices":[
                        {"index":0,"delta":{},"finish_reason":"stop"},
                        {"index":1,"delta":{},"finish_reason":"stop"}
                    ],
                    "usage":{"prompt_tokens":1,"completion_tokens":1,"cost":0.5}
                }),
                WireFormat::GoogleGenerateContent => json!({
                    "candidates":[
                        {"index":0,"content":{"parts":[]},"finishReason":"STOP"},
                        {"index":1,"content":{"parts":[]},"finishReason":"STOP"}
                    ],
                    "usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"cost":0.5}
                }),
                _ => unreachable!(),
            };
            assert!(usage.ingest(&event.to_string()).is_some());
            assert!(usage.retention_error.is_some());
            assert!(usage.terminal_candidate.is_none());
            assert!(usage.known_chat_choices.is_empty());
            assert!(usage.finished_chat_choices.is_empty());
            assert!(usage.known_gemini_candidates.is_empty());
            assert!(usage.finished_gemini_candidates.is_empty());
            assert_eq!(usage.budget.retained(), Default::default());
        }
    }

    #[test]
    fn compact_usage_preserves_every_cache_and_reasoning_dialect() {
        for (usage, pointer, expected) in [
            (json!({"cache_read_tokens":1}), "/cache_read_tokens", 1),
            (
                json!({"cache_read_input_tokens":2}),
                "/cache_read_input_tokens",
                2,
            ),
            (
                json!({"input_tokens_details":{"cached_tokens":3}}),
                "/input_tokens_details/cached_tokens",
                3,
            ),
            (
                json!({"prompt_tokens_details":{"cached_tokens":4}}),
                "/prompt_tokens_details/cached_tokens",
                4,
            ),
            (
                json!({"cachedContentTokenCount":5}),
                "/cachedContentTokenCount",
                5,
            ),
            (
                json!({"prompt_cache_hit_tokens":6}),
                "/prompt_cache_hit_tokens",
                6,
            ),
            (json!({"cache_write_tokens":7}), "/cache_write_tokens", 7),
            (
                json!({"cache_creation_input_tokens":8}),
                "/cache_creation_input_tokens",
                8,
            ),
            (
                json!({"prompt_tokens_details":{"cache_write_tokens":9}}),
                "/prompt_tokens_details/cache_write_tokens",
                9,
            ),
            (json!({"reasoning_tokens":10}), "/reasoning_tokens", 10),
            (
                json!({"output_tokens_details":{"reasoning_tokens":11}}),
                "/output_tokens_details/reasoning_tokens",
                11,
            ),
            (
                json!({"completion_tokens_details":{"reasoning_tokens":12}}),
                "/completion_tokens_details/reasoning_tokens",
                12,
            ),
        ] {
            let compact = compact_native_usage(WireFormat::OpenAiChat, &usage);
            assert_eq!(
                compact.pointer(pointer).and_then(Value::as_u64),
                Some(expected)
            );
        }
    }
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

pub(crate) fn compact_native_response_usage(
    wire: WireFormat,
    native_response: &Value,
) -> Option<Value> {
    let native_usage = native_usage_object(wire, native_response)?;
    if !native_usage.is_object() {
        return None;
    }
    Some(json!({"usage": compact_native_usage(wire, native_usage)}))
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

fn compact_native_usage(_wire: WireFormat, native_usage: &Value) -> Value {
    let mut compact = json!({});
    for field in [
        "input_tokens",
        "output_tokens",
        "total_tokens",
        "prompt_tokens",
        "completion_tokens",
        "promptTokenCount",
        "candidatesTokenCount",
        "totalTokenCount",
        "cache_read_tokens",
        "cache_read_input_tokens",
        "cachedContentTokenCount",
        "prompt_cache_hit_tokens",
        "cache_write_tokens",
        "cache_creation_input_tokens",
        "uncached_input_tokens",
        "reasoning_tokens",
    ] {
        if let Some(value) = native_usage.get(field).and_then(Value::as_u64) {
            compact[field] = json!(value);
        }
    }
    for (object, fields) in [
        ("input_tokens_details", &["cached_tokens"][..]),
        (
            "prompt_tokens_details",
            &["cached_tokens", "cache_write_tokens"][..],
        ),
        ("output_tokens_details", &["reasoning_tokens"][..]),
        ("completion_tokens_details", &["reasoning_tokens"][..]),
    ] {
        for field in fields {
            if let Some(value) = native_usage
                .get(object)
                .and_then(|details| details.get(*field))
                .and_then(Value::as_u64)
            {
                compact[object][*field] = json!(value);
            }
        }
    }
    if let Some(cost) = native_usage
        .get("cost")
        .and_then(Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
    {
        compact["cost"] = json!(cost);
    }
    compact
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
    anthropic_billing_usage: Value,
    known_chat_choices: std::collections::BTreeSet<u64>,
    finished_chat_choices: std::collections::BTreeSet<u64>,
    known_gemini_candidates: std::collections::BTreeSet<u64>,
    finished_gemini_candidates: std::collections::BTreeSet<u64>,
    terminal_candidate: Option<NativeUsageObservation>,
    terminal_candidate_footprint: crate::stream_retention::RetainedFootprint,
    anthropic_footprints:
        std::collections::BTreeMap<String, crate::stream_retention::RetainedFootprint>,
    budget: crate::stream_retention::RetainedBudget,
    retained: crate::stream_retention::RetainedFootprint,
    retention_error: Option<crate::error::ShimError>,
}

impl NativeStreamUsage {
    pub(crate) fn with_limits(
        target: ReplayTarget,
        limits: crate::stream_retention::StreamRetentionLimits,
    ) -> Self {
        Self {
            target,
            anthropic_usage: json!({}),
            anthropic_billing_usage: json!({}),
            known_chat_choices: std::collections::BTreeSet::new(),
            finished_chat_choices: std::collections::BTreeSet::new(),
            known_gemini_candidates: std::collections::BTreeSet::new(),
            finished_gemini_candidates: std::collections::BTreeSet::new(),
            terminal_candidate: None,
            terminal_candidate_footprint: Default::default(),
            anthropic_footprints: Default::default(),
            budget: crate::stream_retention::RetainedBudget::new(
                limits.native_usage_bytes,
                limits.native_usage_entries,
            ),
            retained: Default::default(),
            retention_error: None,
        }
    }

    pub(crate) fn take_retention_error(&mut self) -> Option<crate::error::ShimError> {
        let error = self.retention_error.take();
        if error.is_some() {
            self.clear_retained_state();
            self.anthropic_billing_usage = json!({});
        }
        error
    }

    pub(crate) fn take_terminal_candidate(&mut self) -> Option<NativeUsageObservation> {
        let candidate = self.terminal_candidate.take().map(|mut observation| {
            observation.terminal = true;
            observation
        });
        self.release(self.terminal_candidate_footprint);
        self.terminal_candidate_footprint = Default::default();
        candidate
    }

    #[cfg(test)]
    pub(crate) fn ingest(&mut self, native_event_text: &str) -> Option<NativeUsageObservation> {
        let native_event: Value = serde_json::from_str(native_event_text).ok()?;
        self.ingest_value(native_event)
    }

    pub(crate) fn ingest_bounded(
        &mut self,
        native_event_text: &str,
    ) -> crate::error::Result<Option<NativeUsageObservation>> {
        if native_event_text.trim().is_empty() || native_event_text.trim() == "[DONE]" {
            return Ok(None);
        }
        let native_event =
            match crate::json_bounds::parse_str(native_event_text, crate::json_bounds::Limits::SSE)
            {
                Ok(value) => value,
                Err(crate::json_bounds::ParseError::Malformed(_)) => {
                    return Err(crate::error::ShimError::Stream(
                        "invalid upstream JSON".into(),
                    ))
                }
                Err(crate::json_bounds::ParseError::Complexity) => {
                    return Err(crate::error::ShimError::Stream(
                        "upstream JSON exceeds complexity limit".into(),
                    ))
                }
            };
        Ok(self.ingest_value(native_event))
    }

    fn ingest_value(&mut self, native_event: Value) -> Option<NativeUsageObservation> {
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
                self.merge_anthropic_billing_usage(incoming_usage);
                if self.retention_error.is_none() {
                    for (key, value) in incoming {
                        if self.retain_anthropic_field(key, value).is_err() {
                            self.fail_retention();
                            break;
                        }
                    }
                }
                let terminal = native_event["type"] == "message_delta"
                    && native_event
                        .pointer("/delta/stop_reason")
                        .is_some_and(Value::is_string);
                let mut observation =
                    usage_observation(self.target.wire, &self.anthropic_billing_usage, false)?;
                if terminal && !has_counter(incoming_usage, "/output_tokens") {
                    observation.counters_complete = false;
                    observation.explicit_zero = false;
                }
                if terminal && self.retention_error.is_none() {
                    self.set_terminal_candidate(Some(observation.clone()));
                }
                Some(observation)
            }
            WireFormat::OpenAiResponses => {
                let terminal = native_event["type"] == "response.completed";
                let observation = native_event.get("response").and_then(|response| {
                    native_usage_object(self.target.wire, response)
                        .map(|usage| compact_native_usage(self.target.wire, usage))
                        .and_then(|usage| usage_observation(self.target.wire, &usage, false))
                });
                if terminal && self.retention_error.is_none() {
                    self.set_terminal_candidate(observation.clone());
                }
                observation
            }
            WireFormat::OpenAiChat => {
                let observation = native_event
                    .get("usage")
                    .map(|usage| compact_native_usage(self.target.wire, usage))
                    .and_then(|usage| usage_observation(self.target.wire, &usage, false));
                if self.retention_error.is_some() {
                    return observation;
                }
                let choices = native_event.get("choices").and_then(Value::as_array);
                let mut invalidated_by_activity = false;
                if let Some(choices) = choices {
                    for (position, choice) in choices.iter().enumerate() {
                        let index = choice["index"].as_u64().unwrap_or(position as u64);
                        let newly_seen = !self.known_chat_choices.contains(&index);
                        if newly_seen && self.reserve_record(0).is_err() {
                            self.fail_retention();
                            break;
                        } else {
                            self.known_chat_choices.insert(index);
                        }
                        let finished = choice["finish_reason"].is_string();
                        let has_output = choice
                            .get("delta")
                            .and_then(Value::as_object)
                            .is_some_and(|delta| !delta.is_empty());
                        if newly_seen || !finished || has_output {
                            self.set_terminal_candidate(None);
                            invalidated_by_activity = true;
                            if self.retention_error.is_some() {
                                break;
                            }
                        }
                        if finished {
                            if !self.finished_chat_choices.contains(&index)
                                && self.reserve_record(0).is_err()
                            {
                                self.fail_retention();
                                break;
                            } else {
                                self.finished_chat_choices.insert(index);
                            }
                        } else {
                            if self.finished_chat_choices.remove(&index) {
                                self.release(crate::stream_retention::RetainedFootprint::record(0));
                            }
                        }
                    }
                }
                if self.retention_error.is_some() {
                    return observation;
                }
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
                            self.set_terminal_candidate(Some(observation.clone()));
                        }
                    } else {
                        self.set_terminal_candidate(None);
                    }
                }
                observation
            }
            WireFormat::GoogleGenerateContent => {
                let observation = native_event
                    .get("usageMetadata")
                    .map(|usage| compact_native_usage(self.target.wire, usage))
                    .and_then(|usage| usage_observation(self.target.wire, &usage, false));
                if self.retention_error.is_some() {
                    return observation;
                }
                let candidates = native_event.get("candidates").and_then(Value::as_array);
                let mut invalidated_by_activity = false;
                if let Some(candidates) = candidates {
                    for (position, candidate) in candidates.iter().enumerate() {
                        let index = candidate["index"].as_u64().unwrap_or(position as u64);
                        let newly_seen = !self.known_gemini_candidates.contains(&index);
                        if newly_seen && self.reserve_record(0).is_err() {
                            self.fail_retention();
                            break;
                        } else {
                            self.known_gemini_candidates.insert(index);
                        }
                        let finished = candidate["finishReason"].is_string();
                        let has_output = candidate
                            .pointer("/content/parts")
                            .and_then(Value::as_array)
                            .is_some_and(|parts| !parts.is_empty());
                        if newly_seen || !finished || has_output {
                            self.set_terminal_candidate(None);
                            invalidated_by_activity = true;
                            if self.retention_error.is_some() {
                                break;
                            }
                        }
                        if finished {
                            if !self.finished_gemini_candidates.contains(&index)
                                && self.reserve_record(0).is_err()
                            {
                                self.fail_retention();
                                break;
                            } else {
                                self.finished_gemini_candidates.insert(index);
                            }
                        } else {
                            if self.finished_gemini_candidates.remove(&index) {
                                self.release(crate::stream_retention::RetainedFootprint::record(0));
                            }
                        }
                    }
                }
                if self.retention_error.is_some() {
                    return observation;
                }
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
                            self.set_terminal_candidate(Some(observation.clone()));
                        }
                    } else {
                        self.set_terminal_candidate(None);
                    }
                }
                observation
            }
        }
    }

    fn retain_anthropic_field(&mut self, key: &str, value: &Value) -> crate::error::Result<()> {
        let previous = self
            .anthropic_footprints
            .get(key)
            .copied()
            .unwrap_or_default();
        let mut replacement = crate::stream_retention::estimate_value(value)?;
        if previous.entries == 0 {
            replacement = replacement
                .checked_add(crate::stream_retention::RetainedFootprint::record(
                    key.len(),
                ))
                .ok_or_else(crate::stream_retention::retention_error)?;
        }
        self.replace(previous, replacement)?;
        self.anthropic_usage[key] = value.clone();
        self.anthropic_footprints
            .insert(key.to_owned(), replacement);
        Ok(())
    }

    fn merge_anthropic_billing_usage(&mut self, incoming: &Value) {
        let compact = compact_native_usage(WireFormat::AnthropicMessages, incoming);
        if let Some(fields) = compact.as_object() {
            for (field, value) in fields {
                self.anthropic_billing_usage[field] = value.clone();
            }
        }
    }

    fn set_terminal_candidate(&mut self, candidate: Option<NativeUsageObservation>) {
        let replacement = match candidate.as_ref() {
            Some(observation) => {
                let Ok(footprint) = crate::stream_retention::estimate_value(&observation.usage)
                else {
                    self.fail_retention();
                    return;
                };
                footprint
            }
            None => Default::default(),
        };
        if self
            .replace(self.terminal_candidate_footprint, replacement)
            .is_err()
        {
            self.fail_retention();
            return;
        }
        self.terminal_candidate = candidate;
        self.terminal_candidate_footprint = replacement;
    }

    fn reserve_record(&mut self, bytes: usize) -> crate::error::Result<()> {
        let footprint = crate::stream_retention::RetainedFootprint::record(bytes);
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
    ) -> crate::error::Result<()> {
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

    fn fail_retention(&mut self) {
        self.clear_retained_state();
        self.retention_error = Some(crate::stream_retention::retention_error());
    }

    fn clear_retained_state(&mut self) {
        self.known_chat_choices.clear();
        self.finished_chat_choices.clear();
        self.known_gemini_candidates.clear();
        self.finished_gemini_candidates.clear();
        self.terminal_candidate = None;
        self.terminal_candidate_footprint = Default::default();
        self.anthropic_usage = json!({});
        self.anthropic_footprints.clear();
        self.budget.reset();
        self.retained = Default::default();
    }
}

pub(crate) fn normalize_response(response: &mut Value) {
    let native = response.get("usage").cloned().unwrap_or(json!({}));
    normalize_cache(&native, &mut response["usage"]);
}

/// Anthropic splits input/cache counters at message_start and cumulative output
/// counters at message_delta. Merge raw counters before normalizing the final
/// chunk, so absent input fields cannot reset an earlier cache read to zero.
pub(crate) struct StreamUsage {
    anthropic: Value,
    chat_usage: Option<Value>,
    chat_terminal: Option<Value>,
    chat_choices: std::collections::BTreeMap<u64, Value>,
    known_chat_choices: std::collections::BTreeSet<u64>,
    finished_chat_choices: std::collections::BTreeSet<u64>,
    chat_provider_cost_floor: Option<f64>,
    terminal_chat_provider_cost: Option<f64>,
    budget: crate::stream_retention::RetainedBudget,
    retained: crate::stream_retention::RetainedFootprint,
    anthropic_footprints:
        std::collections::BTreeMap<String, crate::stream_retention::RetainedFootprint>,
    chat_usage_footprint: crate::stream_retention::RetainedFootprint,
    chat_terminal_footprint: crate::stream_retention::RetainedFootprint,
    chat_choice_footprints:
        std::collections::BTreeMap<u64, crate::stream_retention::RetainedFootprint>,
}

impl Default for StreamUsage {
    fn default() -> Self {
        let limits = crate::stream_retention::StreamRetentionLimits::default();
        Self::with_budget(crate::stream_retention::RetainedBudget::new(
            limits.normalizer_bytes,
            limits.normalizer_entries,
        ))
    }
}

impl StreamUsage {
    pub(crate) fn with_budget(budget: crate::stream_retention::RetainedBudget) -> Self {
        Self {
            anthropic: json!({}),
            chat_usage: None,
            chat_terminal: None,
            chat_choices: Default::default(),
            known_chat_choices: Default::default(),
            finished_chat_choices: Default::default(),
            chat_provider_cost_floor: None,
            terminal_chat_provider_cost: None,
            budget,
            retained: Default::default(),
            anthropic_footprints: Default::default(),
            chat_usage_footprint: Default::default(),
            chat_terminal_footprint: Default::default(),
            chat_choice_footprints: Default::default(),
        }
    }
    /// Chat Completions may send usage after the finish-reason chunk. Delay the
    /// terminal marker so proxy clients do not stop before receiving accounting.
    pub(crate) fn defer_chat_terminal(&mut self, data: String) -> crate::error::Result<String> {
        let mut chunk: Value =
            match crate::json_bounds::parse_str(&data, crate::json_bounds::Limits::SSE) {
                Ok(value) => value,
                Err(crate::json_bounds::ParseError::Malformed(error)) => return Err(error.into()),
                Err(crate::json_bounds::ParseError::Complexity) => {
                    return Err(crate::error::ShimError::Stream(
                        "stream JSON exceeds complexity limit".into(),
                    ))
                }
            };
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
        let mut terminal = chunk.clone();
        terminal["choices"] = json!([]);
        if let Some(object) = terminal.as_object_mut() {
            object.remove("usage");
        }
        let mut invalidated_by_activity = false;
        if let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut) {
            for (i, choice) in choices.iter_mut().enumerate() {
                let index = choice["index"].as_u64().unwrap_or(i as u64);
                let newly_seen = !self.known_chat_choices.contains(&index);
                if newly_seen {
                    self.reserve(crate::stream_retention::RetainedFootprint::record(0))?;
                    self.known_chat_choices.insert(index);
                }
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
                    if !self.finished_chat_choices.contains(&index) {
                        self.reserve(crate::stream_retention::RetainedFootprint::record(0))?;
                        self.finished_chat_choices.insert(index);
                    }
                } else if self.finished_chat_choices.remove(&index) {
                    self.release(crate::stream_retention::RetainedFootprint::record(0));
                }
                if finished {
                    let mut done = choice.clone();
                    done["delta"] = json!({});
                    let previous = self
                        .chat_choice_footprints
                        .get(&index)
                        .copied()
                        .unwrap_or_default();
                    let mut replacement = crate::stream_retention::estimate_value(&done)?;
                    if previous.entries == 0 {
                        replacement = replacement
                            .checked_add(crate::stream_retention::RetainedFootprint::record(0))
                            .ok_or_else(crate::stream_retention::retention_error)?;
                    }
                    self.replace(previous, replacement)?;
                    self.chat_choices.insert(index, done);
                    self.chat_choice_footprints.insert(index, replacement);
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
            let replacement = crate::stream_retention::estimate_value(&usage)?;
            self.replace(self.chat_usage_footprint, replacement)?;
            self.chat_usage = Some(usage);
            self.chat_usage_footprint = replacement;
        }
        if !self.chat_choices.is_empty() && self.chat_terminal.is_none() {
            let replacement = crate::stream_retention::estimate_value(&terminal)?;
            self.replace(self.chat_terminal_footprint, replacement)?;
            self.chat_terminal = Some(terminal);
            self.chat_terminal_footprint = replacement;
        }
        Ok(chunk.to_string())
    }

    pub(crate) fn take_terminal(&mut self) -> Option<String> {
        let mut terminal = self.chat_terminal.take()?;
        terminal["choices"] = json!(std::mem::take(&mut self.chat_choices)
            .into_values()
            .collect::<Vec<_>>());
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
        let serialized = terminal.to_string();
        self.clear();
        Some(serialized)
    }

    pub(crate) fn ingest(&mut self, provider: &str, event: &mut Value) -> crate::error::Result<()> {
        if provider != "anthropic" {
            return Ok(());
        }
        let path = match event["type"].as_str() {
            Some("message_start") => "/message/usage",
            Some("message_delta") => "/usage",
            _ => return Ok(()),
        };
        if !self.anthropic.is_object() {
            self.anthropic = json!({});
        }
        if let Some(incoming) = event.pointer(path).and_then(Value::as_object) {
            for (key, value) in incoming {
                let previous = self
                    .anthropic_footprints
                    .get(key)
                    .copied()
                    .unwrap_or_default();
                let mut replacement = crate::stream_retention::estimate_value(value)?;
                if previous.entries == 0 {
                    replacement = replacement
                        .checked_add(crate::stream_retention::RetainedFootprint::record(
                            key.len(),
                        ))
                        .ok_or_else(crate::stream_retention::retention_error)?;
                }
                self.replace(previous, replacement)?;
                self.anthropic[key] = value.clone();
                self.anthropic_footprints.insert(key.clone(), replacement);
            }
        }
        if path == "/usage" {
            event["usage"] = self.anthropic.clone();
        }
        Ok(())
    }

    pub(crate) fn clear(&mut self) {
        self.anthropic = json!({});
        self.chat_usage = None;
        self.chat_terminal = None;
        self.chat_choices.clear();
        self.known_chat_choices.clear();
        self.finished_chat_choices.clear();
        self.chat_provider_cost_floor = None;
        self.terminal_chat_provider_cost = None;
        self.anthropic_footprints.clear();
        self.chat_choice_footprints.clear();
        self.chat_usage_footprint = Default::default();
        self.chat_terminal_footprint = Default::default();
        self.budget.release(self.retained);
        self.retained = Default::default();
    }

    fn reserve(
        &mut self,
        footprint: crate::stream_retention::RetainedFootprint,
    ) -> crate::error::Result<()> {
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
    ) -> crate::error::Result<()> {
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
}

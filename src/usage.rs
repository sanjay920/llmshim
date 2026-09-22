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

pub(crate) fn normalize_native_response_usage(
    target: &ReplayTarget,
    native_response: &Value,
) -> Option<Value> {
    let native_usage = match target.wire {
        WireFormat::AnthropicMessages | WireFormat::OpenAiChat | WireFormat::OpenAiResponses => {
            native_response.get("usage")?
        }
        WireFormat::GoogleGenerateContent => native_response.get("usageMetadata")?,
    };
    normalize_native_usage(target.wire, native_usage)
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
}

impl NativeStreamUsage {
    pub(crate) fn new(target: ReplayTarget) -> Self {
        Self {
            target,
            anthropic_usage: json!({}),
        }
    }

    pub(crate) fn ingest(&mut self, native_event_text: &str) -> Option<Value> {
        let native_event: Value = serde_json::from_str(native_event_text).ok()?;
        match self.target.wire {
            WireFormat::AnthropicMessages => {
                let usage_path = match native_event["type"].as_str() {
                    Some("message_start") => "/message/usage",
                    Some("message_delta") => "/usage",
                    _ if native_event["usage"].is_object() => "/usage",
                    _ => return None,
                };
                let incoming = native_event.pointer(usage_path)?.as_object()?;
                for (key, value) in incoming {
                    self.anthropic_usage[key] = value.clone();
                }
                normalize_native_usage(self.target.wire, &self.anthropic_usage)
            }
            WireFormat::OpenAiResponses => native_event
                .get("response")
                .and_then(|response| normalize_native_response_usage(&self.target, response)),
            WireFormat::OpenAiChat => native_event
                .get("usage")
                .and_then(|usage| normalize_native_usage(self.target.wire, usage)),
            WireFormat::GoogleGenerateContent => native_event
                .get("usageMetadata")
                .and_then(|usage| normalize_native_usage(self.target.wire, usage)),
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
}

impl StreamUsage {
    /// Chat Completions may send usage after the finish-reason chunk. Delay the
    /// terminal marker so proxy clients do not stop before receiving accounting.
    pub(crate) fn defer_chat_terminal(&mut self, data: String) -> crate::error::Result<String> {
        let mut chunk: Value = serde_json::from_str(&data)?;
        if let Some(usage) = chunk.get("usage").filter(|v| v.is_object()) {
            self.chat_usage = Some(usage.clone());
        }
        let terminal = chunk.clone();
        if let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut) {
            for (i, choice) in choices.iter_mut().enumerate() {
                if !choice["finish_reason"].is_string() {
                    continue;
                }
                let mut done = choice.clone();
                done["delta"] = json!({});
                self.chat_choices
                    .insert(choice["index"].as_u64().unwrap_or(i as u64), done);
                choice["finish_reason"] = Value::Null;
            }
        }
        if !self.chat_choices.is_empty() {
            let pending = self.chat_terminal.get_or_insert(terminal);
            pending["choices"] = json!(self.chat_choices.values().collect::<Vec<_>>());
        }
        Ok(chunk.to_string())
    }

    pub(crate) fn take_terminal(&mut self) -> Option<String> {
        let mut terminal = self.chat_terminal.take()?;
        if let Some(usage) = self.chat_usage.take() {
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

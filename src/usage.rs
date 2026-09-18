//! Shared cache accounting. Native counters are read once at the transport
//! boundary; consumers never need to recognize a provider's usage dialect.
use serde_json::{json, Value};

fn counter(usage: &Value, paths: &[&str]) -> u64 {
    paths
        .iter()
        .find_map(|path| usage.pointer(path).and_then(Value::as_u64))
        .unwrap_or(0)
}

/// Add normalized read/write counts without changing other token semantics.
/// Zero means no cache tokens were reported. It is not a pricing assertion.
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

use serde::{ser::SerializeMap, Serialize, Serializer};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct Usage {
    pub input_tokens: u64,
    pub uncached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// USD accounting for this response. A provider floor is a known lower
    /// bound; `null` means the cost could not be known and never means free.
    ///
    /// Where a model prices some token classes and not others, the unpriced ones
    /// are charged at its highest published rate, so this is an **upper bound**
    /// rather than `null`. Under-reporting would let a spend cap stop binding;
    /// over-reporting merely spends a budget slightly early. See `llmshim::cost`.
    ///
    /// Unless `cost_source` says `"provider"`, in which case the number is not
    /// an estimate at all but what the provider reported charging. A
    /// `"provider_floor"` source is the highest partial provider bill observed;
    /// it is a known lower bound rather than an exact final invoice.
    pub cost_usd: Option<f64>,
    /// `"provider"` when `cost_usd` is the provider's own reported final bill,
    /// `"provider_floor"` for the highest partial provider bill observed, and
    /// `"catalog"` when it was computed from catalog prices.
    /// Absent on a usage object nothing has priced.
    pub cost_source: Option<String>,
}

impl Serialize for Usage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for (name, count) in [
            ("input_tokens", self.input_tokens),
            ("uncached_input_tokens", self.uncached_input_tokens),
            ("output_tokens", self.output_tokens),
            ("total_tokens", self.total_tokens),
            ("cache_read_tokens", self.cache_read_tokens),
            ("cache_write_tokens", self.cache_write_tokens),
        ] {
            map.serialize_entry(name, &count)?;
        }
        if self.reasoning_tokens != 0 {
            map.serialize_entry("reasoning_tokens", &self.reasoning_tokens)?;
        }
        map.serialize_entry("cost_usd", &self.cost_usd)?;
        if let Some(source) = &self.cost_source {
            map.serialize_entry("cost_source", source)?;
        }
        map.serialize_entry(
            "x-llmshim-usage",
            &json!({
                "uncached_input_tokens": self.uncached_input_tokens,
                "cache_read_tokens": self.cache_read_tokens,
                "cache_write_tokens": self.cache_write_tokens,
                "output_tokens": self.output_tokens,
                "reasoning_tokens": self.reasoning_tokens,
            }),
        )?;
        map.end()
    }
}

/// Extract usage from an OpenAI-format usage object.
pub(super) fn extract_usage(usage: &Value) -> Usage {
    let input = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning = usage
        .get("reasoning_tokens")
        .or_else(|| usage.pointer("/completion_tokens_details/reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(input + output);

    Usage {
        input_tokens: input,
        uncached_input_tokens: usage["uncached_input_tokens"].as_u64().unwrap_or(0),
        output_tokens: output,
        reasoning_tokens: reasoning,
        total_tokens: total,
        cache_read_tokens: usage["cache_read_tokens"].as_u64().unwrap_or(0),
        cache_write_tokens: usage["cache_write_tokens"].as_u64().unwrap_or(0),
        cost_usd: crate::cost::stamped(usage),
        cost_source: crate::cost::stamped_source(usage).map(str::to_owned),
    }
}

#[cfg(test)]
#[test]
fn cache_accounting_survives_proxy_projection() {
    let usage = extract_usage(
        &json!({"prompt_tokens": 10, "cache_read_tokens": 7, "cache_write_tokens": 3}),
    );
    let wire = serde_json::to_value(usage).unwrap();
    assert_eq!(wire["cache_read_tokens"], 7);
    assert_eq!(wire["cache_write_tokens"], 3);
    let empty = serde_json::to_value(extract_usage(&json!({}))).unwrap();
    assert_eq!(empty["cache_read_tokens"], 0);
    assert_eq!(empty["cache_write_tokens"], 0);
    let provider_floor = serde_json::to_value(extract_usage(&json!({
        "cost_usd": 1.0,
        "cost_source": "provider_floor"
    })))
    .unwrap();
    assert_eq!(provider_floor["cost_usd"], 1.0);
    assert_eq!(provider_floor["cost_source"], "provider_floor");
    let events = super::convert::chunk_to_events(
        &json!({"choices":[],"usage":{"cache_read_tokens":9}}).to_string(),
    );
    assert!(matches!(
        &events[..],
        [super::types::StreamEvent::Usage(Usage {
            cache_read_tokens: 9,
            ..
        })]
    ));
}

#[cfg(test)]
#[test]
fn proxy_usage_preserves_normalized_counts_without_guessing_native_dialects() {
    let explicit = extract_usage(&json!({
        "prompt_tokens": 20, "uncached_input_tokens": 11,
        "cache_read_tokens": 7, "cache_write_tokens": 2,
        "reasoning_tokens": 3, "completion_tokens_details": {"reasoning_tokens": 4}
    }));
    assert_eq!(explicit.uncached_input_tokens, 11);
    assert_eq!(explicit.reasoning_tokens, 3);
    let nested = extract_usage(&json!({"completion_tokens_details":{"reasoning_tokens":4}}));
    assert_eq!(nested.reasoning_tokens, 4);
    for value in [
        json!({}),
        json!({"uncached_input_tokens":-1,"cache_read_tokens":"7",
        "cache_write_tokens":null,"reasoning_tokens":false}),
    ] {
        let usage = serde_json::to_value(extract_usage(&value)).unwrap();
        assert_eq!(
            usage["x-llmshim-usage"],
            json!({"uncached_input_tokens":0,
            "cache_read_tokens":0,"cache_write_tokens":0,"output_tokens":0,"reasoning_tokens":0})
        );
    }
}

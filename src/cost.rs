//! USD cost accounting for a completed response.
//!
//! The catalog carries prices in USD per million tokens ([`Cost`]); this module
//! is the only thing that multiplies them by the normalized usage counters
//! [`crate::usage`] produces.
//!
//! **An absent price is never zero.** A model the catalog cannot price yields
//! `None`, and so does a model priced for input but not for the cache reads a
//! response actually used. A `0.0` that means "unknown" is how a spend dashboard
//! silently under-reports, so unknown is unrepresentable as a number here.

use crate::catalog::Cost;
use serde_json::Value;

/// USD per token, from a catalog price quoted per million tokens.
const PER_MILLION: f64 = 1_000_000.0;

fn count(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Charge `tokens` at `rate` USD per million. Zero tokens cost nothing even
/// when the rate is unknown; any positive count with an unknown rate poisons
/// the whole total, because a partial sum would read as a complete one.
fn charge(tokens: u64, rate: Option<f64>) -> Option<f64> {
    if tokens == 0 {
        return Some(0.0);
    }
    Some(tokens as f64 * rate? / PER_MILLION)
}

/// The highest rate this model publishes, used where a class has none of its own.
///
/// Catalogs price partially and often: **2,537 of 7,461 priced models in the
/// vendored snapshot carry no `cache_read` rate and 5,892 no `cache_write`**,
/// including `gpt-5-pro` and `o3-pro`, priced `{input, output}` only. Returning
/// `None` for those made the whole response unpriceable the moment it reported a
/// cached token — and a discarded charge is a spend cap that silently stops
/// binding, which is the failure this crate exists to prevent.
///
/// Substituting the highest published rate can only over-estimate, never under.
/// For a cap that is the safe direction: it spends a budget slightly early, where
/// under-estimating spends it forever.
fn highest_published_rate(cost: &Cost) -> Option<f64> {
    [cost.input, cost.output, cost.cache_read, cost.cache_write]
        .into_iter()
        .flatten()
        .fold(None, |best: Option<f64>, rate| {
            Some(best.map_or(rate, |b| b.max(rate)))
        })
}

/// Price one normalized usage object.
///
/// Input is charged on `uncached_input_tokens` — the prompt minus whatever
/// cache read the provider already counted inside it — so a cached prompt is
/// never billed twice. Cache reads and cache writes are charged separately at
/// their own rates. Reasoning tokens are already inside `completion_tokens` on
/// every transport and are not charged again.
pub fn price(usage: &Value, cost: &Cost) -> Option<f64> {
    let cache_read = count(usage, "cache_read_tokens");
    let cache_write = count(usage, "cache_write_tokens");
    // Older or hand-built usage objects predate the uncached counter; the
    // prompt total is then the best available input count.
    let input = usage
        .get("uncached_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| count(usage, "prompt_tokens"));

    // Every counter zero means the response reported no usage at all — a
    // response that consumed nothing does not exist. Pricing it at $0 would be
    // the same silent under-report as pricing an unpriced model at $0.
    if input == 0 && cache_read == 0 && cache_write == 0 && count(usage, "completion_tokens") == 0 {
        return None;
    }

    // A class the model does not price falls back to its highest published rate
    // rather than voiding the total. See `highest_published_rate`: the result is
    // an upper bound, which is the only safe direction for a budget.
    let ceiling = highest_published_rate(cost);
    let rate_for = |own: Option<f64>| own.or(ceiling);

    Some(
        charge(input, rate_for(cost.input))?
            + charge(count(usage, "completion_tokens"), rate_for(cost.output))?
            + charge(cache_read, rate_for(cost.cache_read))?
            + charge(cache_write, rate_for(cost.cache_write))?,
    )
}

/// The catalog's price for a model string, in any spelling it carries
/// (`provider/model`, a bare name, or an alias). `None` when the model is
/// unknown or carries no pricing.
pub fn for_model(model: &str) -> Option<Cost> {
    crate::catalog::resolve(model)?.cost
}

/// Price for a dispatch target. The catalog keys models as `provider/name`;
/// the bare name is the fallback for aggregators and self-hosted servers whose
/// qualified id the catalog does not carry.
pub fn for_target(provider: &str, model: &str) -> Option<Cost> {
    for_model(&format!("{provider}/{model}")).or_else(|| for_model(model))
}

/// USD cost of one response's usage, or `None` when it cannot be known.
pub fn cost_usd(provider: &str, model: &str, usage: &Value) -> Option<f64> {
    let info = crate::catalog::resolve(&format!("{provider}/{model}"))
        .filter(|m| m.cost.is_some())
        .or_else(|| crate::catalog::resolve(model))?;
    // Normalized uncached input excludes reads/writes for every provider.
    // Tier selection includes them; subtracting cache hits would undercharge
    // long cached conversations. Legacy usage falls back to its prompt total.
    let input_tokens = usage
        .get("uncached_input_tokens")
        .and_then(Value::as_u64)
        .map(|n| {
            n.saturating_add(count(usage, "cache_read_tokens"))
                .saturating_add(count(usage, "cache_write_tokens"))
        })
        .unwrap_or_else(|| count(usage, "prompt_tokens"));
    price(usage, &info.cost_for_input_tokens(input_tokens)?)
}

/// Whether this target can be priced at all, asked *before* the request runs.
///
/// Pricing after the fact cannot enforce a budget: by then the money is spent.
/// A cap therefore needs this question answered up front.
pub fn is_priceable(provider: &str, model: &str) -> bool {
    for_target(provider, model).is_some()
}

/// Read a cost already stamped onto a normalized usage object. A stamped
/// `null` and an unstamped body both mean "not known".
pub fn stamped(usage: &Value) -> Option<f64> {
    usage.get("cost_usd").and_then(Value::as_f64)
}

/// Stamp `usage.cost_usd` on a normalized response. The key is always present
/// so a reader never has to distinguish "absent" from "free"; `null` is the
/// explicit unknown.
pub fn stamp(provider: &str, model: &str, response: &mut Value) {
    if !response["usage"].is_object() {
        return;
    }
    let cost = cost_usd(provider, model, &response["usage"]);
    response["usage"]["cost_usd"] = match cost {
        Some(usd) => serde_json::json!(usd),
        None => Value::Null,
    };
}

/// Stamp a streaming chunk in place of a full response. Chunks without a usage
/// object are returned untouched, and an unparseable chunk is never rewritten.
pub fn stamp_chunk(provider: &str, model: &str, chunk: String) -> String {
    // Chat Completions streams carry `"usage":null` on every chunk when usage
    // is requested; matching the opening brace keeps the hot path from parsing
    // and reserializing once per token. serde_json's compact output makes the
    // spelling exact.
    if !chunk.contains("\"usage\":{") {
        return chunk;
    }
    let Ok(mut value) = serde_json::from_str::<Value>(&chunk) else {
        return chunk;
    };
    if !value["usage"].is_object() {
        return chunk;
    }
    stamp(provider, model, &mut value);
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_context_threshold_counts_cached_input_and_reprices_the_whole_response() {
        for model in ["grok-4.6", "grok-4.7"] {
            let mut usage = serde_json::json!({"uncached_input_tokens":50_000,"cache_read_tokens":150_000,"completion_tokens":1000});
            close(cost_usd("xai", model, &usage), 0.1 + 0.075 + 0.006);
            usage["cache_read_tokens"] = serde_json::json!(150_001);
            close(cost_usd("xai", model, &usage), 0.2 + 0.150001 + 0.012);
        }
    }
    use serde_json::json;

    const TOLERANCE: f64 = 1e-12;

    fn priced() -> Cost {
        Cost {
            input: Some(3.0),
            output: Some(15.0),
            cache_read: Some(0.3),
            cache_write: Some(3.75),
        }
    }

    fn close(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("a priced model must produce a cost");
        assert!(
            (actual - expected).abs() < TOLERANCE,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn exact_arithmetic_on_a_known_price() {
        // 1M uncached input @ $3 + 1M output @ $15.
        let usage = json!({"uncached_input_tokens": 1_000_000, "completion_tokens": 1_000_000});
        close(price(&usage, &priced()), 18.0);
    }

    #[test]
    fn cache_reads_and_writes_are_priced_separately() {
        let usage = json!({
            "uncached_input_tokens": 1_000_000,
            "completion_tokens": 0,
            "cache_read_tokens": 1_000_000,
            "cache_write_tokens": 1_000_000,
        });
        // 3.00 + 0.30 + 3.75 — each bucket at its own rate, not the input rate.
        close(price(&usage, &priced()), 7.05);
    }

    #[test]
    fn a_cached_prompt_is_not_billed_twice() {
        // An inclusive-prompt provider: 1M prompt of which 900k was cached.
        let mut usage = json!({"prompt_tokens": 1_000_000, "completion_tokens": 0});
        crate::usage::normalize_cache(
            &json!({"prompt_tokens": 1_000_000, "prompt_tokens_details": {"cached_tokens": 900_000}}),
            &mut usage,
        );
        // 100k @ $3 + 900k @ $0.30.
        close(price(&usage, &priced()), 0.57);
    }

    #[test]
    fn an_exclusive_prompt_provider_keeps_its_whole_prompt() {
        // Anthropic reports input_tokens *without* the cache read.
        let mut usage = json!({"prompt_tokens": 100_000, "completion_tokens": 0});
        crate::usage::normalize_cache(
            &json!({"input_tokens": 100_000, "cache_read_input_tokens": 900_000}),
            &mut usage,
        );
        assert_eq!(usage["uncached_input_tokens"], 100_000);
        close(price(&usage, &priced()), 0.57);
    }

    #[test]
    fn an_unreported_usage_object_is_unknown_not_free() {
        // A provider that reported nothing leaves every counter at zero. That
        // is missing data, not a free response.
        assert_eq!(
            price(
                &json!({"prompt_tokens":0,"completion_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"uncached_input_tokens":0}),
                &priced()
            ),
            None
        );
        assert_eq!(price(&json!({}), &priced()), None);
        // One real token is enough to make the total meaningful again.
        close(
            price(&json!({"uncached_input_tokens": 1_000_000}), &priced()),
            3.0,
        );
    }

    #[test]
    fn a_model_with_no_price_is_unknown_not_free() {
        let usage = json!({"uncached_input_tokens": 1_000, "completion_tokens": 1_000});
        assert_eq!(price(&usage, &Cost::default()), None);
    }

    #[test]
    fn a_missing_cache_rate_is_charged_at_the_models_highest_rate() {
        // This replaced an assertion that a used-but-unpriced class voids the
        // total. That instinct was right — never under-report — but the remedy
        // was wrong: `None` is discarded by `SpendCap::record`, so a spend cap
        // silently stopped binding for the 2,537 catalogued models that publish
        // no `cache_read` rate. An upper bound honours the same principle
        // without the hole.
        let partial = Cost {
            input: Some(3.0),
            output: Some(15.0),
            cache_read: None,
            cache_write: None,
        };

        // No cache tokens: the missing rates never come into play.
        close(
            price(
                &json!({"uncached_input_tokens": 1_000_000, "completion_tokens": 0, "cache_read_tokens": 0}),
                &partial,
            ),
            3.0,
        );

        // Cache tokens used with no rate of their own: charged at 15.0, the
        // highest rate this model publishes — an over-estimate, never an under.
        close(
            price(
                &json!({"uncached_input_tokens": 1_000_000, "cache_read_tokens": 1_000_000}),
                &partial,
            ),
            18.0,
        );

        // The bound must never fall below what a complete price would charge.
        let complete = Cost {
            cache_read: Some(0.3),
            ..partial
        };
        let usage = json!({"uncached_input_tokens": 1_000_000, "cache_read_tokens": 1_000_000});
        assert!(
            price(&usage, &partial).unwrap() >= price(&usage, &complete).unwrap(),
            "a fallback rate must bound the real one from above, or a cap under-charges"
        );
    }

    #[test]
    fn stamp_always_writes_the_key() {
        let mut response = json!({"usage": {"uncached_input_tokens": 1, "completion_tokens": 1}});
        stamp("nowhere", "definitely-not-a-model", &mut response);
        assert!(
            response["usage"]["cost_usd"].is_null(),
            "an unpriceable model must stamp null, never 0"
        );
        assert!(response["usage"].get("cost_usd").is_some());
    }

    #[test]
    fn a_response_without_usage_is_left_alone() {
        let mut response = json!({"id": "r1"});
        stamp("nowhere", "anything", &mut response);
        assert!(response.get("usage").is_none());
    }

    #[test]
    fn stamp_chunk_leaves_usageless_chunks_untouched() {
        assert_eq!(stamp_chunk("nowhere", "m", "[DONE]".into()), "[DONE]");
        let no_usage = json!({"choices": []}).to_string();
        assert_eq!(stamp_chunk("nowhere", "m", no_usage.clone()), no_usage);
        let stamped = stamp_chunk(
            "nowhere",
            "m",
            json!({"usage": {"prompt_tokens": 1}}).to_string(),
        );
        assert!(stamped.contains("cost_usd"));
    }
}

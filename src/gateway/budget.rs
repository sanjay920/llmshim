use crate::policy::{AttemptOutcome, PreparedAttempt};
use crate::reasoning::WireFormat;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const NANOS_PER_USD: f64 = 1_000_000_000.0;
const PER_MILLION: u128 = 1_000_000;
pub(crate) const MAX_EXACT_REDIS_NANOS: u64 = 1_u64 << 53;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct BudgetPolicySnapshot {
    pub(crate) limit_nanos: u64,
    pub(crate) window_secs: u64,
    pub(crate) allow_unpriced: bool,
    pub(crate) valid: bool,
}

impl BudgetPolicySnapshot {
    pub(crate) fn from_identity(identity: &crate::gateway::auth::Identity) -> Option<Self> {
        let budget_usd = identity.budget_usd?;
        let scaled = budget_usd * NANOS_PER_USD;
        let valid = budget_usd.is_finite()
            && budget_usd >= 0.0
            && scaled.is_finite()
            && scaled <= MAX_EXACT_REDIS_NANOS as f64;
        Some(Self {
            limit_nanos: if valid { scaled.floor() as u64 } else { 0 },
            window_secs: identity
                .budget_window_secs
                .unwrap_or(crate::gateway::quota::DEFAULT_BUDGET_WINDOW_SECS)
                .max(1),
            allow_unpriced: identity.budget_allow_unpriced,
            valid,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BudgetReservation {
    pub(crate) amount_nanos: u64,
    pub(crate) window_index: u64,
    pub(crate) expires_at_epoch_secs: u64,
    pub(crate) bounded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BudgetPlanError {
    Unpriceable,
}

pub(crate) fn reservation(
    policy: Option<&BudgetPolicySnapshot>,
    attempt: &PreparedAttempt<'_>,
    now_epoch_secs: u64,
) -> Result<Option<BudgetReservation>, BudgetPlanError> {
    let Some(policy) = policy else {
        return Ok(None);
    };
    if !policy.valid {
        return Err(BudgetPlanError::Unpriceable);
    }
    let (amount_nanos, bounded) = match conservative_reservation_nanos(attempt)
        .filter(|amount| *amount <= MAX_EXACT_REDIS_NANOS)
    {
        Some(amount) => (amount, true),
        None if policy.allow_unpriced => (0, false),
        None => return Err(BudgetPlanError::Unpriceable),
    };
    let window_index = now_epoch_secs / policy.window_secs;
    let next_window = window_index
        .saturating_add(1)
        .saturating_mul(policy.window_secs);
    Ok(Some(BudgetReservation {
        amount_nanos,
        window_index,
        expires_at_epoch_secs: next_window.saturating_add(policy.window_secs),
        bounded,
    }))
}

pub(crate) fn observed_cost_nanos(usage: &Value) -> Option<u64> {
    let cost_usd = crate::cost::stamped(usage)?;
    if !cost_usd.is_finite() || cost_usd < 0.0 {
        return None;
    }
    let scaled = cost_usd * NANOS_PER_USD;
    if !scaled.is_finite() || scaled > MAX_EXACT_REDIS_NANOS as f64 {
        return None;
    }
    Some(scaled.ceil() as u64)
}

pub(crate) fn may_finalize(outcome: AttemptOutcome) -> bool {
    matches!(
        outcome,
        AttemptOutcome::Completed {
            accounting: crate::policy::AttemptAccounting::UsageObserved
        } | AttemptOutcome::InvalidResponse {
            accounting: crate::policy::AttemptAccounting::UsageObserved
        }
    )
}

fn conservative_reservation_nanos(attempt: &PreparedAttempt<'_>) -> Option<u64> {
    let identity = attempt.identity();
    let provider = identity.provider_name();
    let model = identity.native_model();
    let body = attempt.native_body();

    if has_unbounded_paid_control(provider, model, identity.wire(), body)
        || contains_unbounded_media(body)
    {
        return None;
    }

    let qualified = format!("{provider}/{model}");
    let model_info = crate::catalog::lookup_id(&qualified)?;
    let input_tokens = u64::from(model_info.context_window_tokens?);
    let output_tokens = output_token_bound(
        identity.wire(),
        body,
        u64::from(model_info.max_output_tokens?),
    );
    let applicable_cost = model_info.cost_for_input_tokens(input_tokens)?;
    let rate_per_million = [
        applicable_cost.input?,
        applicable_cost.output?,
        applicable_cost.cache_read.unwrap_or_default(),
        applicable_cost.cache_write.unwrap_or_default(),
    ]
    .into_iter()
    .try_fold(0.0_f64, |highest, rate| {
        (rate.is_finite() && rate >= 0.0).then_some(highest.max(rate))
    })?;
    let rate_nanos_per_million = (rate_per_million * NANOS_PER_USD).ceil();
    if !rate_nanos_per_million.is_finite()
        || rate_nanos_per_million < 0.0
        || rate_nanos_per_million > u64::MAX as f64
    {
        return None;
    }
    let bounded_tokens = u128::from(input_tokens).checked_add(u128::from(output_tokens))?;
    let numerator = bounded_tokens.checked_mul(rate_nanos_per_million as u128)?;
    let nanos = numerator
        .checked_add(PER_MILLION - 1)?
        .checked_div(PER_MILLION)?;
    u64::try_from(nanos).ok()
}

fn declared_output_bound(wire: WireFormat, body: &Value) -> Option<u64> {
    match wire {
        WireFormat::AnthropicMessages => body.get("max_tokens").and_then(Value::as_u64),
        WireFormat::OpenAiResponses => body.get("max_output_tokens").and_then(Value::as_u64),
        WireFormat::OpenAiChat => body
            .get("max_completion_tokens")
            .or_else(|| body.get("max_tokens"))
            .and_then(Value::as_u64),
        WireFormat::GoogleGenerateContent => body
            .pointer("/generationConfig/maxOutputTokens")
            .and_then(Value::as_u64),
    }
}

fn reasoning_output_bound(wire: WireFormat, body: &Value) -> Option<u64> {
    match wire {
        WireFormat::AnthropicMessages => body
            .pointer("/thinking/budget_tokens")
            .and_then(Value::as_u64),
        WireFormat::GoogleGenerateContent => body
            .pointer("/generationConfig/thinkingConfig/thinkingBudget")
            .and_then(Value::as_u64),
        WireFormat::OpenAiResponses | WireFormat::OpenAiChat => body
            .pointer("/reasoning/max_tokens")
            .and_then(Value::as_u64),
    }
}

fn output_token_bound(wire: WireFormat, body: &Value, catalog_default: u64) -> u64 {
    declared_output_bound(wire, body)
        .unwrap_or(catalog_default)
        .max(reasoning_output_bound(wire, body).unwrap_or_default())
}

fn has_unbounded_paid_control(provider: &str, model: &str, wire: WireFormat, body: &Value) -> bool {
    if body.get("priority").is_some()
        || body.get("service_tier").is_some()
        || body.get("speed").is_some()
        || contains_key(body, "cache_control")
        || contains_key(body, "prompt_cache_retention")
    {
        return true;
    }
    if provider == "openrouter"
        && (body.get("models").is_some()
            || body.get("provider").is_some()
            || crate::catalog::strip_variant_suffix(model) != model)
    {
        return true;
    }
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return false;
    };
    tools.iter().any(|tool| match wire {
        WireFormat::OpenAiResponses | WireFormat::OpenAiChat => {
            tool.get("type").and_then(Value::as_str) != Some("function")
        }
        WireFormat::AnthropicMessages => {
            tool.get("type").is_some()
                || tool.get("name").and_then(Value::as_str).is_none()
                || !tool.get("input_schema").is_some_and(Value::is_object)
        }
        WireFormat::GoogleGenerateContent => {
            let Some(object) = tool.as_object() else {
                return true;
            };
            object.len() != 1
                || !object
                    .get("functionDeclarations")
                    .is_some_and(Value::is_array)
        }
    })
}

fn contains_key(value: &Value, needle: &str) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| contains_key(value, needle)),
        Value::Object(object) => {
            object.contains_key(needle) || object.values().any(|value| contains_key(value, needle))
        }
        _ => false,
    }
}

fn contains_unbounded_media(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_unbounded_media),
        Value::Object(object) => object.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "image_url"
                    | "input_image"
                    | "input_audio"
                    | "audio"
                    | "video"
                    | "inlineData"
                    | "fileData"
            ) || (key == "type"
                && value.as_str().is_some_and(|kind| {
                    kind.contains("image")
                        || kind.contains("audio")
                        || kind.contains("video")
                        || kind.contains("document")
                }))
                || contains_unbounded_media(value)
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(budget_usd: f64) -> crate::gateway::auth::Identity {
        crate::gateway::auth::Identity {
            tenant: "test".into(),
            tier: 0,
            rpm: None,
            tpm: None,
            budget_usd: Some(budget_usd),
            budget_window_secs: Some(0),
            budget_allow_unpriced: true,
        }
    }

    #[test]
    fn budget_snapshot_rejects_invalid_arithmetic_and_rounds_caps_down() {
        assert!(
            !BudgetPolicySnapshot::from_identity(&identity(-1.0))
                .unwrap()
                .valid
        );
        assert!(
            !BudgetPolicySnapshot::from_identity(&identity(f64::NAN))
                .unwrap()
                .valid
        );
        assert!(
            !BudgetPolicySnapshot::from_identity(&identity(10_000_000.0))
                .unwrap()
                .valid
        );
        let tiny = BudgetPolicySnapshot::from_identity(&identity(0.000_000_000_9)).unwrap();
        assert!(tiny.valid);
        assert_eq!(tiny.limit_nanos, 0);
        assert_eq!(tiny.window_secs, 1);
    }

    #[test]
    fn observed_cost_rounds_up_and_rejects_invalid_values() {
        assert_eq!(
            observed_cost_nanos(&serde_json::json!({"cost_usd": 0.0000000001})),
            Some(1)
        );
        assert_eq!(
            observed_cost_nanos(&serde_json::json!({"cost_usd": -1.0})),
            None
        );
        assert_eq!(
            observed_cost_nanos(&serde_json::json!({"cost_usd": "unknown"})),
            None
        );
    }

    #[test]
    fn only_final_known_usage_can_release_a_reservation() {
        use crate::policy::{AttemptAccounting, AttemptKind};
        assert!(may_finalize(AttemptOutcome::InvalidResponse {
            accounting: AttemptAccounting::UsageObserved,
        }));
        assert!(!may_finalize(AttemptOutcome::Abandoned {
            kind: AttemptKind::Stream,
            accounting: AttemptAccounting::UsageObserved,
        }));
        assert!(!may_finalize(AttemptOutcome::Completed {
            accounting: AttemptAccounting::NoUsageReported,
        }));
    }

    #[test]
    fn reasoning_without_a_declared_output_limit_keeps_the_catalog_ceiling() {
        assert_eq!(
            output_token_bound(
                WireFormat::OpenAiResponses,
                &serde_json::json!({"reasoning": {"max_tokens": 100}}),
                128_000,
            ),
            128_000
        );
        assert_eq!(
            output_token_bound(
                WireFormat::OpenAiResponses,
                &serde_json::json!({
                    "max_output_tokens": 50,
                    "reasoning": {"max_tokens": 100}
                }),
                128_000,
            ),
            100
        );
    }
}

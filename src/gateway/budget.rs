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
pub(crate) struct BudgetQuote {
    pub(crate) amount_nanos: u64,
    pub(crate) bounded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BudgetPlanError {
    Unpriceable,
}

pub(crate) fn quote(
    policy: Option<&BudgetPolicySnapshot>,
    attempt: &PreparedAttempt<'_>,
) -> Result<Option<BudgetQuote>, BudgetPlanError> {
    let Some(policy) = policy else {
        return Ok(None);
    };
    if !policy.valid {
        return Err(BudgetPlanError::Unpriceable);
    }
    if policy.limit_nanos == 0 {
        return Ok(Some(BudgetQuote {
            amount_nanos: 0,
            bounded: true,
        }));
    }
    let (amount_nanos, bounded) = match conservative_reservation_nanos(attempt)
        .filter(|amount| *amount <= MAX_EXACT_REDIS_NANOS)
    {
        Some(amount) => (amount, true),
        None if policy.allow_unpriced => (0, false),
        None => return Err(BudgetPlanError::Unpriceable),
    };
    Ok(Some(BudgetQuote {
        amount_nanos,
        bounded,
    }))
}

pub(crate) fn observed_cost_nanos(usage: &Value) -> Option<u64> {
    let cost_usd = crate::cost::stamped(usage)?;
    if !cost_usd.is_finite() || cost_usd < 0.0 {
        return None;
    }
    if cost_usd >= MAX_EXACT_REDIS_NANOS as f64 / NANOS_PER_USD {
        return Some(MAX_EXACT_REDIS_NANOS);
    }
    let scaled = cost_usd * NANOS_PER_USD;
    Some(scaled.ceil() as u64)
}

#[cfg(feature = "redis-coordination")]
pub(crate) fn legacy_spend_nanos(cost_usd: f64) -> Option<u64> {
    if !cost_usd.is_finite() || cost_usd < 0.0 {
        return None;
    }
    if cost_usd >= MAX_EXACT_REDIS_NANOS as f64 / NANOS_PER_USD {
        return Some(MAX_EXACT_REDIS_NANOS);
    }
    Some((cost_usd * NANOS_PER_USD).ceil() as u64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpendAuthority {
    Catalog,
    Provider,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SpendObservation {
    pub(crate) amount_nanos: Option<u64>,
    pub(crate) authority: SpendAuthority,
    pub(crate) terminal_authoritative: bool,
}

pub(crate) fn spend_observation(
    observation: crate::policy::AttemptUsageObservation<'_>,
) -> SpendObservation {
    let authority =
        if crate::cost::stamped_source(observation.usage()) == Some(crate::cost::SOURCE_PROVIDER) {
            SpendAuthority::Provider
        } else {
            SpendAuthority::Catalog
        };
    let stamped_amount = observed_cost_nanos(observation.usage());
    let inferred_zero =
        stamped_amount.is_none() && observation.counters_complete() && observation.explicit_zero();
    let amount_nanos = stamped_amount.or_else(|| inferred_zero.then_some(0));
    let terminal_authoritative = observation.terminal()
        && amount_nanos.is_some()
        && (authority == SpendAuthority::Provider || observation.counters_complete());
    SpendObservation {
        amount_nanos,
        authority,
        terminal_authoritative,
    }
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

    if !supported_pricing_shape(provider, model, identity.wire(), body) {
        return None;
    }

    let qualified = format!("{provider}/{model}");
    let model_info = crate::catalog::lookup_id(&qualified)?;
    if !trusted_limit_source(&model_info, "context_window_tokens") {
        return None;
    }
    let input_tokens = u64::from(model_info.context_window_tokens?);
    let declared_output = declared_output_bound(identity.wire(), body);
    let catalog_output = match declared_output {
        Some(_) => 0,
        None if trusted_limit_source(&model_info, "max_output_tokens") => {
            u64::from(model_info.max_output_tokens?)
        }
        None => return None,
    };
    let output_tokens = output_token_bound(identity.wire(), body, catalog_output);
    let applicable_cost = trusted_applicable_cost(&model_info, input_tokens)?;
    let rate_per_million = [applicable_cost.input?, applicable_cost.output?]
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

fn trusted_limit_source(model: &crate::catalog::ModelInfo, field: &str) -> bool {
    matches!(
        model.field_sources.get(field),
        Some(
            crate::catalog::CatalogSource::Local
                | crate::catalog::CatalogSource::ProviderApi
                | crate::catalog::CatalogSource::Builtin
        )
    )
}

fn trusted_price_source(model: &crate::catalog::ModelInfo, field: &str) -> bool {
    matches!(
        model.field_sources.get(field),
        Some(crate::catalog::CatalogSource::Local | crate::catalog::CatalogSource::Builtin)
    )
}

fn trusted_applicable_cost(
    model: &crate::catalog::ModelInfo,
    input_tokens: u64,
) -> Option<crate::catalog::Cost> {
    let mut input_is_trusted = trusted_price_source(model, "cost.input");
    let mut output_is_trusted = trusted_price_source(model, "cost.output");
    if let Some(tiers) = model.context_cost_tiers.as_deref() {
        if !trusted_price_source(model, "context_cost_tiers") {
            return None;
        }
        let mut applicable: Vec<_> = tiers
            .iter()
            .filter(|tier| input_tokens > tier.above_input_tokens)
            .collect();
        applicable.sort_by_key(|tier| tier.above_input_tokens);
        for tier in applicable {
            if tier.cost.input.is_some() {
                input_is_trusted = true;
            }
            if tier.cost.output.is_some() {
                output_is_trusted = true;
            }
        }
    }
    if !input_is_trusted || !output_is_trusted {
        return None;
    }
    model.cost_for_input_tokens(input_tokens)
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

fn supported_pricing_shape(provider: &str, model: &str, wire: WireFormat, body: &Value) -> bool {
    if body.get("priority").is_some()
        || body.get("service_tier").is_some()
        || body.get("speed").is_some()
        || contains_key(body, "cache_control")
        || contains_key(body, "prompt_cache_retention")
        || contains_unbounded_media(body)
    {
        return false;
    }
    if wire == WireFormat::OpenAiChat
        && body.get("n").is_some_and(|count| count.as_u64() != Some(1))
    {
        return false;
    }
    if provider == "openrouter"
        && (model == "auto"
            || model.starts_with("openrouter/")
            || crate::catalog::strip_variant_suffix(model) != model
            || body.get("transforms").is_some_and(|transforms| {
                transforms.as_array().is_none_or(|items| !items.is_empty())
            }))
    {
        return false;
    }
    if !only_supported_fields(provider, wire, body) {
        return false;
    }
    body.get("tools")
        .and_then(Value::as_array)
        .is_none_or(|tools| tools.iter().all(|tool| supported_tool(wire, tool)))
}

fn supported_tool(wire: WireFormat, tool: &Value) -> bool {
    match wire {
        WireFormat::OpenAiResponses | WireFormat::OpenAiChat => {
            tool.get("type").and_then(Value::as_str) == Some("function")
        }
        WireFormat::AnthropicMessages => {
            tool.get("type").is_none()
                && tool.get("name").and_then(Value::as_str).is_some()
                && tool.get("input_schema").is_some_and(Value::is_object)
        }
        WireFormat::GoogleGenerateContent => {
            let Some(object) = tool.as_object() else {
                return false;
            };
            object.len() == 1
                && object
                    .get("functionDeclarations")
                    .is_some_and(Value::is_array)
        }
    }
}

fn only_supported_fields(provider: &str, wire: WireFormat, body: &Value) -> bool {
    let Some(object) = body.as_object() else {
        return false;
    };
    let allowed: &[&str] = match wire {
        WireFormat::OpenAiResponses => &[
            "model",
            "input",
            "store",
            "max_output_tokens",
            "temperature",
            "top_p",
            "reasoning",
            "stream",
            "tools",
            "tool_choice",
            "include",
            "prompt_cache_key",
            "safety_identifier",
            "instructions",
            "text",
            "parallel_tool_calls",
            "metadata",
        ],
        WireFormat::OpenAiChat => &[
            "model",
            "messages",
            "max_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
            "top_k",
            "frequency_penalty",
            "presence_penalty",
            "repetition_penalty",
            "stop",
            "seed",
            "stream",
            "stream_options",
            "usage",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "response_format",
            "logprobs",
            "top_logprobs",
            "n",
            "reasoning",
            "transforms",
        ],
        WireFormat::AnthropicMessages => &[
            "model",
            "max_tokens",
            "messages",
            "system",
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "stream",
            "tools",
            "tool_choice",
            "thinking",
            "output_config",
        ],
        WireFormat::GoogleGenerateContent => &[
            "contents",
            "systemInstruction",
            "generationConfig",
            "tools",
            "toolConfig",
        ],
    };
    if !object.keys().all(|field| allowed.contains(&field.as_str())) {
        return false;
    }
    if provider == "openrouter"
        && object
            .keys()
            .any(|field| matches!(field.as_str(), "provider" | "models" | "route"))
    {
        return false;
    }
    if wire == WireFormat::GoogleGenerateContent {
        let allowed_generation = [
            "temperature",
            "topP",
            "topK",
            "maxOutputTokens",
            "stopSequences",
            "thinkingConfig",
            "responseMimeType",
            "responseSchema",
        ];
        if body
            .get("generationConfig")
            .and_then(Value::as_object)
            .is_some_and(|generation| {
                generation
                    .keys()
                    .any(|field| !allowed_generation.contains(&field.as_str()))
            })
        {
            return false;
        }
    }
    true
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
                    | "file_id"
                    | "file_url"
                    | "input_file"
                    | "inlineData"
                    | "fileData"
                    | "inline_data"
                    | "file_data"
            ) || (key == "type"
                && value.as_str().is_some_and(|kind| {
                    kind.contains("image")
                        || kind.contains("audio")
                        || kind.contains("video")
                        || kind.contains("document")
                        || kind == "file"
                        || kind.ends_with("_file")
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

    #[test]
    fn strict_catalog_policy_requires_trusted_sources_per_field() {
        let community = crate::catalog::lookup_id("openai/gpt-5.6-luna").unwrap();
        assert!(!trusted_price_source(&community, "cost.input"));
        assert!(!trusted_price_source(&community, "cost.output"));
        assert!(trusted_applicable_cost(
            &community,
            u64::from(community.context_window_tokens.unwrap())
        )
        .is_none());

        let verified = crate::catalog::lookup_id("openrouter/x-ai/grok-4.7").unwrap();
        assert!(trusted_limit_source(&verified, "context_window_tokens"));
        assert!(trusted_limit_source(&verified, "max_output_tokens"));
        assert!(trusted_price_source(&verified, "cost.input"));
        assert!(trusted_price_source(&verified, "cost.output"));
        assert!(trusted_price_source(&verified, "context_cost_tiers"));
        assert!(trusted_applicable_cost(
            &verified,
            u64::from(verified.context_window_tokens.unwrap())
        )
        .is_some());

        let mut mixed = verified.as_ref().clone();
        mixed.field_sources.insert(
            "context_cost_tiers".into(),
            crate::catalog::CatalogSource::ModelsDev,
        );
        assert!(!trusted_price_source(&mixed, "context_cost_tiers"));
        assert!(
            trusted_applicable_cost(&mixed, u64::from(mixed.context_window_tokens.unwrap()))
                .is_none()
        );
        let mut local = crate::catalog::Catalog::empty();
        local
            .merge_local_toml(
                r#"
[models."custom/model"]
context_window_tokens = 1000
max_output_tokens = 100
context_cost_tiers = [{above_input_tokens = 500, cost = {input = 2, output = 4}}]
[models."custom/model".cost]
input = 1
output = 3
"#,
            )
            .unwrap();
        let local = local.lookup_id("custom/model").unwrap();
        assert!(trusted_limit_source(local, "context_window_tokens"));
        assert!(trusted_limit_source(local, "max_output_tokens"));
        assert!(trusted_price_source(local, "cost.input"));
        assert!(trusted_price_source(local, "cost.output"));
        assert!(trusted_price_source(local, "context_cost_tiers"));
        assert!(trusted_applicable_cost(local, 1000).is_some());
    }

    #[test]
    fn pricing_shapes_reject_multiplicity_routing_and_native_media() {
        let base = serde_json::json!({
            "model": "x-ai/grok-4.7",
            "messages": [{"role": "user", "content": "hi"}],
            "transforms": []
        });
        assert!(supported_pricing_shape(
            "openrouter",
            "x-ai/grok-4.7",
            WireFormat::OpenAiChat,
            &base,
        ));
        for unsafe_body in [
            serde_json::json!({"model":"x-ai/grok-4.7","messages":[],"transforms":[],"n":2}),
            serde_json::json!({"model":"x-ai/grok-4.7","messages":[],"transforms":[],"route":"fallback"}),
        ] {
            assert!(!supported_pricing_shape(
                "openrouter",
                "x-ai/grok-4.7",
                WireFormat::OpenAiChat,
                &unsafe_body,
            ));
        }
        assert!(!supported_pricing_shape(
            "openrouter",
            "openrouter/free",
            WireFormat::OpenAiChat,
            &base,
        ));
        assert!(!supported_pricing_shape(
            "gemini",
            "gemini-3.8-flash",
            WireFormat::GoogleGenerateContent,
            &serde_json::json!({
                "contents": [{"parts": [{"inline_data": {"data": "AA=="}}]}]
            }),
        ));
    }

    #[test]
    fn known_costs_at_and_above_the_exact_redis_range_saturate() {
        let nanos = |value: f64| {
            observed_cost_nanos(&serde_json::json!({
                "cost_usd": value / NANOS_PER_USD
            }))
        };
        assert_eq!(
            nanos((MAX_EXACT_REDIS_NANOS - 1) as f64),
            Some(MAX_EXACT_REDIS_NANOS - 1)
        );
        assert_eq!(
            nanos(MAX_EXACT_REDIS_NANOS as f64),
            Some(MAX_EXACT_REDIS_NANOS)
        );
        assert_eq!(
            nanos((MAX_EXACT_REDIS_NANOS + 2) as f64),
            Some(MAX_EXACT_REDIS_NANOS)
        );
        assert_eq!(
            observed_cost_nanos(&serde_json::json!({"cost_usd": f64::MAX})),
            Some(MAX_EXACT_REDIS_NANOS)
        );
    }

    #[cfg(feature = "redis-coordination")]
    #[test]
    fn legacy_spend_conversion_rounds_up_saturates_and_rejects_invalid_values() {
        assert_eq!(legacy_spend_nanos(0.000_000_001), Some(1));
        assert_eq!(legacy_spend_nanos(0.000_000_001_1), Some(2));
        assert_eq!(legacy_spend_nanos(f64::MAX), Some(MAX_EXACT_REDIS_NANOS));
        assert_eq!(legacy_spend_nanos(-1.0), None);
        assert_eq!(legacy_spend_nanos(f64::NAN), None);
    }
}

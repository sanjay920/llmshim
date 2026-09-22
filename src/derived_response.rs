use crate::error::{Result, ShimError};
use crate::reasoning::{ReasoningOrigin, ReplayTarget};
use serde_json::Value;
use std::mem::size_of;

pub(crate) const DERIVED_RESPONSE_ERROR: &str = "upstream derived response metadata exceeds limit";
const DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_ENTRIES: usize = 4_096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DerivedFootprint {
    pub(crate) bytes: usize,
    pub(crate) entries: usize,
}

impl DerivedFootprint {
    pub(crate) fn record(bytes: usize) -> Option<Self> {
        Some(Self {
            bytes: bytes.checked_add(64)?,
            entries: 1,
        })
    }

    pub(crate) fn strings(bytes: usize) -> Self {
        Self { bytes, entries: 0 }
    }

    pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            bytes: self.bytes.checked_add(other.bytes)?,
            entries: self.entries.checked_add(other.entries)?,
        })
    }

    pub(crate) fn checked_multiply(self, multiplier: usize) -> Option<Self> {
        Some(Self {
            bytes: self.bytes.checked_mul(multiplier)?,
            entries: self.entries.checked_mul(multiplier)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureMode {
    Unary,
    Stream,
}

#[derive(Debug)]
pub(crate) struct DerivedResponseBudget {
    retained: DerivedFootprint,
    maximum: DerivedFootprint,
    failure_mode: FailureMode,
}

impl DerivedResponseBudget {
    pub(crate) fn unary() -> Self {
        Self::new(DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, FailureMode::Unary)
    }

    pub(crate) fn stream() -> Self {
        Self::new(DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, FailureMode::Stream)
    }

    #[cfg(test)]
    pub(crate) fn unary_with_limits(maximum_bytes: usize, maximum_entries: usize) -> Self {
        Self::new(maximum_bytes, maximum_entries, FailureMode::Unary)
    }

    #[cfg(test)]
    pub(crate) fn stream_with_limits(maximum_bytes: usize, maximum_entries: usize) -> Self {
        Self::new(maximum_bytes, maximum_entries, FailureMode::Stream)
    }

    fn new(maximum_bytes: usize, maximum_entries: usize, failure_mode: FailureMode) -> Self {
        Self {
            retained: DerivedFootprint::default(),
            maximum: DerivedFootprint {
                bytes: maximum_bytes,
                entries: maximum_entries,
            },
            failure_mode,
        }
    }

    pub(crate) fn reserve(&mut self, footprint: DerivedFootprint) -> Result<()> {
        let retained = self
            .retained
            .checked_add(footprint)
            .ok_or_else(|| self.error())?;
        if retained.bytes > self.maximum.bytes || retained.entries > self.maximum.entries {
            return Err(self.error());
        }
        self.retained = retained;
        Ok(())
    }

    pub(crate) fn reserve_value(&mut self, value: &Value) -> Result<()> {
        self.reserve(value_footprint(value).map_err(|_| self.error())?)
    }

    pub(crate) fn error(&self) -> ShimError {
        match self.failure_mode {
            FailureMode::Unary => ShimError::ProviderError {
                status: 502,
                body: DERIVED_RESPONSE_ERROR.into(),
                retry_after: None,
            },
            FailureMode::Stream => ShimError::Stream(DERIVED_RESPONSE_ERROR.into()),
        }
    }
}

pub(crate) fn origin_footprint(target: &ReplayTarget) -> Option<DerivedFootprint> {
    let string_bytes = target
        .provider
        .len()
        .checked_add(target.model.len())?
        .checked_add(target.account.as_deref().map(str::len).unwrap_or(0))?;
    DerivedFootprint::record(size_of::<ReasoningOrigin>())?
        .checked_add(DerivedFootprint::strings(string_bytes))
}

pub(crate) fn value_footprint(value: &Value) -> std::result::Result<DerivedFootprint, ()> {
    let footprint = crate::stream_retention::estimate_value(value).map_err(|_| ())?;
    Ok(DerivedFootprint {
        bytes: footprint.bytes,
        entries: footprint.entries,
    })
}

pub(crate) fn capture_unary(
    target: &ReplayTarget,
    native: &Value,
    response: &mut Value,
) -> Result<()> {
    let mut budget = DerivedResponseBudget::unary();
    crate::reasoning::capture_response_with_budget(target, native, response, &mut budget)?;
    crate::toolcall::capture_response_with_budget(target, native, response, &mut budget)
}

pub(crate) fn bind_unary_context(response: &mut Value, target: &ReplayTarget) -> Result<()> {
    bind_context(response, target, FailureMode::Unary)
}

#[cfg(test)]
pub(crate) fn bind_unary_context_with_limits(
    response: &mut Value,
    target: &ReplayTarget,
    maximum_bytes: usize,
    maximum_entries: usize,
) -> Result<()> {
    let budget = DerivedResponseBudget::unary_with_limits(maximum_bytes, maximum_entries);
    bind_context_with_budget(response, target, budget)
}

pub(crate) fn bind_stream_context(response: &mut Value, target: &ReplayTarget) -> Result<()> {
    bind_context(response, target, FailureMode::Stream)
}

fn bind_context(response: &mut Value, target: &ReplayTarget, mode: FailureMode) -> Result<()> {
    let budget = match mode {
        FailureMode::Unary => DerivedResponseBudget::unary(),
        FailureMode::Stream => DerivedResponseBudget::stream(),
    };
    bind_context_with_budget(response, target, budget)
}

fn bind_context_with_budget(
    response: &mut Value,
    target: &ReplayTarget,
    mut budget: DerivedResponseBudget,
) -> Result<()> {
    for choice in response["choices"].as_array().into_iter().flatten() {
        for field in ["message", "delta"] {
            let Some(message) = choice.get(field) else {
                continue;
            };
            if let Some(reasoning) = message.get("reasoning") {
                budget.reserve_value(reasoning)?;
                for block in reasoning.as_array().into_iter().flatten() {
                    reserve_origin_changes(&mut budget, block.get("origin"), target)?;
                }
            }
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                if let Some(signature) = call.get("thought_signature") {
                    budget.reserve_value(signature)?;
                    reserve_origin_changes(&mut budget, signature.get("origin"), target)?;
                }
                if let Some(bindings) = call.get("wire_ids") {
                    budget.reserve_value(bindings)?;
                    for binding in bindings.as_array().into_iter().flatten() {
                        reserve_changed_string(
                            &mut budget,
                            binding.get("provider"),
                            &target.provider,
                        )?;
                        budget.reserve(DerivedFootprint::strings(32))?;
                    }
                }
            }
        }
    }
    crate::reasoning::bind_response_context_unchecked(response, target);
    crate::toolcall::bind_response_context_unchecked(response, target);
    Ok(())
}

pub(crate) fn reserve_existing_metadata(
    response: &Value,
    budget: &mut DerivedResponseBudget,
) -> Result<()> {
    for choice in response["choices"].as_array().into_iter().flatten() {
        for field in ["message", "delta"] {
            let Some(message) = choice.get(field) else {
                continue;
            };
            if let Some(reasoning) = message.get("reasoning") {
                budget.reserve_value(reasoning)?;
            }
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                if let Some(signature) = call.get("thought_signature") {
                    budget.reserve_value(signature)?;
                }
                if let Some(bindings) = call.get("wire_ids") {
                    budget.reserve_value(bindings)?;
                }
            }
        }
    }
    Ok(())
}

fn reserve_origin_changes(
    budget: &mut DerivedResponseBudget,
    origin: Option<&Value>,
    target: &ReplayTarget,
) -> Result<()> {
    let Some(origin) = origin else {
        return Ok(());
    };
    reserve_changed_string(budget, origin.get("provider"), &target.provider)?;
    reserve_changed_string(budget, origin.get("model"), &target.model)?;
    budget.reserve(DerivedFootprint::strings(64))?;
    if let Some(account) = target.account.as_deref() {
        reserve_changed_string(budget, origin.get("account"), account)?;
        if origin.get("account").is_none() {
            budget.reserve(
                DerivedFootprint::record("account".len()).ok_or_else(|| budget.error())?,
            )?;
        }
    }
    Ok(())
}

fn reserve_changed_string(
    budget: &mut DerivedResponseBudget,
    current: Option<&Value>,
    replacement: &str,
) -> Result<()> {
    if current.and_then(Value::as_str) != Some(replacement) {
        budget.reserve(DerivedFootprint::strings(replacement.len()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reasoning::WireFormat;
    use serde_json::json;

    fn chat_target(model: &str) -> ReplayTarget {
        ReplayTarget::new("vllm", model, WireFormat::OpenAiChat)
    }

    fn chat_native(blocks: usize) -> Value {
        json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "ok",
                    "reasoning_details": (0..blocks).map(|index| json!({
                        "type": "reasoning.text",
                        "text": "r",
                        "id": format!("r{index}")
                    })).collect::<Vec<_>>()
                },
                "finish_reason": "stop"
            }]
        })
    }

    #[test]
    fn unary_budget_refuses_before_installing_partial_reasoning() {
        let target = chat_target(&"m".repeat(2_048));
        let native = chat_native(4);
        let mut response = native.clone();
        let mut budget = DerivedResponseBudget::unary_with_limits(5_000, 4_096);

        let error = crate::reasoning::capture_response_with_budget(
            &target,
            &native,
            &mut response,
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ShimError::ProviderError { status: 502, ref body, .. }
                if body == DERIVED_RESPONSE_ERROR
        ));
        assert!(response["choices"][0]["message"].get("reasoning").is_none());
        assert_eq!(
            response["choices"][0]["message"]["reasoning_details"],
            native["choices"][0]["message"]["reasoning_details"]
        );
    }

    #[test]
    fn exact_long_origin_is_preserved_below_the_default_budget() {
        let model = "opaque/".to_owned() + &"m".repeat(8_192);
        let target = chat_target(&model);
        let native = chat_native(1);
        let mut response = native.clone();

        crate::reasoning::capture_response(&target, &native, &mut response).unwrap();

        assert_eq!(
            response["choices"][0]["message"]["reasoning"][0]["origin"]["model"],
            model
        );
    }

    #[test]
    fn unknown_blocks_do_not_clone_an_origin_or_consume_the_tiny_budget() {
        let target = chat_target(&"m".repeat(8_192));
        let native = json!({"choices":[{"message":{"reasoning_details":[
            {"type":"future.unknown","payload":"x"},
            {"type":"another.unknown","payload":"y"}
        ]}}]});
        let mut response = native.clone();
        let mut budget = DerivedResponseBudget::unary_with_limits(1, 1);

        crate::reasoning::capture_response_with_budget(
            &target,
            &native,
            &mut response,
            &mut budget,
        )
        .unwrap();

        assert!(response["choices"][0]["message"].get("reasoning").is_none());
    }

    #[test]
    fn entry_budget_rejects_many_tiny_blocks() {
        let target = chat_target("model");
        let native = chat_native(3);
        let mut response = native.clone();
        let mut budget = DerivedResponseBudget::unary_with_limits(1024 * 1024, 8);

        assert!(crate::reasoning::capture_response_with_budget(
            &target,
            &native,
            &mut response,
            &mut budget,
        )
        .is_err());
        assert!(response["choices"][0]["message"].get("reasoning").is_none());
    }

    #[test]
    fn nested_google_signature_shares_the_unary_budget() {
        let target = chat_target(&"m".repeat(8_192));
        let native = json!({"choices":[{"message":{"tool_calls":[{
            "id":"native",
            "type":"function",
            "function":{"name":"read","arguments":"{}"},
            "extra_content":{"google":{"thought_signature":"opaque"}}
        }]}}]});
        let mut response = native.clone();
        let mut budget = DerivedResponseBudget::unary_with_limits(12_000, 4_096);

        let error = crate::toolcall::capture_response_with_budget(
            &target,
            &native,
            &mut response,
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ShimError::ProviderError { status: 502, .. }
        ));
        assert!(response["choices"][0]["message"]["tool_calls"][0]
            .get("thought_signature")
            .is_none());
    }

    #[test]
    fn multi_block_stream_frame_is_checked_before_eager_origin_clones() {
        let target = ReplayTarget::new("openai", &"m".repeat(2_048), WireFormat::OpenAiResponses);
        let native = json!({
            "type":"response.completed",
            "response":{"output":[
                {"type":"reasoning","id":"one","summary":[{"text":"a"}]},
                {"type":"reasoning","id":"two","summary":[{"text":"b"}]},
                {"type":"reasoning","id":"three","summary":[{"text":"c"}]}
            ]}
        });
        let mut budget = DerivedResponseBudget::stream_with_limits(6_000, 4_096);

        let error =
            crate::reasoning::capture_stream_with_budget(&target, &native, None, &mut budget)
                .unwrap_err();

        assert!(matches!(
            error,
            ShimError::Stream(ref message) if message == DERIVED_RESPONSE_ERROR
        ));
    }

    #[test]
    fn same_context_keeps_allocations_and_changed_context_is_preflighted() {
        let original = chat_target(&"same".repeat(512));
        let native = chat_native(2);
        let mut response = native.clone();
        crate::reasoning::capture_response(&original, &native, &mut response).unwrap();
        let before = response["choices"][0]["message"]["reasoning"][0]["origin"]["model"]
            .as_str()
            .unwrap()
            .as_ptr() as usize;

        bind_unary_context(&mut response, &original).unwrap();
        let after = response["choices"][0]["message"]["reasoning"][0]["origin"]["model"]
            .as_str()
            .unwrap()
            .as_ptr() as usize;
        assert_eq!(before, after);

        let mut changed = chat_target(&"changed".repeat(512));
        changed.provider = "bound-vllm".into();
        changed.account = Some("sha256:final-request-account".into());
        let old_model =
            response["choices"][0]["message"]["reasoning"][0]["origin"]["model"].clone();
        assert!(bind_unary_context_with_limits(&mut response, &changed, 8_000, 4_096).is_err());
        assert_eq!(
            response["choices"][0]["message"]["reasoning"][0]["origin"]["model"],
            old_model
        );

        bind_unary_context(&mut response, &changed).unwrap();
        assert!(response["choices"][0]["message"]["reasoning"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| {
                block["origin"]["model"] == changed.model
                    && block["origin"]["provider"] == changed.provider
                    && block["origin"]["account"] == changed.account.as_deref().unwrap()
            }));
    }

    #[test]
    fn chatgpt_collected_responses_use_the_same_reasoning_budget() {
        let native = |blocks: usize| {
            json!({
                "id":"response",
                "status":"completed",
                "output":(0..blocks).map(|index| json!({
                    "type":"reasoning",
                    "id":format!("reasoning-{index}"),
                    "summary":[{"type":"summary_text","text":"r"}]
                })).chain(std::iter::once(json!({
                    "type":"message",
                    "role":"assistant",
                    "content":[{"type":"output_text","text":"ok"}]
                }))).collect::<Vec<_>>(),
                "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
            })
        };
        let short_target = ReplayTarget::new("chatgpt", "gpt-6-astra", WireFormat::OpenAiResponses);
        let response = crate::providers::chatgpt::transform_collected_response(
            &short_target,
            "gpt-6-astra",
            native(1),
        )
        .unwrap();
        assert_eq!(
            response["choices"][0]["message"]["reasoning"][0]["origin"]["provider"],
            "chatgpt"
        );

        let long_model = "m".repeat(65_536);
        let long_target = ReplayTarget::new("chatgpt", &long_model, WireFormat::OpenAiResponses);
        let error = crate::providers::chatgpt::transform_collected_response(
            &long_target,
            &long_model,
            native(130),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ShimError::ProviderError { status: 502, ref body, .. }
                if body == DERIVED_RESPONSE_ERROR
        ));
    }
}

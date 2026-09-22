use crate::error::{Result, ShimError};
use serde_json::Value;
use std::mem::size_of;

use super::ReasoningOrigin;

pub(super) const INBOUND_REASONING_ERROR: &str =
    "request reasoning metadata exceeds derived size limit";
const DEFAULT_MAXIMUM_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAXIMUM_RECORDS: usize = 16_384;
const DERIVED_BLOCK_OVERHEAD_BYTES: usize = 512;
const SOURCE_VALUE_COPIES: usize = 2;
const DERIVED_ORIGIN_COPIES: usize = 4;
const DERIVED_PAYLOAD_COPIES: usize = 2;
const NORMALIZED_VALUE_COPIES: usize = 3;

#[derive(Debug)]
struct InboundReasoningBudget {
    used_bytes: usize,
    used_records: usize,
    maximum_bytes: usize,
    maximum_records: usize,
}

impl Default for InboundReasoningBudget {
    fn default() -> Self {
        Self::new(DEFAULT_MAXIMUM_BYTES, DEFAULT_MAXIMUM_RECORDS)
    }
}

impl InboundReasoningBudget {
    fn new(maximum_bytes: usize, maximum_records: usize) -> Self {
        Self {
            used_bytes: 0,
            used_records: 0,
            maximum_bytes,
            maximum_records,
        }
    }

    fn error(&self) -> ShimError {
        ShimError::ProviderError {
            status: 400,
            body: INBOUND_REASONING_ERROR.into(),
            retry_after: None,
        }
    }

    fn reserve(&mut self, bytes: usize, records: usize) -> Result<()> {
        let used_bytes = self
            .used_bytes
            .checked_add(bytes)
            .ok_or_else(|| self.error())?;
        let used_records = self
            .used_records
            .checked_add(records)
            .ok_or_else(|| self.error())?;
        if used_bytes > self.maximum_bytes || used_records > self.maximum_records {
            return Err(self.error());
        }
        self.used_bytes = used_bytes;
        self.used_records = used_records;
        Ok(())
    }

    fn reserve_value_copies(&mut self, value: &Value, copies: usize, records: usize) -> Result<()> {
        let footprint = crate::stream_retention::estimate_value(value).map_err(|_| self.error())?;
        let bytes = footprint
            .bytes
            .checked_mul(copies)
            .ok_or_else(|| self.error())?;
        self.reserve(bytes, records)
    }

    fn reserve_string_copies(
        &mut self,
        string_length: usize,
        copies: usize,
        records: usize,
    ) -> Result<()> {
        let bytes = size_of::<Value>()
            .checked_add(64)
            .and_then(|bytes| bytes.checked_add(string_length))
            .and_then(|bytes| bytes.checked_mul(copies))
            .ok_or_else(|| self.error())?;
        self.reserve(bytes, records)
    }
}

fn emitted_legacy_block(payload: &Value) -> bool {
    match payload["type"].as_str() {
        Some("reasoning" | "thinking" | "reasoning.text" | "reasoning.summary") => true,
        Some("redacted_thinking" | "reasoning.encrypted") => payload["data"].is_string(),
        _ => payload["thought"] == true,
    }
}

fn reserve_repeated_block(
    budget: &mut InboundReasoningBudget,
    origin: &Value,
    payload: &Value,
) -> Result<()> {
    budget.reserve_value_copies(origin, DERIVED_ORIGIN_COPIES, 0)?;
    budget.reserve_value_copies(payload, DERIVED_PAYLOAD_COPIES, 0)?;
    budget.reserve(DERIVED_BLOCK_OVERHEAD_BYTES, 0)
}

fn preflight_legacy_message(message: &Value, budget: &mut InboundReasoningBudget) -> Result<()> {
    let has_legacy_fields = [
        "reasoning_content",
        "reasoning_signature",
        "redacted_reasoning_content",
        "reasoning_details",
        "thinking_blocks",
    ]
    .into_iter()
    .any(|field| message.get(field).is_some())
        || message["reasoning"].is_string();
    if !has_legacy_fields {
        return Ok(());
    }

    let Some(origin) = message.get("reasoning_origin") else {
        return Ok(());
    };
    budget.reserve_value_copies(origin, SOURCE_VALUE_COPIES, 0)?;
    if serde_json::from_value::<ReasoningOrigin>(origin.clone()).is_err() {
        return Ok(());
    }

    for field in ["reasoning_details", "thinking_blocks"] {
        let mut emitted_any = false;
        for payload in message[field].as_array().into_iter().flatten() {
            budget.reserve_value_copies(payload, SOURCE_VALUE_COPIES, 1)?;
            if emitted_legacy_block(payload) {
                emitted_any = true;
                reserve_repeated_block(budget, origin, payload)?;
            }
        }
        if emitted_any {
            return Ok(());
        }
    }

    if let Some(text) = message["reasoning_content"]
        .as_str()
        .or_else(|| message["reasoning"].as_str())
    {
        budget.reserve_string_copies(text.len(), SOURCE_VALUE_COPIES, 1)?;
        if let Some(signature) = message["reasoning_signature"].as_str() {
            budget.reserve_string_copies(signature.len(), SOURCE_VALUE_COPIES, 0)?;
        }
        budget.reserve_value_copies(origin, DERIVED_ORIGIN_COPIES, 0)?;
        budget.reserve_string_copies(text.len(), DERIVED_PAYLOAD_COPIES, 0)?;
        budget.reserve(DERIVED_BLOCK_OVERHEAD_BYTES, 0)?;
    }
    if let Some(data) = message["redacted_reasoning_content"].as_str() {
        budget.reserve_string_copies(data.len(), SOURCE_VALUE_COPIES, 1)?;
        budget.reserve_value_copies(origin, DERIVED_ORIGIN_COPIES, 0)?;
        budget.reserve_string_copies(data.len(), DERIVED_PAYLOAD_COPIES, 0)?;
        budget.reserve(DERIVED_BLOCK_OVERHEAD_BYTES, 0)?;
    }
    Ok(())
}

fn preflight_tool_signatures(message: &Value, budget: &mut InboundReasoningBudget) -> Result<()> {
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        if let Some(signature) = call.get("thought_signature") {
            budget.reserve_value_copies(signature, NORMALIZED_VALUE_COPIES, 1)?;
        }
        if let Some(signature) = call.pointer("/extra_content/google/thought_signature") {
            budget.reserve_value_copies(signature, SOURCE_VALUE_COPIES, 1)?;
        }
    }
    Ok(())
}

fn preflight_message_with_budget(
    message: &Value,
    budget: &mut InboundReasoningBudget,
) -> Result<()> {
    if let Some(reasoning) = message.get("reasoning").filter(|value| value.is_array()) {
        budget.reserve_value_copies(
            reasoning,
            NORMALIZED_VALUE_COPIES,
            reasoning.as_array().map(Vec::len).unwrap_or(0),
        )?;
    } else {
        preflight_legacy_message(message, budget)?;
    }
    preflight_tool_signatures(message, budget)
}

pub(super) fn preflight_message(message: &Value) -> Result<()> {
    preflight_message_with_budget(message, &mut InboundReasoningBudget::default())
}

pub(super) fn preflight_request(request: &Value) -> Result<()> {
    let mut budget = InboundReasoningBudget::default();
    for message in request["messages"].as_array().into_iter().flatten() {
        preflight_message_with_budget(message, &mut budget)?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn preflight_request_with_limits(
    request: &Value,
    maximum_bytes: usize,
    maximum_records: usize,
) -> Result<()> {
    let mut budget = InboundReasoningBudget::new(maximum_bytes, maximum_records);
    for message in request["messages"].as_array().into_iter().flatten() {
        preflight_message_with_budget(message, &mut budget)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn origin(model_bytes: usize) -> Value {
        json!({
            "provider": "source",
            "model": "m".repeat(model_bytes),
            "family": "claude",
            "wire": "openai-chat",
            "received_at": "2026-09-22T00:00:00Z"
        })
    }

    fn legacy_message(model_bytes: usize, block_count: usize) -> Value {
        json!({
            "role": "assistant",
            "content": "answer",
            "reasoning_origin": origin(model_bytes),
            "reasoning_details": (0..block_count)
                .map(|index| json!({"type":"reasoning.text","text":"x","index":index}))
                .collect::<Vec<_>>()
        })
    }

    #[test]
    fn record_limit_counts_reasoning_records_instead_of_json_descendants() {
        let request = json!({"messages":[legacy_message(1, 2)]});
        assert!(preflight_request_with_limits(&request, usize::MAX, 2).is_ok());
        assert!(preflight_request_with_limits(&request, usize::MAX, 1).is_err());
    }

    #[test]
    fn byte_limit_is_aggregated_across_the_whole_request() {
        let message = legacy_message(8 * 1024, 1);
        let one = json!({"messages":[message.clone()]});
        let two = json!({"messages":[message.clone(),message]});
        assert!(preflight_request_with_limits(&one, 80 * 1024, usize::MAX).is_ok());
        assert!(preflight_request_with_limits(&two, 80 * 1024, usize::MAX).is_err());
    }

    #[test]
    fn inspected_unknown_rows_and_later_precedence_rows_share_the_entry_limit() {
        let request = json!({"messages":[{
            "role":"assistant",
            "reasoning_origin":origin(1),
            "reasoning_details":[{"type":"unknown"}],
            "thinking_blocks":[{"type":"thinking","thinking":"kept","signature":"sig"}]
        }]});
        assert!(preflight_request_with_limits(&request, usize::MAX, 2).is_ok());
        assert!(preflight_request_with_limits(&request, usize::MAX, 1).is_err());
    }

    #[test]
    fn default_budget_rejects_small_shared_origin_amplification_input() {
        let request = json!({"messages":[legacy_message(64 * 1024, 64)]});
        assert!(request.to_string().len() < 80 * 1024);
        assert!(matches!(
            preflight_request(&request),
            Err(ShimError::ProviderError { status: 400, body, .. })
                if body == INBOUND_REASONING_ERROR
        ));
    }

    #[test]
    fn normalized_blocks_and_tool_signatures_use_record_units() {
        let normalized = json!({"messages":[{
            "role":"assistant",
            "reasoning":[{"kind":"text"},{"kind":"text"}],
            "tool_calls":[{"thought_signature":{"data":"a"}}]
        }]});
        assert!(preflight_request_with_limits(&normalized, usize::MAX, 3).is_ok());
        assert!(preflight_request_with_limits(&normalized, usize::MAX, 2).is_err());
    }

    #[test]
    fn missing_or_malformed_origins_keep_existing_drop_before_children_behavior() {
        let details: Vec<Value> = (0..8)
            .map(|_| json!({"type":"reasoning.text","text":"ignored"}))
            .collect();
        for message in [
            json!({"role":"assistant","reasoning_details":details.clone()}),
            json!({
                "role":"assistant",
                "reasoning_origin":{"provider":7},
                "reasoning_details":details
            }),
        ] {
            let request = json!({"messages":[message]});
            assert!(preflight_request_with_limits(&request, usize::MAX, 0).is_ok());
        }
    }
}

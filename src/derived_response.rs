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
    pub(crate) fn record(bytes: usize) -> Self {
        Self {
            bytes: bytes.saturating_add(64),
            entries: 1,
        }
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
    DerivedFootprint::record(size_of::<ReasoningOrigin>())
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

pub(crate) fn bind_stream_context(response: &mut Value, target: &ReplayTarget) -> Result<()> {
    bind_context(response, target, FailureMode::Stream)
}

fn bind_context(response: &mut Value, target: &ReplayTarget, mode: FailureMode) -> Result<()> {
    let mut budget = match mode {
        FailureMode::Unary => DerivedResponseBudget::unary(),
        FailureMode::Stream => DerivedResponseBudget::stream(),
    };
    for choice in response["choices"].as_array().into_iter().flatten() {
        for field in ["message", "delta"] {
            let Some(message) = choice.get(field) else {
                continue;
            };
            if let Some(reasoning) = message.get("reasoning") {
                reserve_projected_value(&mut budget, reasoning, target)?;
            }
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                if let Some(signature) = call.get("thought_signature") {
                    reserve_projected_value(&mut budget, signature, target)?;
                }
                if let Some(bindings) = call.get("wire_ids") {
                    reserve_projected_value(&mut budget, bindings, target)?;
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

fn reserve_projected_value(
    budget: &mut DerivedResponseBudget,
    value: &Value,
    target: &ReplayTarget,
) -> Result<()> {
    budget.reserve_value(value)?;
    let mut pending = vec![value];
    while let Some(current) = pending.pop() {
        match current {
            Value::Array(values) => pending.extend(values),
            Value::Object(values) => {
                if values.contains_key("model") && values.contains_key("provider") {
                    reserve_changed_string(budget, values.get("provider"), &target.provider)?;
                    reserve_changed_string(budget, values.get("model"), &target.model)?;
                    if let Some(account) = target.account.as_deref() {
                        reserve_changed_string(budget, values.get("account"), account)?;
                    }
                } else if values.contains_key("wire") && values.contains_key("scope") {
                    reserve_changed_string(budget, values.get("provider"), &target.provider)?;
                }
                pending.extend(values.values());
            }
            _ => {}
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

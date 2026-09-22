use crate::error::{Result, ShimError};
use serde_json::Value;
use std::{
    mem::size_of,
    sync::{Arc, Mutex},
};

pub(crate) const RETENTION_ERROR: &str = "upstream stream retained state exceeds limit";
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 65_536;
const MAP_ENTRY_OVERHEAD: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamRetentionLimits {
    pub normalizer_bytes: usize,
    pub normalizer_entries: usize,
    pub native_usage_bytes: usize,
    pub native_usage_entries: usize,
}

impl Default for StreamRetentionLimits {
    fn default() -> Self {
        Self {
            normalizer_bytes: 16 * 1024 * 1024,
            normalizer_entries: 4_096,
            native_usage_bytes: 4 * 1024 * 1024,
            native_usage_entries: 4_096,
        }
    }
}

impl StreamRetentionLimits {
    pub fn new(
        normalizer_bytes: usize,
        normalizer_entries: usize,
        native_usage_bytes: usize,
        native_usage_entries: usize,
    ) -> Result<Self> {
        if [normalizer_bytes, native_usage_bytes]
            .into_iter()
            .any(|limit| limit == 0 || limit > MAX_BYTES)
            || [normalizer_entries, native_usage_entries]
                .into_iter()
                .any(|limit| limit == 0 || limit > MAX_ENTRIES)
        {
            return Err(retention_error());
        }
        Ok(Self {
            normalizer_bytes,
            normalizer_entries,
            native_usage_bytes,
            native_usage_entries,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RetainedFootprint {
    pub(crate) bytes: usize,
    pub(crate) entries: usize,
}

impl RetainedFootprint {
    pub(crate) fn record(bytes: usize) -> Self {
        Self {
            bytes: bytes.saturating_add(MAP_ENTRY_OVERHEAD),
            entries: 1,
        }
    }

    pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            bytes: self.bytes.checked_add(other.bytes)?,
            entries: self.entries.checked_add(other.entries)?,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RetainedBudget {
    state: Arc<Mutex<BudgetState>>,
}

#[derive(Debug)]
struct BudgetState {
    retained: RetainedFootprint,
    max_bytes: usize,
    max_entries: usize,
}

impl RetainedBudget {
    pub(crate) fn new(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(BudgetState {
                retained: RetainedFootprint::default(),
                max_bytes,
                max_entries,
            })),
        }
    }

    pub(crate) fn replace(
        &self,
        previous: RetainedFootprint,
        replacement: RetainedFootprint,
    ) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| retention_error())?;
        let bytes = state
            .retained
            .bytes
            .checked_sub(previous.bytes)
            .and_then(|value| value.checked_add(replacement.bytes))
            .ok_or_else(retention_error)?;
        let entries = state
            .retained
            .entries
            .checked_sub(previous.entries)
            .and_then(|value| value.checked_add(replacement.entries))
            .ok_or_else(retention_error)?;
        if bytes > state.max_bytes || entries > state.max_entries {
            return Err(retention_error());
        }
        state.retained = RetainedFootprint { bytes, entries };
        Ok(())
    }

    pub(crate) fn reserve(&self, footprint: RetainedFootprint) -> Result<()> {
        self.replace(RetainedFootprint::default(), footprint)
    }

    pub(crate) fn release(&self, footprint: RetainedFootprint) {
        if let Ok(mut state) = self.state.lock() {
            state.retained.bytes = state.retained.bytes.saturating_sub(footprint.bytes);
            state.retained.entries = state.retained.entries.saturating_sub(footprint.entries);
        }
    }

    pub(crate) fn reset(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.retained = RetainedFootprint::default();
        }
    }

    #[cfg(test)]
    pub(crate) fn retained(&self) -> RetainedFootprint {
        self.state
            .lock()
            .map(|state| state.retained)
            .unwrap_or_default()
    }
}

pub(crate) fn estimate_value(value: &Value) -> Result<RetainedFootprint> {
    let mut footprint = RetainedFootprint::default();
    let mut pending = vec![value];
    while let Some(current) = pending.pop() {
        footprint = footprint
            .checked_add(RetainedFootprint::record(size_of::<Value>()))
            .ok_or_else(retention_error)?;
        if footprint.bytes > MAX_BYTES || footprint.entries > MAX_ENTRIES {
            return Err(retention_error());
        }
        match current {
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
            Value::String(text) => {
                footprint.bytes = footprint
                    .bytes
                    .checked_add(text.capacity())
                    .ok_or_else(retention_error)?;
            }
            Value::Array(values) => {
                if values.len() > MAX_ENTRIES.saturating_sub(pending.len()) {
                    return Err(retention_error());
                }
                footprint.bytes = footprint
                    .bytes
                    .checked_add(size_of::<Vec<Value>>())
                    .and_then(|bytes| {
                        values
                            .capacity()
                            .checked_mul(size_of::<Value>())
                            .and_then(|array_bytes| bytes.checked_add(array_bytes))
                    })
                    .ok_or_else(retention_error)?;
                pending.extend(values);
            }
            Value::Object(values) => {
                if values.len() > MAX_ENTRIES.saturating_sub(pending.len()) {
                    return Err(retention_error());
                }
                footprint.bytes = footprint
                    .bytes
                    .checked_add(
                        values
                            .len()
                            .checked_mul(MAP_ENTRY_OVERHEAD)
                            .ok_or_else(retention_error)?,
                    )
                    .ok_or_else(retention_error)?;
                for (key, child) in values {
                    footprint.bytes = footprint
                        .bytes
                        .checked_add(key.capacity())
                        .ok_or_else(retention_error)?;
                    pending.push(child);
                }
            }
        }
    }
    Ok(footprint)
}

pub(crate) fn retention_error() -> ShimError {
    ShimError::Stream(RETENTION_ERROR.into())
}

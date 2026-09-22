use crate::error::{Result, ShimError};
use serde_json::Value;

const MAX_MEASURE_NODES: usize = 524_288;
const MAX_MEASURE_LITERAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_MEASURE_OWNED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct BudgetLimits {
    pub schema_copies: usize,
    pub retained_nodes: usize,
    pub retained_literal_bytes: usize,
    pub retained_owned_bytes: usize,
    pub work_passes: usize,
    pub work_nodes: usize,
    pub work_literal_bytes: usize,
    pub validators: usize,
    pub prompt_bytes: usize,
}

impl Default for BudgetLimits {
    fn default() -> Self {
        Self {
            schema_copies: 1024,
            retained_nodes: 262_144,
            retained_literal_bytes: 32 * 1024 * 1024,
            retained_owned_bytes: 64 * 1024 * 1024,
            work_passes: 512,
            work_nodes: 524_288,
            work_literal_bytes: 64 * 1024 * 1024,
            validators: 128,
            prompt_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Footprint {
    pub nodes: usize,
    pub literal_bytes: usize,
    pub owned_bytes: usize,
}

#[derive(Debug, Default)]
struct Usage {
    schema_copies: usize,
    retained_nodes: usize,
    retained_literal_bytes: usize,
    retained_owned_bytes: usize,
    work_passes: usize,
    work_nodes: usize,
    work_literal_bytes: usize,
    validators: usize,
    prompt_bytes: usize,
}

#[derive(Debug)]
pub(crate) struct RequestBudget {
    limits: BudgetLimits,
    used: Usage,
}

impl RequestBudget {
    pub(crate) fn new() -> Self {
        Self::with_limits(BudgetLimits::default())
    }

    pub(crate) fn with_limits(limits: BudgetLimits) -> Self {
        Self {
            limits,
            used: Usage::default(),
        }
    }

    pub(crate) fn reserve_retained(&mut self, value: &Value) -> Result<()> {
        let footprint = measure(value)?;
        self.reserve_retained_footprint(footprint)
    }

    pub(super) fn reserve_retained_footprint(&mut self, footprint: Footprint) -> Result<()> {
        reserve(&mut self.used.schema_copies, 1, self.limits.schema_copies)?;
        reserve(
            &mut self.used.retained_nodes,
            footprint.nodes,
            self.limits.retained_nodes,
        )?;
        reserve(
            &mut self.used.retained_literal_bytes,
            footprint.literal_bytes,
            self.limits.retained_literal_bytes,
        )?;
        reserve(
            &mut self.used.retained_owned_bytes,
            footprint.owned_bytes,
            self.limits.retained_owned_bytes,
        )
    }

    pub(crate) fn reserve_work(&mut self, value: &Value) -> Result<()> {
        let footprint = measure(value)?;
        reserve(&mut self.used.work_passes, 1, self.limits.work_passes)?;
        reserve(
            &mut self.used.work_nodes,
            footprint.nodes,
            self.limits.work_nodes,
        )?;
        reserve(
            &mut self.used.work_literal_bytes,
            footprint.literal_bytes,
            self.limits.work_literal_bytes,
        )
    }

    pub(crate) fn reserve_validator(&mut self, value: &Value) -> Result<()> {
        reserve(&mut self.used.validators, 1, self.limits.validators)?;
        self.reserve_work(value)?;
        self.reserve_retained(value)
    }

    pub(crate) fn reserve_prompt_schema(&mut self, value: &Value) -> Result<()> {
        let footprint = measure(value)?;
        reserve(
            &mut self.used.prompt_bytes,
            footprint.owned_bytes.saturating_add(128),
            self.limits.prompt_bytes,
        )
    }

    pub(crate) fn reserve_request_schemas(&mut self, request: &Value) -> Result<()> {
        reserve_schema_locations(self, request)?;
        if let Some(object) = request.as_object() {
            for (key, extension) in object.iter().filter(|(key, _)| key.starts_with("x-")) {
                let _ = key;
                reserve_schema_locations(self, extension)?;
            }
        }
        Ok(())
    }
}

fn reserve_schema_locations(budget: &mut RequestBudget, container: &Value) -> Result<()> {
    if let Some(tools) = container.get("tools").and_then(Value::as_array) {
        for tool in tools {
            reserve_tool_schemas(budget, tool)?;
        }
    }
    for pointer in [
        "/response_format/json_schema/schema",
        "/text/format/schema",
        "/output_config/format/schema",
        "/generationConfig/responseSchema",
        "/generationConfig/responseJsonSchema",
        "/responseSchema",
        "/response_json_schema",
    ] {
        if let Some(schema) = container.pointer(pointer) {
            budget.reserve_retained(schema)?;
        }
    }
    Ok(())
}

fn reserve_tool_schemas(budget: &mut RequestBudget, tool: &Value) -> Result<()> {
    for pointer in [
        "/inputSchema",
        "/input_schema",
        "/parameters",
        "/function/parameters",
    ] {
        if let Some(schema) = tool.pointer(pointer) {
            budget.reserve_retained(schema)?;
        }
    }
    if let Some(declarations) = tool.get("functionDeclarations").and_then(Value::as_array) {
        for declaration in declarations {
            if let Some(schema) = declaration.get("parameters") {
                budget.reserve_retained(schema)?;
            }
        }
    }
    Ok(())
}

pub(super) fn measure(value: &Value) -> Result<Footprint> {
    let mut pending = vec![value];
    let mut footprint = Footprint::default();
    while let Some(value) = pending.pop() {
        footprint.nodes = footprint.nodes.checked_add(1).ok_or_else(exhausted)?;
        footprint.owned_bytes = footprint
            .owned_bytes
            .checked_add(64)
            .ok_or_else(exhausted)?;
        match value {
            Value::Object(object) => {
                if footprint
                    .nodes
                    .saturating_add(pending.len())
                    .saturating_add(object.len())
                    > MAX_MEASURE_NODES
                {
                    return Err(exhausted());
                }
                footprint.owned_bytes = footprint
                    .owned_bytes
                    .checked_add(576)
                    .ok_or_else(exhausted)?;
                for (key, value) in object {
                    footprint.literal_bytes = footprint
                        .literal_bytes
                        .checked_add(key.len())
                        .ok_or_else(exhausted)?;
                    footprint.owned_bytes = footprint
                        .owned_bytes
                        .checked_add(key.len())
                        .and_then(|bytes| bytes.checked_add(32))
                        .ok_or_else(exhausted)?;
                    if footprint.literal_bytes > MAX_MEASURE_LITERAL_BYTES
                        || footprint.owned_bytes > MAX_MEASURE_OWNED_BYTES
                    {
                        return Err(exhausted());
                    }
                    pending.push(value);
                }
            }
            Value::Array(array) => {
                if footprint
                    .nodes
                    .saturating_add(pending.len())
                    .saturating_add(array.len())
                    > MAX_MEASURE_NODES
                {
                    return Err(exhausted());
                }
                pending.extend(array);
            }
            Value::String(text) => {
                footprint.literal_bytes = footprint
                    .literal_bytes
                    .checked_add(text.len())
                    .ok_or_else(exhausted)?;
                footprint.owned_bytes = footprint
                    .owned_bytes
                    .checked_add(text.len())
                    .and_then(|bytes| bytes.checked_add(32))
                    .ok_or_else(exhausted)?;
            }
            _ => {}
        }
        if footprint.nodes.saturating_add(pending.len()) > MAX_MEASURE_NODES
            || footprint.literal_bytes > MAX_MEASURE_LITERAL_BYTES
            || footprint.owned_bytes > MAX_MEASURE_OWNED_BYTES
        {
            return Err(exhausted());
        }
    }
    Ok(footprint)
}

fn reserve(used: &mut usize, amount: usize, limit: usize) -> Result<()> {
    let next = used.checked_add(amount).ok_or_else(exhausted)?;
    if next > limit {
        return Err(exhausted());
    }
    *used = next;
    Ok(())
}

pub(crate) fn exhausted() -> ShimError {
    ShimError::ProviderError {
        status: 400,
        body: "request schema budget exceeded".into(),
        retry_after: None,
    }
}

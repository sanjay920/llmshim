//! One schema walker with per-transport policy. External references are never
//! fetched; unresolved/cyclic schemas fall back per tool rather than per request.
mod budget;
mod memo;
pub mod validate;
mod walk;
use crate::error::Result;
pub(crate) use budget::{BudgetLimits, RequestBudget};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
    Google,
    CloudCodeAssist,
    Mcp,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Nullable {
    Preserve,
    Marker,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Options {
    pub strict: bool,
    pub nullable: Nullable,
    pub rewrite_one_of: bool,
    pub strip_lookarounds: bool,
    pub target: Target,
    pub bypass: bool,
    pub max_depth: usize,
    pub max_nodes: usize,
    pub max_literal_bytes: usize,
}
impl Options {
    pub fn for_target(target: Target) -> Self {
        Self {
            strict: false,
            nullable: match target {
                Target::Google => Nullable::Marker,
                Target::CloudCodeAssist => Nullable::Remove,
                _ => Nullable::Preserve,
            },
            rewrite_one_of: matches!(
                target,
                Target::OpenAiResponses | Target::Google | Target::CloudCodeAssist
            ),
            strip_lookarounds: target == Target::OpenAiResponses,
            target,
            bypass: false,
            max_depth: 96,
            max_nodes: 32_768,
            max_literal_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Normalization {
    pub changed: bool,
    pub used_fallback: bool,
    pub strict: bool,
    pub bypassed: bool,
}
fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

pub fn normalize_for(target: Target, schema: &mut Value) -> Normalization {
    normalize_with(&Options::for_target(target), schema)
}

pub fn normalize_with(options: &Options, schema: &mut Value) -> Normalization {
    normalize_with_optional_budget(options, schema, None)
        .expect("unbudgeted normalization cannot exhaust")
}

pub(crate) fn normalize_with_budget(
    options: &Options,
    schema: &mut Value,
    budget: &mut RequestBudget,
) -> Result<Normalization> {
    normalize_with_optional_budget(options, schema, Some(budget))
}

fn normalize_with_optional_budget(
    options: &Options,
    schema: &mut Value,
    mut budget: Option<&mut RequestBudget>,
) -> Result<Normalization> {
    if options.bypass || flag("LLMSHIM_NO_SCHEMA_NORMALIZATION") {
        return Ok(Normalization {
            bypassed: true,
            ..Normalization::default()
        });
    }
    let mut options = options.clone();
    if flag("LLMSHIM_NO_STRICT") {
        options.strict = false;
    }
    let key = (!flag("LLMSHIM_NO_SCHEMA_CACHE"))
        .then(|| memo::CACHE.key(&options, schema))
        .flatten();
    if let Some(key) = &key {
        if let Some(cached) = memo::CACHE.get(key, &options, schema) {
            if !cached.unchanged {
                if let Some(budget) = budget.as_deref_mut() {
                    budget.reserve_retained_footprint(cached.footprint)?;
                }
                *schema = cached.value.clone();
            }
            return Ok(cached.report);
        }
    }
    if let Some(budget) = budget.as_deref_mut() {
        budget.reserve_work(schema)?;
    }
    let input = std::mem::take(schema);
    let (value, report) = normalized_value(&options, &input);
    if let Some(budget) = budget {
        if let Err(error) = budget
            .reserve_work(&value)
            .and_then(|_| budget.reserve_retained(&value))
        {
            *schema = input;
            return Err(error);
        }
    }
    if let Some(key) = key {
        memo::CACHE.insert(key, options, input, &value, report);
    }
    *schema = value;
    Ok(report)
}

#[cfg(test)]
fn normalize_uncached(options: &Options, schema: &mut Value) -> Normalization {
    let original = std::mem::take(schema);
    let (value, report) = normalized_value(options, &original);
    *schema = value;
    report
}

fn normalized_value(options: &Options, original: &Value) -> (Value, Normalization) {
    match walk::normalize(original, options) {
        Ok(value) if validate::compile(&value).is_ok() => {
            let changed = value != *original;
            (
                value,
                Normalization {
                    changed,
                    strict: options.strict,
                    ..Normalization::default()
                },
            )
        }
        Ok(_) | Err(()) => {
            let value = json!({"type":"object","properties":{}});
            let changed = value != *original;
            (
                value,
                Normalization {
                    changed,
                    used_fallback: true,
                    ..Normalization::default()
                },
            )
        }
    }
}

/// Normalize an MCP tool as it enters a tool registry, before any model request.
pub fn normalize_mcp_tool(tool: &mut Value) -> Normalization {
    let Some(schema) = tool.get_mut("inputSchema") else {
        return Normalization::default();
    };
    let mut report = normalize_for(Target::Mcp, schema);
    if !report.bypassed && schema["type"] != "object" {
        *schema = json!({"type":"object","properties":{}});
        report.changed = true;
        report.used_fallback = true;
        report.strict = false;
    }
    report
}

/// Accept raw MCP tool definitions as well as canonical nested function tools.
pub(crate) fn prepare_request(request: &Value, budget: &mut RequestBudget) -> Result<Value> {
    budget.reserve_request_schemas(request)?;
    let mut request = request.clone();
    if let Some(tools) = request.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if tool.get("inputSchema").is_none() {
                continue;
            }
            normalize_mcp_tool_with_budget(tool, budget)?;
            budget.reserve_retained(&tool["inputSchema"])?;
            let mut canonical = json!({"type":"function","function":{"name":tool["name"],"parameters":tool["inputSchema"]}});
            if let Some(description) = tool.get("description").or_else(|| tool.get("title")) {
                canonical["function"]["description"] = description.clone();
            }
            if let Some(cache) = tool.get("cache_control") {
                canonical["cache_control"] = cache.clone();
            }
            *tool = canonical;
        }
    }
    Ok(request)
}

fn normalize_mcp_tool_with_budget(
    tool: &mut Value,
    budget: &mut RequestBudget,
) -> Result<Normalization> {
    let Some(schema) = tool.get_mut("inputSchema") else {
        return Ok(Normalization::default());
    };
    let mut report = normalize_with_budget(&Options::for_target(Target::Mcp), schema, budget)?;
    if !report.bypassed && schema["type"] != "object" {
        let fallback = json!({"type":"object","properties":{}});
        budget.reserve_retained(&fallback)?;
        *schema = fallback;
        report.changed = true;
        report.used_fallback = true;
        report.strict = false;
    }
    Ok(report)
}

fn tool_schema(
    target: Target,
    tool: &mut Value,
    key: &str,
    budget: &mut RequestBudget,
) -> Result<()> {
    let requested = tool["strict"].as_bool().unwrap_or(false);
    let Some(schema) = tool.get_mut(key) else {
        return Ok(());
    };
    budget.reserve_retained(schema)?;
    let mut options = Options::for_target(target);
    options.strict = requested;
    let mut report = normalize_with_budget(&options, schema, budget)?;
    if !report.bypassed
        && (requested || matches!(target, Target::Google | Target::Anthropic))
        && schema["type"] != "object"
    {
        let fallback = json!({"type":"object","properties":{}});
        budget.reserve_retained(&fallback)?;
        *schema = fallback;
        report.used_fallback = true;
        report.strict = false;
    }
    if requested
        || (report.used_fallback && matches!(target, Target::OpenAiChat | Target::OpenAiResponses))
    {
        tool["strict"] = json!(report.strict);
    }
    Ok(())
}

/// A provider-ready output schema plus the reversible non-object wrapper.
#[derive(Debug, Clone)]
pub struct OutputSchema {
    pub schema: Value,
    pub wrapped: bool,
    pub normalization: Normalization,
}
impl OutputSchema {
    pub fn unwrap(&self, value: &Value) -> Option<Value> {
        if self.wrapped {
            value.get("response").cloned()
        } else {
            Some(value.clone())
        }
    }
}

/// Resolve the original document before wrapping so local references keep
/// their original root. Shims can validate the unwrapped value independently.
pub fn normalize_output_for(target: Target, original: &Value, strict: bool) -> OutputSchema {
    normalize_output_for_optional_budget(target, original, strict, None)
        .expect("unbudgeted normalization cannot exhaust")
}

pub(crate) fn normalize_output_for_budget(
    target: Target,
    original: &Value,
    strict: bool,
    budget: &mut RequestBudget,
) -> Result<OutputSchema> {
    normalize_output_for_optional_budget(target, original, strict, Some(budget))
}

fn normalize_output_for_optional_budget(
    target: Target,
    original: &Value,
    strict: bool,
    mut budget: Option<&mut RequestBudget>,
) -> Result<OutputSchema> {
    if let Some(budget) = budget.as_deref_mut() {
        budget.reserve_retained(original)?;
    }
    let mut schema = original.clone();
    let base = if let Some(budget) = budget.as_deref_mut() {
        normalize_with_budget(&Options::for_target(Target::Mcp), &mut schema, budget)?
    } else {
        normalize_for(Target::Mcp, &mut schema)
    };
    let wrapped = schema["type"] != "object";
    if wrapped {
        schema = json!({"type":"object","properties":{"response":schema},"required":["response"]});
        if let Some(budget) = budget.as_deref_mut() {
            budget.reserve_retained(&schema)?;
        }
    }
    let mut options = Options::for_target(target);
    options.strict = strict;
    let mut normalization = if let Some(budget) = budget {
        normalize_with_budget(&options, &mut schema, budget)?
    } else {
        normalize_with(&options, &mut schema)
    };
    normalization.used_fallback |= base.used_fallback;
    normalization.changed |= base.changed || wrapped;
    Ok(OutputSchema {
        schema,
        wrapped,
        normalization,
    })
}

/// Normalize native tool schemas after extension overrides. No user argument,
/// enum/default literal, or conversational content is recursively rewritten.
pub(crate) fn normalize_native_tools(
    target: Target,
    body: &mut Value,
    budget: &mut RequestBudget,
) -> Result<()> {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            match target {
                Target::Google => {
                    if let Some(declarations) = tool
                        .get_mut("functionDeclarations")
                        .and_then(Value::as_array_mut)
                    {
                        for declaration in declarations {
                            tool_schema(target, declaration, "parameters", budget)?;
                            declaration.as_object_mut().unwrap().remove("strict");
                        }
                    }
                }
                Target::Anthropic => tool_schema(target, tool, "input_schema", budget)?,
                _ => {
                    if tool["function"].is_object() {
                        tool_schema(target, &mut tool["function"], "parameters", budget)?;
                    } else if tool["type"] == "function" {
                        tool_schema(target, tool, "parameters", budget)?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod request_budget_tests {
    use super::*;

    fn limits(schema_copies: usize) -> BudgetLimits {
        BudgetLimits {
            schema_copies,
            retained_nodes: 10_000,
            retained_literal_bytes: 1_000_000,
            retained_owned_bytes: 1_000_000,
            work_passes: 100,
            work_nodes: 10_000,
            work_literal_bytes: 1_000_000,
            validators: 100,
            prompt_bytes: 1_000_000,
        }
    }

    fn assert_exhausted(error: crate::error::ShimError) {
        match error {
            crate::error::ShimError::ProviderError { status, body, .. } => {
                assert_eq!(status, 400);
                assert_eq!(body, "request schema budget exceeded");
            }
            error => panic!("unexpected error: {error:?}"),
        }
    }

    #[test]
    fn cached_normalized_values_reserve_each_returned_clone_before_assignment() {
        let input = json!({"type":"object","properties":{"request_budget_cache_fixture":{"type":"string","default":"value"}}});
        let mut options = Options::for_target(Target::OpenAiResponses);
        options.strict = true;
        let mut warmed = input.clone();
        assert!(normalize_with(&options, &mut warmed).changed);

        let mut budget = RequestBudget::with_limits(limits(2));
        let mut first = input.clone();
        normalize_with_budget(&options, &mut first, &mut budget).unwrap();
        let mut second = input.clone();
        normalize_with_budget(&options, &mut second, &mut budget).unwrap();
        assert_eq!(first, warmed);
        assert_eq!(second, warmed);

        let mut rejected = input.clone();
        let error = normalize_with_budget(&options, &mut rejected, &mut budget).unwrap_err();
        assert_exhausted(error);
        assert_eq!(rejected, input);
    }

    #[test]
    fn native_shapes_share_one_aggregate_budget_and_keep_per_schema_fallback() {
        let valid = json!({"type":"object","properties":{"value":{"type":"string"}}});
        let invalid = json!({"$ref":"#/missing"});
        let cases = [
            (
                Target::OpenAiChat,
                json!({"tools":[
                    {"type":"function","function":{"name":"a","strict":true,"parameters":invalid}},
                    {"type":"function","function":{"name":"b","strict":true,"parameters":valid}}
                ]}),
            ),
            (
                Target::OpenAiResponses,
                json!({"tools":[
                    {"type":"function","name":"a","strict":true,"parameters":invalid},
                    {"type":"function","name":"b","strict":true,"parameters":valid}
                ]}),
            ),
            (
                Target::Anthropic,
                json!({"tools":[
                    {"name":"a","input_schema":invalid},
                    {"name":"b","input_schema":valid}
                ]}),
            ),
            (
                Target::Google,
                json!({"tools":[{"functionDeclarations":[
                    {"name":"a","parameters":invalid},
                    {"name":"b","parameters":valid}
                ]}]}),
            ),
        ];

        for (target, mut body) in cases {
            let mut budget = RequestBudget::with_limits(limits(8));
            normalize_native_tools(target, &mut body, &mut budget).unwrap();
            let schemas: Vec<&Value> = match target {
                Target::OpenAiChat => body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| &tool["function"]["parameters"])
                    .collect(),
                Target::OpenAiResponses => body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| &tool["parameters"])
                    .collect(),
                Target::Anthropic => body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| &tool["input_schema"])
                    .collect(),
                Target::Google => body["tools"][0]["functionDeclarations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| &tool["parameters"])
                    .collect(),
                _ => unreachable!(),
            };
            assert_eq!(schemas[0], &json!({"type":"object","properties":{}}));
            assert_eq!(schemas[1]["type"], "object");
            assert!(schemas[1]["properties"].is_object());
            match target {
                Target::OpenAiChat => assert_eq!(body["tools"][0]["function"]["strict"], false),
                Target::OpenAiResponses => assert_eq!(body["tools"][0]["strict"], false),
                _ => {}
            }
        }

        let repeated_schema = json!({"type":"object","additional_properties":false});
        let mut warmed = repeated_schema.clone();
        assert!(normalize_for(Target::OpenAiResponses, &mut warmed).changed);
        let mut repeated = json!({"tools":[
            {"type":"function","name":"a","parameters":repeated_schema},
            {"type":"function","name":"b","parameters":repeated_schema}
        ]});
        let mut budget = RequestBudget::with_limits(limits(3));
        assert_exhausted(
            normalize_native_tools(Target::OpenAiResponses, &mut repeated, &mut budget)
                .unwrap_err(),
        );
    }

    #[test]
    fn bypass_and_output_schemas_cannot_escape_the_request_budget() {
        let schema = json!({"type":"object","properties":{"value":{"type":"string"}}});
        let mut bypassed = json!({"tools":[
            {"type":"function","name":"a","strict":true,"parameters":schema},
            {"type":"function","name":"b","strict":true,"parameters":schema}
        ]});
        let mut bypass_options = Options::for_target(Target::OpenAiResponses);
        bypass_options.bypass = true;
        let mut budget = RequestBudget::with_limits(limits(1));
        let first = bypassed["tools"][0].get_mut("parameters").unwrap();
        normalize_with_budget(&bypass_options, first, &mut budget).unwrap();
        budget.reserve_retained(first).unwrap();
        let second = bypassed["tools"][1].get_mut("parameters").unwrap();
        assert_exhausted(budget.reserve_retained(second).unwrap_err());

        let mut budget = RequestBudget::with_limits(limits(1));
        let output = normalize_output_for_budget(
            Target::OpenAiResponses,
            &json!({"type":"string"}),
            true,
            &mut budget,
        );
        assert_exhausted(output.unwrap_err());
    }

    #[test]
    fn raw_mcp_conversion_reserves_normalized_and_canonical_schema_copies() {
        let request = json!({"tools":[
            {"name":"a","inputSchema":{"type":"object","description":"mcp-a"}},
            {"name":"b","inputSchema":{"type":"object","description":"mcp-b"}}
        ]});
        let mut budget = RequestBudget::with_limits(limits(4));
        assert_exhausted(prepare_request(&request, &mut budget).unwrap_err());
    }

    #[test]
    fn delegated_provider_assembly_and_second_override_pass_reuse_the_same_budget() {
        let request = json!({
            "messages": [],
            "tools": [{"type":"function","function":{"name":"tool","parameters":{
                "type":"object","additional_properties":false
            }}}]
        });
        let provider = crate::providers::openai::OpenAi::new("test".into());
        let target = crate::reasoning::ReplayTarget::new(
            "openai",
            "model",
            crate::reasoning::WireFormat::OpenAiResponses,
        );
        let mut budget = RequestBudget::with_limits(limits(1));
        let error = provider
            .transform_request_for_target_with_budget("model", &request, &target, &mut budget)
            .err()
            .expect("provider assembly must reject the second retained schema copy");
        assert_exhausted(error);

        let schema = json!({"type":"object","additional_properties":false});
        let mut first_body =
            json!({"tools":[{"type":"function","name":"first","parameters":schema}]});
        let mut warmed = schema.clone();
        normalize_for(Target::OpenAiResponses, &mut warmed);
        let mut budget = RequestBudget::with_limits(limits(3));
        normalize_native_tools(Target::OpenAiResponses, &mut first_body, &mut budget).unwrap();
        let mut override_body =
            json!({"tools":[{"type":"function","name":"override","parameters":schema}]});
        assert_exhausted(
            normalize_native_tools(Target::OpenAiResponses, &mut override_body, &mut budget)
                .unwrap_err(),
        );
    }
}

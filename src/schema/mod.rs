//! One schema walker with per-transport policy. External references are never
//! fetched; unresolved/cyclic schemas fall back per tool rather than per request.
mod memo;
pub mod validate;
mod walk;
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
    if options.bypass || flag("LLMSHIM_NO_SCHEMA_NORMALIZATION") {
        return Normalization {
            bypassed: true,
            ..Normalization::default()
        };
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
                *schema = cached.value.clone();
            }
            return cached.report;
        }
    }
    let input = key.as_ref().map(|_| schema.clone());
    let report = normalize_uncached(&options, schema);
    if let (Some(key), Some(input)) = (key, input) {
        memo::CACHE.insert(key, options, input, schema, report);
    }
    report
}

fn normalize_uncached(options: &Options, schema: &mut Value) -> Normalization {
    let original = schema.clone();
    match walk::normalize(&original, options) {
        Ok(value) if validate::compile(&value).is_ok() => {
            *schema = value;
            Normalization {
                changed: *schema != original,
                strict: options.strict,
                ..Normalization::default()
            }
        }
        Ok(_) | Err(()) => {
            *schema = json!({"type":"object","properties":{}});
            Normalization {
                changed: *schema != original,
                used_fallback: true,
                ..Normalization::default()
            }
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
pub(crate) fn prepare_request(request: &Value) -> Value {
    let mut request = request.clone();
    if let Some(tools) = request.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if tool.get("inputSchema").is_none() {
                continue;
            }
            normalize_mcp_tool(tool);
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
    request
}

fn tool_schema(target: Target, tool: &mut Value, key: &str) {
    let requested = tool["strict"].as_bool().unwrap_or(false);
    let Some(schema) = tool.get_mut(key) else {
        return;
    };
    let mut options = Options::for_target(target);
    options.strict = requested;
    let mut report = normalize_with(&options, schema);
    if !report.bypassed
        && (requested || matches!(target, Target::Google | Target::Anthropic))
        && schema["type"] != "object"
    {
        *schema = json!({"type":"object","properties":{}});
        report.used_fallback = true;
        report.strict = false;
    }
    if requested
        || (report.used_fallback && matches!(target, Target::OpenAiChat | Target::OpenAiResponses))
    {
        tool["strict"] = json!(report.strict);
    }
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
    let mut schema = original.clone();
    let base = normalize_for(Target::Mcp, &mut schema);
    let wrapped = schema["type"] != "object";
    if wrapped {
        schema = json!({"type":"object","properties":{"response":schema},"required":["response"]});
    }
    let mut options = Options::for_target(target);
    options.strict = strict;
    let mut normalization = normalize_with(&options, &mut schema);
    normalization.used_fallback |= base.used_fallback;
    normalization.changed |= base.changed || wrapped;
    OutputSchema {
        schema,
        wrapped,
        normalization,
    }
}

/// Normalize native tool schemas after extension overrides. No user argument,
/// enum/default literal, or conversational content is recursively rewritten.
pub(crate) fn normalize_native_tools(target: Target, body: &mut Value) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            match target {
                Target::Google => {
                    if let Some(declarations) = tool
                        .get_mut("functionDeclarations")
                        .and_then(Value::as_array_mut)
                    {
                        for declaration in declarations {
                            tool_schema(target, declaration, "parameters");
                            declaration.as_object_mut().unwrap().remove("strict");
                        }
                    }
                }
                Target::Anthropic => tool_schema(target, tool, "input_schema"),
                _ => {
                    if tool["function"].is_object() {
                        tool_schema(target, &mut tool["function"], "parameters");
                    } else if tool["type"] == "function" {
                        tool_schema(target, tool, "parameters");
                    }
                }
            }
        }
    }
}

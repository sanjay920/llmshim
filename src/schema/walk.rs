use super::{Nullable, Options, Target};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

type Result<T> = std::result::Result<T, ()>;
const TYPES: &[&str] = &[
    "object", "array", "string", "number", "integer", "boolean", "null",
];
const RENAMES: &[(&str, &str)] = &[
    ("any_of", "anyOf"),
    ("one_of", "oneOf"),
    ("all_of", "allOf"),
    ("additional_properties", "additionalProperties"),
    ("pattern_properties", "patternProperties"),
    ("property_ordering", "propertyOrdering"),
    ("min_items", "minItems"),
    ("max_items", "maxItems"),
    ("min_length", "minLength"),
    ("max_length", "maxLength"),
    ("min_properties", "minProperties"),
    ("max_properties", "maxProperties"),
    ("exclusive_minimum", "exclusiveMinimum"),
    ("exclusive_maximum", "exclusiveMaximum"),
];
const MAP_CHILDREN: &[&str] = &["properties", "patternProperties", "dependentSchemas"];
const ONE_CHILDREN: &[&str] = &[
    "items",
    "contains",
    "additionalProperties",
    "unevaluatedProperties",
    "unevaluatedItems",
    "propertyNames",
    "not",
    "if",
    "then",
    "else",
];
const ARRAY_CHILDREN: &[&str] = &["anyOf", "oneOf", "allOf", "prefixItems"];

pub(super) fn normalize(root: &Value, options: &Options) -> Result<Value> {
    let mut state = Walker {
        registry: RefRegistry::new(root, options)?,
        options,
        nodes: 0,
        literal_bytes: 0,
        refs: BTreeSet::new(),
    };
    let base = reqwest::Url::parse("https://llmshim.invalid/schema").map_err(|_| ())?;
    let result = state.node(root.clone(), 0, &base)?;
    validate(&result, options, 0)?;
    Ok(result)
}

struct Walker<'a> {
    registry: RefRegistry<'a>,
    options: &'a Options,
    nodes: usize,
    literal_bytes: usize,
    refs: BTreeSet<String>,
}
impl<'a> Walker<'a> {
    fn node(&mut self, mut value: Value, depth: usize, inherited: &reqwest::Url) -> Result<Value> {
        self.nodes += 1;
        if self.nodes > self.options.max_nodes || depth > self.options.max_depth {
            return Err(());
        }
        if value.is_boolean() {
            return if self.options.strict
                || matches!(
                    self.options.target,
                    Target::Google | Target::CloudCodeAssist | Target::OpenAiResponses
                ) {
                Err(())
            } else {
                Ok(value)
            };
        }
        let mut owned_base = value
            .get("$id")
            .and_then(Value::as_str)
            .map(|id| inherited.join(id).map_err(|_| ()))
            .transpose()?;
        if let Some(base) = owned_base.as_mut() {
            base.set_fragment(None);
        }
        let base = owned_base.as_ref().unwrap_or(inherited);
        let object = value.as_object_mut().ok_or(())?;
        for (key, literal) in object.iter().filter(|(key, _)| {
            !MAP_CHILDREN.contains(&key.as_str())
                && !ONE_CHILDREN.contains(&key.as_str())
                && !ARRAY_CHILDREN.contains(&key.as_str())
                && !matches!(key.as_str(), "$defs" | "definitions")
        }) {
            self.literal_bytes = self
                .literal_bytes
                .saturating_add(key.len())
                .saturating_add(literal.to_string().len());
        }
        if self.literal_bytes > self.options.max_literal_bytes {
            return Err(());
        }
        for (from, to) in RENAMES {
            if let Some(v) = object.remove(*from) {
                object.insert((*to).into(), v);
            }
        }
        if let Some(reference) = object.remove("$ref") {
            let reference = reference.as_str().ok_or(())?.to_owned();
            let uri = base.join(&reference).map_err(|_| ())?;
            let key = uri.to_string();
            if !self.refs.insert(key.clone()) {
                return Err(());
            }
            let (resolved, resolved_base) = self.registry.resolve(&uri)?;
            let mut resolved = self.node(resolved, depth + 1, &resolved_base)?;
            let merged = resolved.as_object_mut().ok_or(())?;
            let mut siblings = object.clone();
            siblings.remove("$id");
            merged.extend(siblings);
            let result = self.node(resolved, depth + 1, base);
            self.refs.remove(&key);
            return result;
        }
        object.remove("$defs");
        object.remove("definitions");
        if object.contains_key("$schema") {
            if self.options.target == Target::Mcp {
                object.insert(
                    "$schema".into(),
                    json!("https://json-schema.org/draft/2020-12/schema"),
                );
            } else {
                object.remove("$schema");
            }
        }
        object.remove("$id");
        object.remove("$anchor");
        object.remove("$comment");
        // Draft-07 tuple arrays and dependencies become their 2020-12 forms.
        if object.get("items").is_some_and(Value::is_array) {
            let items = object.remove("items").unwrap();
            object.insert("prefixItems".into(), items);
            if let Some(tail) = object.remove("additionalItems") {
                object.insert("items".into(), tail);
            }
        } else {
            object.remove("additionalItems");
        }
        if let Some(dependencies) = object.remove("dependencies") {
            for (key, dependency) in dependencies.as_object().ok_or(())? {
                let field = if dependency.is_array() {
                    "dependentRequired"
                } else {
                    "dependentSchemas"
                };
                let entries = object
                    .entry(field)
                    .or_insert(json!({}))
                    .as_object_mut()
                    .ok_or(())?;
                entries.insert(key.clone(), dependency.clone());
            }
        }
        if self.options.rewrite_one_of || self.options.strict {
            if let Some(one) = object.remove("oneOf") {
                if let Some(any) = object.remove("anyOf") {
                    object
                        .entry("allOf")
                        .or_insert(json!([]))
                        .as_array_mut()
                        .ok_or(())?
                        .extend([json!({"anyOf":any}), json!({"anyOf":one})]);
                } else {
                    object.insert("anyOf".into(), one);
                }
            }
        }
        if let Some(entries) = object
            .get("allOf")
            .and_then(Value::as_array)
            .filter(|a| a.len() == 1)
        {
            let child = entries[0].clone();
            object.remove("allOf");
            let child = self.node(child, depth + 1, base)?;
            object.extend(child.as_object().ok_or(())?.clone());
        }
        if object.get("type").is_none() && object.get("properties").is_some() {
            object.insert("type".into(), json!("object"));
        }
        if object.get("type").is_none() {
            if let Some(v) = object.get("const") {
                if let Some(kind) = primitive(v) {
                    object.insert("type".into(), json!(kind));
                } else if self.options.strict {
                    return Err(());
                }
            } else if let Some(values) = object.get("enum").and_then(Value::as_array) {
                if values.is_empty() {
                    return Err(());
                }
                let kind = primitive(&values[0]);
                match kind {
                    Some(kind) if values.iter().all(|v| primitive(v) == Some(kind)) => {
                        object.insert("type".into(), json!(kind));
                    }
                    _ if self.options.strict => return Err(()),
                    _ => {}
                }
            }
        }
        let stripped: Vec<_> = object
            .keys()
            .filter(|key| strip(key, self.options))
            .cloned()
            .collect();
        for key in stripped {
            let v = object.remove(&key).unwrap();
            spill(object, &key, &v);
        }
        if self.options.strip_lookarounds {
            if let Some(pattern) = object
                .get("pattern")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                let cleaned = remove_lookarounds(&pattern)?;
                if cleaned != pattern {
                    spill(object, "pattern", &json!(pattern));
                    object.insert("pattern".into(), json!(cleaned));
                }
            }
        }
        if self.options.nullable != Nullable::Preserve {
            if let Some(types) = object.get("type").and_then(Value::as_array).cloned() {
                let non_null: Vec<_> = types
                    .iter()
                    .filter(|t| t.as_str() != Some("null"))
                    .cloned()
                    .collect();
                if non_null.len() == 1 && non_null.len() != types.len() {
                    object.insert("type".into(), non_null[0].clone());
                    if self.options.nullable == Nullable::Marker {
                        object.insert("nullable".into(), json!(true));
                    }
                    if let Some(values) = object.get_mut("enum").and_then(Value::as_array_mut) {
                        values.retain(|v| !v.is_null());
                    }
                }
            }
            if self.options.nullable == Nullable::Remove {
                object.remove("nullable");
            }
        }
        // Strict type arrays become properly typed alternatives. Keep shared
        // prose on the wrapper and prune keywords belonging to another type.
        if self.options.strict || self.options.target == Target::Google {
            if let Some(types) = object.get("type").and_then(Value::as_array).cloned() {
                if types.is_empty() {
                    return Err(());
                }
                let description = object.remove("description");
                object.remove("type");
                let mut branches = Vec::new();
                let nullable =
                    self.options.target == Target::Google && types.contains(&json!("null"));
                for kind in types {
                    let kind = kind.as_str().filter(|k| TYPES.contains(k)).ok_or(())?;
                    if kind == "null" && nullable {
                        continue;
                    }
                    let mut branch = object.clone();
                    branch.insert("type".into(), json!(kind));
                    if kind != "object" {
                        for key in ["properties", "required", "additionalProperties"] {
                            branch.remove(key);
                        }
                    }
                    if kind != "array" {
                        for key in ["items", "prefixItems"] {
                            branch.remove(key);
                        }
                    }
                    if !matches!(kind, "integer" | "number") {
                        for key in [
                            "minimum",
                            "maximum",
                            "exclusiveMinimum",
                            "exclusiveMaximum",
                            "multipleOf",
                        ] {
                            branch.remove(key);
                        }
                    }
                    if kind == "null" {
                        branch.retain(|k, _| k == "type");
                    }
                    branches.push(self.node(Value::Object(branch), depth + 1, base)?);
                }
                let mut result = json!({"anyOf":branches});
                if nullable {
                    result["nullable"] = json!(true);
                }
                if let Some(d) = description {
                    result["description"] = d;
                }
                return Ok(result);
            }
        }
        let original_required: BTreeSet<String> = object
            .get("required")
            .map(|v| {
                v.as_array().ok_or(()).and_then(|a| {
                    a.iter()
                        .map(|n| n.as_str().map(str::to_owned).ok_or(()))
                        .collect()
                })
            })
            .transpose()?
            .unwrap_or_default();
        for key in MAP_CHILDREN {
            if let Some(children) = object.get_mut(*key) {
                let children = children.as_object_mut().ok_or(())?;
                for child in children.values_mut() {
                    *child = self.node(child.clone(), depth + 1, base)?;
                }
            }
        }
        for key in ONE_CHILDREN {
            let tuple_tail = *key == "items" && object.contains_key("prefixItems");
            if let Some(child) = object.get_mut(*key) {
                if child.is_boolean() && (*key == "additionalProperties" || tuple_tail) {
                    continue;
                }
                *child = self.node(child.clone(), depth + 1, base)?;
            }
        }
        for key in ARRAY_CHILDREN {
            if let Some(children) = object.get_mut(*key) {
                let children = children.as_array_mut().ok_or(())?;
                if children.is_empty() {
                    return Err(());
                }
                for child in children {
                    *child = self.node(child.clone(), depth + 1, base)?;
                }
            }
        }
        if object.get("type").and_then(Value::as_str) == Some("object") {
            object.entry("properties").or_insert(json!({}));
            if self.options.strict {
                let properties = object["properties"].as_object().ok_or(())?;
                if original_required
                    .iter()
                    .any(|k| !properties.contains_key(k))
                {
                    return Err(());
                }
                let names: Vec<_> = properties.keys().cloned().collect();
                for (name, property) in object
                    .get_mut("properties")
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                {
                    if !original_required.contains(name) && !allows_null(property) {
                        let description = property
                            .as_object_mut()
                            .and_then(|o| o.remove("description"));
                        *property = json!({"anyOf":[property.clone(),{"type":"null"}]});
                        if let Some(d) = description {
                            property["description"] = d;
                        }
                    }
                }
                object.insert("required".into(), json!(names));
                object.insert("additionalProperties".into(), json!(false));
            }
        }
        if matches!(
            self.options.target,
            Target::Google | Target::CloudCodeAssist
        ) {
            collapse_nullable_combinator(object, self.options)?;
            if self.options.target == Target::CloudCodeAssist {
                collapse_combinators(object)?;
            }
            if object
                .get("enum")
                .and_then(Value::as_array)
                .is_some_and(|a| a.iter().any(|v| !v.is_string()))
            {
                let values = object.remove("enum").unwrap();
                spill(object, "enum", &values);
            }
        }
        Ok(value)
    }
}

fn primitive(v: &Value) -> Option<&'static str> {
    match v {
        Value::Null => Some("null"),
        Value::Bool(_) => Some("boolean"),
        Value::String(_) => Some("string"),
        Value::Number(n) => Some(if n.is_i64() || n.is_u64() {
            "integer"
        } else {
            "number"
        }),
        _ => None,
    }
}
fn allows_null(v: &Value) -> bool {
    v["type"] == "null"
        || v["type"]
            .as_array()
            .is_some_and(|a| a.contains(&json!("null")))
        || v["anyOf"]
            .as_array()
            .is_some_and(|a| a.iter().any(allows_null))
}

fn strip(key: &str, options: &Options) -> bool {
    if options.strict
        && ![
            "type",
            "title",
            "description",
            "properties",
            "required",
            "additionalProperties",
            "items",
            "prefixItems",
            "anyOf",
            "allOf",
            "oneOf",
            "enum",
            "const",
            "$schema",
        ]
        .contains(&key)
    {
        return true;
    }
    if options.strict
        && (matches!(
            key,
            "format"
                | "pattern"
                | "minimum"
                | "maximum"
                | "exclusiveMinimum"
                | "exclusiveMaximum"
                | "multipleOf"
                | "examples"
                | "default"
                | "if"
                | "then"
                | "else"
                | "not"
                | "patternProperties"
                | "propertyNames"
                | "contains"
                | "minContains"
                | "maxContains"
                | "uniqueItems"
                | "$dynamicRef"
                | "$dynamicAnchor"
                | "nullable"
                | "readOnly"
                | "writeOnly"
                | "deprecated"
        ) || key.starts_with("min")
            || key.starts_with("max")
            || key.starts_with("unevaluated")
            || key.starts_with("dependent")
            || key.starts_with("content"))
    {
        return true;
    }
    if matches!(options.target, Target::Google | Target::CloudCodeAssist) {
        return ![
            "type",
            "title",
            "description",
            "format",
            "nullable",
            "enum",
            "properties",
            "required",
            "items",
            "anyOf",
            "oneOf",
            "allOf",
            "minimum",
            "maximum",
            "minItems",
            "maxItems",
            "propertyOrdering",
        ]
        .contains(&key);
    }
    false
}

fn spill(object: &mut Map<String, Value>, key: &str, value: &Value) {
    if ![
        "default",
        "examples",
        "pattern",
        "format",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
        "uniqueItems",
        "enum",
        "const",
        "contentEncoding",
        "contentMediaType",
        "if",
        "then",
        "else",
        "not",
        "dependentRequired",
        "dependentSchemas",
        "patternProperties",
        "propertyNames",
        "contains",
        "minContains",
        "maxContains",
        "readOnly",
        "writeOnly",
    ]
    .contains(&key)
    {
        return;
    }
    let marker = format!("({key}:");
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    if description.contains(&marker) {
        return;
    }
    let rendered = if key == "pattern" || key == "format" {
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string())
    } else {
        value.to_string()
    };
    object.insert(
        "description".into(),
        json!(format!("{description} ({key}: {rendered})").trim_start()),
    );
}

fn collapse_nullable_combinator(object: &mut Map<String, Value>, options: &Options) -> Result<()> {
    let Some(branches) = object.get("anyOf").and_then(Value::as_array) else {
        return Ok(());
    };
    let live: Vec<_> = branches.iter().filter(|b| b["type"] != "null").collect();
    if live.len() == 1 && live.len() != branches.len() {
        let inner = live[0].as_object().ok_or(())?.clone();
        object.remove("anyOf");
        for (key, value) in inner {
            object.entry(key).or_insert(value);
        }
        if options.nullable == Nullable::Marker {
            object.insert("nullable".into(), json!(true));
        }
    }
    Ok(())
}

fn collapse_combinators(object: &mut Map<String, Value>) -> Result<()> {
    for key in ["allOf", "anyOf", "oneOf"] {
        let Some(branches) = object.remove(key) else {
            continue;
        };
        let branches = branches.as_array().ok_or(())?;
        let kind = branches
            .first()
            .and_then(|b| b["type"].as_str())
            .ok_or(())?;
        if branches.iter().any(|b| b["type"] != kind) {
            return Err(());
        }
        let mut merged = Map::new();
        merged.insert("type".into(), json!(kind));
        let mut required: Option<BTreeSet<String>> = None;
        for branch in branches {
            let b = branch.as_object().ok_or(())?;
            let names: BTreeSet<_> = b
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            required = Some(match required {
                None => names,
                Some(old) if key == "allOf" => old.union(&names).cloned().collect(),
                Some(old) => old.intersection(&names).cloned().collect(),
            });
            for (field, value) in b {
                if field == "required" || field == "type" {
                    continue;
                }
                if field == "properties" {
                    let properties = merged
                        .entry(field)
                        .or_insert(json!({}))
                        .as_object_mut()
                        .ok_or(())?;
                    for (name, schema) in value.as_object().ok_or(())? {
                        if properties.get(name).is_some_and(|old| old != schema) {
                            return Err(());
                        }
                        properties.insert(name.clone(), schema.clone());
                    }
                } else if field == "description" {
                    let old = merged.get(field).and_then(Value::as_str).unwrap_or("");
                    if let Some(text) = value.as_str() {
                        merged.insert(field.clone(), json!(format!("{old} {text}").trim()));
                    }
                } else if merged.get(field).is_some_and(|old| old != value) {
                    return Err(());
                } else {
                    merged.insert(field.clone(), value.clone());
                }
            }
        }
        if kind == "object" {
            merged.insert("required".into(), json!(required.unwrap_or_default()));
        }
        for (key, value) in merged {
            object.entry(key).or_insert(value);
        }
    }
    Ok(())
}

fn validate(value: &Value, options: &Options, depth: usize) -> Result<()> {
    if depth > options.max_depth {
        return Err(());
    }
    if value.is_boolean() {
        return if options.strict
            || matches!(
                options.target,
                Target::Google | Target::CloudCodeAssist | Target::OpenAiResponses
            ) {
            Err(())
        } else {
            Ok(())
        };
    }
    let object = value.as_object().ok_or(())?;
    if let Some(kind) = object.get("type") {
        match kind {
            Value::String(kind) if TYPES.contains(&kind.as_str()) => {}
            Value::Array(kinds)
                if !options.strict
                    && options.nullable == Nullable::Preserve
                    && !kinds.is_empty()
                    && kinds
                        .iter()
                        .all(|k| k.as_str().is_some_and(|s| TYPES.contains(&s))) => {}
            _ => return Err(()),
        }
    } else if (options.strict || matches!(options.target, Target::Google | Target::CloudCodeAssist))
        && !object.contains_key("anyOf")
    {
        return Err(());
    }
    if matches!(options.target, Target::Google | Target::CloudCodeAssist) && value["type"] == "null"
    {
        return Err(());
    }
    if matches!(options.target, Target::Google | Target::CloudCodeAssist)
        && object
            .keys()
            .any(|key| strip(key, &Options::for_target(options.target)))
    {
        return Err(());
    }
    if options.target == Target::CloudCodeAssist
        && ["anyOf", "allOf", "oneOf", "nullable"]
            .iter()
            .any(|k| object.contains_key(*k))
    {
        return Err(());
    }
    if options.target == Target::Google
        && (object.contains_key("allOf") || object.contains_key("oneOf"))
    {
        return Err(());
    }
    if options.strict && object.contains_key("allOf") {
        return Err(());
    }
    if object.contains_key("$ref") || object.contains_key("$defs") {
        return Err(());
    }
    if let Some(required) = object.get("required") {
        let required = required.as_array().ok_or(())?;
        let mut seen = BTreeSet::new();
        if required
            .iter()
            .any(|v| v.as_str().is_none_or(|s| !seen.insert(s)))
        {
            return Err(());
        }
    }
    for key in ["title", "description", "format", "pattern"] {
        if object.get(key).is_some_and(|v| !v.is_string()) {
            return Err(());
        }
    }
    if object
        .get("enum")
        .is_some_and(|v| v.as_array().is_none_or(Vec::is_empty))
    {
        return Err(());
    }
    if object.get("nullable").is_some_and(|v| !v.is_boolean()) {
        return Err(());
    }
    for key in [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
    ] {
        if object.get(key).is_some_and(|v| !v.is_number()) {
            return Err(());
        }
    }
    if options.strict && value["type"] == "object" {
        let props = value["properties"].as_object().ok_or(())?;
        if value["additionalProperties"] != false
            || value["required"]
                .as_array()
                .is_none_or(|a| a.len() != props.len())
        {
            return Err(());
        }
    }
    if options.strict
        && value["type"] == "array"
        && !object.contains_key("items")
        && !object.contains_key("prefixItems")
    {
        return Err(());
    }
    for key in MAP_CHILDREN {
        if let Some(children) = object.get(*key) {
            for child in children.as_object().ok_or(())?.values() {
                validate(child, options, depth + 1)?;
            }
        }
    }
    for key in ONE_CHILDREN {
        if let Some(child) = object.get(*key) {
            if child.is_boolean()
                && (*key == "additionalProperties"
                    || (*key == "items" && object.contains_key("prefixItems")))
            {
                continue;
            }
            validate(child, options, depth + 1)?;
        }
    }
    for key in ARRAY_CHILDREN {
        if let Some(children) = object.get(*key) {
            for child in children.as_array().ok_or(())? {
                validate(child, options, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Remove zero-width lookaround groups, preserving consuming regex syntax.
fn remove_lookarounds(pattern: &str) -> Result<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut output = String::new();
    let mut i = 0;
    let mut class = false;
    while i < chars.len() {
        if chars[i] == '\\' {
            output.push(chars[i]);
            i += 1;
            if i >= chars.len() {
                return Err(());
            }
            output.push(chars[i]);
            i += 1;
            continue;
        }
        if chars[i] == '[' {
            class = true;
        } else if chars[i] == ']' {
            class = false;
        }
        let look = !class
            && chars[i..].starts_with(&['(', '?'])
            && (matches!(chars.get(i + 2), Some('=' | '!'))
                || chars.get(i + 2) == Some(&'<') && matches!(chars.get(i + 3), Some('=' | '!')));
        if look {
            let mut depth = 1;
            i += 2;
            let mut in_class = false;
            while i < chars.len() && depth > 0 {
                match chars[i] {
                    '\\' => {
                        i += 2;
                        continue;
                    }
                    '[' => in_class = true,
                    ']' => in_class = false,
                    '(' if !in_class => depth += 1,
                    ')' if !in_class => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            if depth != 0 {
                return Err(());
            }
        } else {
            output.push(chars[i]);
            i += 1;
        }
    }
    if class {
        return Err(());
    }
    Ok(output)
}

fn decode_fragment(fragment: &str) -> Result<String> {
    let bytes = fragment.as_bytes();
    let mut decoded = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let pair = std::str::from_utf8(bytes.get(i + 1..i + 3).ok_or(())?).map_err(|_| ())?;
            decoded.push(u8::from_str_radix(pair, 16).map_err(|_| ())?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| ())
}

struct RefRegistry<'a> {
    resources: BTreeMap<String, &'a Value>,
    anchors: BTreeMap<String, &'a Value>,
    contexts: BTreeMap<usize, Arc<reqwest::Url>>,
}
impl<'a> RefRegistry<'a> {
    fn new(root: &'a Value, options: &Options) -> Result<Self> {
        let initial =
            Arc::new(reqwest::Url::parse("https://llmshim.invalid/schema").map_err(|_| ())?);
        let mut result = Self {
            resources: BTreeMap::from([(initial.to_string(), root)]),
            anchors: BTreeMap::new(),
            contexts: BTreeMap::new(),
        };
        let mut pending = vec![(root, initial, 0usize)];
        let mut visited = 0usize;
        let mut uri_bytes = 0usize;
        while let Some((node, parent, depth)) = pending.pop() {
            visited += 1;
            if visited > options.max_nodes || depth > options.max_depth {
                return Err(());
            }
            let Some(object) = node.as_object() else {
                continue;
            };
            let mut base = parent.clone();
            if let Some(id) = object.get("$id").and_then(Value::as_str) {
                let mut uri = parent.join(id).map_err(|_| ())?;
                let fragment = uri.fragment().map(str::to_owned);
                uri.set_fragment(None);
                uri_bytes = uri_bytes.saturating_add(uri.as_str().len());
                if uri_bytes > options.max_literal_bytes {
                    return Err(());
                }
                base = Arc::new(uri.clone());
                if !id.is_empty()
                    && !id.starts_with('#')
                    && result
                        .resources
                        .insert(uri.to_string(), node)
                        .is_some_and(|old| !std::ptr::eq(old, node))
                {
                    return Err(());
                }
                if let Some(fragment) = fragment.filter(|s| !s.is_empty()) {
                    uri.set_fragment(Some(&fragment));
                    uri_bytes = uri_bytes.saturating_add(uri.as_str().len());
                    if uri_bytes > options.max_literal_bytes {
                        return Err(());
                    }
                    if result
                        .anchors
                        .insert(uri.to_string(), node)
                        .is_some_and(|old| !std::ptr::eq(old, node))
                    {
                        return Err(());
                    }
                }
            }
            result
                .contexts
                .insert(node as *const Value as usize, base.clone());
            for key in ["$anchor", "$dynamicAnchor"] {
                if let Some(anchor) = object.get(key).and_then(Value::as_str) {
                    let mut uri = base.as_ref().clone();
                    uri.set_fragment(Some(anchor));
                    uri_bytes = uri_bytes.saturating_add(uri.as_str().len());
                    if uri_bytes > options.max_literal_bytes {
                        return Err(());
                    }
                    if result
                        .anchors
                        .insert(uri.to_string(), node)
                        .is_some_and(|old| !std::ptr::eq(old, node))
                    {
                        return Err(());
                    }
                }
            }
            for key in MAP_CHILDREN.iter().copied().chain(["$defs", "definitions"]) {
                if let Some(children) = object.get(key).and_then(Value::as_object) {
                    pending.extend(children.values().map(|c| (c, base.clone(), depth + 1)));
                }
            }
            for key in ONE_CHILDREN {
                if let Some(child) = object.get(*key) {
                    pending.push((child, base.clone(), depth + 1));
                }
            }
            for key in ARRAY_CHILDREN
                .iter()
                .copied()
                .chain(["any_of", "one_of", "all_of"])
            {
                if let Some(children) = object.get(key).and_then(Value::as_array) {
                    pending.extend(children.iter().map(|c| (c, base.clone(), depth + 1)));
                }
            }
        }
        Ok(result)
    }
    fn resolve(&self, uri: &reqwest::Url) -> Result<(Value, reqwest::Url)> {
        let mut base = uri.clone();
        let fragment = decode_fragment(uri.fragment().unwrap_or(""))?;
        base.set_fragment(None);
        let resource = self.resources.get(base.as_str()).ok_or(())?;
        let node = if fragment.is_empty() {
            *resource
        } else if fragment.starts_with('/') {
            resource.pointer(&fragment).ok_or(())?
        } else {
            let mut anchored = base.clone();
            anchored.set_fragment(Some(&fragment));
            *self.anchors.get(anchored.as_str()).ok_or(())?
        };
        let context = self
            .contexts
            .get(&(node as *const Value as usize))
            .map(|base| base.as_ref().clone())
            .unwrap_or(base);
        let mut value = node.clone();
        if let Some(object) = value.as_object_mut() {
            object.remove("$id");
        }
        Ok((value, context))
    }
}

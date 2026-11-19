//! Request guardrails (note 0040): rules that refuse a request whose text matches a pattern.
//!
//! A rule is a set of regular expressions and, optionally, the tenants it applies to. They are
//! compiled once at start; matching is linear in the text (the `regex` engine has no look-around or
//! back-references), so neither a hostile pattern nor hostile text can stall a request. A match
//! returns the rule's name and nothing else: the pattern and the text it matched are never kept.

use super::config::{Guardrail, GuardrailScope};
use crate::gateway::http::RequestGuard;
use regex::{Regex, RegexSet, RegexSetBuilder};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The most memory one compiled pattern may take, and the most all the rules' patterns together
/// may take (a shared set is built once per scope, not once per rule).
const PATTERN_SIZE_LIMIT: usize = 1 << 20;
const SET_SIZE_LIMIT: usize = 64 << 20;

/// The only string leaves that are not the caller's text: bulk media. `audio_b64` is a
/// transcription's audio, and an inline image, audio or video is not something a pattern can match.
pub(crate) const BULK_KEY: &str = "audio_b64";
const MEDIA_PREFIXES: [&str; 3] = ["data:image/", "data:audio/", "data:video/"];

/// Whether a string is inline media (an image, audio or video data URI), not text.
pub(crate) fn is_media(text: &str) -> bool {
    MEDIA_PREFIXES.iter().any(|prefix| text.starts_with(prefix))
}

pub(crate) fn compile(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .size_limit(PATTERN_SIZE_LIMIT)
        .build()
}

/// One compiled set over the patterns of several rules, and which rule each pattern belongs to.
#[derive(Clone)]
struct Scope {
    set: RegexSet,
    rule_of_pattern: Vec<usize>,
}

impl Scope {
    fn build(
        rules: &[(usize, &Guardrail)],
        include_redact: bool,
    ) -> Result<Option<Self>, regex::Error> {
        let mut patterns = Vec::new();
        let mut rule_of_pattern = Vec::new();
        for (index, rule) in rules {
            // A redaction rule's patterns also refuse: redaction rewrites the message text only, and
            // a match left anywhere else (a tool description, a passthrough field) must not go out
            // (note 0044).
            let redact: &[String] = if include_redact {
                &rule.redact_matches
            } else {
                &[]
            };
            for pattern in rule.block_if_matches.iter().chain(redact.iter()) {
                patterns.push(pattern.as_str());
                rule_of_pattern.push(*index);
            }
        }
        if patterns.is_empty() {
            return Ok(None);
        }
        let set = RegexSetBuilder::new(&patterns)
            .size_limit(SET_SIZE_LIMIT)
            .build()?;
        Ok(Some(Self {
            set,
            rule_of_pattern,
        }))
    }

    /// The first rule (in the file's order) a plain text breaks, or `None`.
    fn first_rule_text(&self, text: &str) -> Option<usize> {
        self.set
            .matches(text)
            .iter()
            .map(|pattern| self.rule_of_pattern[pattern])
            .min()
    }

    /// The first rule (in the file's order) any text under `value` breaks, or `None`.
    fn first_rule(&self, value: &Value, files: bool) -> Option<usize> {
        match value {
            Value::String(text) => {
                if files && MEDIA_PREFIXES.iter().any(|prefix| text.starts_with(prefix)) {
                    return None;
                }
                self.set
                    .matches(text)
                    .iter()
                    .map(|pattern| self.rule_of_pattern[pattern])
                    .min()
            }
            Value::Array(items) => items.iter().find_map(|item| self.first_rule(item, files)),
            // Keys are the caller's text too (a tool-call argument object, a schema), so they are
            // matched as well as the values.
            Value::Object(map) => map.iter().find_map(|(key, item)| {
                self.first_rule(&Value::String(key.clone()), files)
                    .or_else(|| {
                        (key != BULK_KEY)
                            .then(|| self.first_rule(item, files))
                            .flatten()
                    })
            }),
            _ => None,
        }
    }
}

/// One direction's rules: one set for the rules that apply to everyone and one per tenant that has
/// rules of its own, so a text is scanned once per scope, not once per rule.
struct Direction {
    global: Option<Scope>,
    by_tenant: BTreeMap<String, Scope>,
}

impl Direction {
    fn build(
        rules: &[(usize, &Guardrail)],
        direction: &str,
        include_redact: bool,
    ) -> Result<Self, String> {
        let global_rules: Vec<(usize, &Guardrail)> = rules
            .iter()
            .copied()
            .filter(|(_, rule)| rule.tenants.is_none())
            .collect();
        let global = Scope::build(&global_rules, include_redact).map_err(|_| {
            format!("the {direction} rules for every caller are too large to compile together")
        })?;
        let mut tenants = BTreeSet::new();
        for (_, rule) in rules {
            tenants.extend(rule.tenants.iter().flatten().cloned());
        }
        let mut by_tenant = BTreeMap::new();
        for tenant in tenants {
            let mine: Vec<(usize, &Guardrail)> = rules
                .iter()
                .copied()
                .filter(|(_, rule)| {
                    rule.tenants
                        .as_ref()
                        .is_some_and(|list| list.contains(&tenant))
                })
                .collect();
            if let Some(scope) = Scope::build(&mine, include_redact).map_err(|_| {
                format!("the {direction} rules for one tenant are too large to compile together")
            })? {
                by_tenant.insert(tenant, scope);
            }
        }
        Ok(Self { global, by_tenant })
    }

    fn is_empty(&self) -> bool {
        self.global.is_none() && self.by_tenant.is_empty()
    }

    fn first_match(&self, tenant: &str, body: &Value, files: bool) -> Option<usize> {
        let global = self
            .global
            .as_ref()
            .and_then(|scope| scope.first_rule(body, files));
        let own = self
            .by_tenant
            .get(tenant)
            .and_then(|scope| scope.first_rule(body, files));
        // The earlier of the two in the file's order.
        [global, own].into_iter().flatten().min()
    }
}

/// A redaction rule (note 0044): the patterns, compiled both as a set (to skip a text no pattern
/// matches) and one by one (to replace), and what a match becomes.
#[derive(Clone)]
struct RedactRule {
    name: String,
    scope: GuardrailScope,
    tenants: Option<BTreeSet<String>>,
    any: RegexSet,
    each: Vec<Regex>,
    replacement: String,
}

/// The compiled rules, for requests and for responses.
pub struct Guardrails {
    names: Vec<String>,
    request: Direction,
    response: Direction,
    stream_blocks: Direction,
    redactions: Vec<RedactRule>,
}

impl Guardrails {
    /// Compile the file's rules. A failure (a rule set too large to compile together) is reported by
    /// scope, never by pattern.
    pub fn build(rules: &[Guardrail]) -> Result<Self, String> {
        let names = rules.iter().map(|rule| rule.name.clone()).collect();
        let indexed: Vec<(usize, &Guardrail)> = rules.iter().enumerate().collect();
        let reading = |reads: fn(GuardrailScope) -> bool| -> Vec<(usize, &Guardrail)> {
            indexed
                .iter()
                .copied()
                .filter(|(_, rule)| reads(rule.scope))
                .collect()
        };
        let mut redactions = Vec::new();
        for rule in rules.iter().filter(|rule| !rule.redact_matches.is_empty()) {
            let any = RegexSetBuilder::new(&rule.redact_matches)
                .size_limit(SET_SIZE_LIMIT)
                .build()
                .map_err(|_| {
                    format!(
                        "redaction rule {} has patterns that do not compile",
                        rule.name
                    )
                })?;
            let each = rule
                .redact_matches
                .iter()
                .map(|pattern| compile(pattern))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| {
                    format!(
                        "redaction rule {} has a pattern that does not compile",
                        rule.name
                    )
                })?;
            redactions.push(RedactRule {
                name: rule.name.clone(),
                scope: rule.scope,
                tenants: rule
                    .tenants
                    .as_ref()
                    .map(|tenants| tenants.iter().cloned().collect()),
                any,
                each,
                replacement: rule
                    .replacement
                    .clone()
                    .unwrap_or_else(|| "[redacted]".to_owned()),
            });
        }
        Ok(Self {
            names,
            request: Direction::build(&reading(GuardrailScope::reads_request), "request", true)?,
            response: Direction::build(&reading(GuardrailScope::reads_response), "response", true)?,
            // What a stream is checked against as it runs: the blocking patterns only, since its
            // redaction patterns are applied to the text as it is released (note 0046).
            stream_blocks: Direction::build(
                &reading(GuardrailScope::reads_response),
                "response stream",
                false,
            )?,
            redactions,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.request.is_empty() && self.response.is_empty() && self.redactions.is_empty()
    }
}

impl RequestGuard for Guardrails {
    fn first_match(&self, tenant: &str, body: &Value) -> Option<String> {
        self.request
            .first_match(tenant, body, true)
            .map(|index| self.names[index].clone())
    }

    fn first_response_match(&self, tenant: &str, body: &Value) -> Option<String> {
        self.response
            .first_match(tenant, &response_view(body), false)
            .map(|index| self.names[index].clone())
    }

    fn stream_filter(
        &self,
        tenant: &str,
    ) -> Option<Box<dyn crate::gateway::http::StreamTextFilter>> {
        let redactions: Vec<RedactRule> = self
            .redactions
            .iter()
            .filter(|rule| {
                rule.scope.reads_response()
                    && rule.tenants.as_ref().is_none_or(|set| set.contains(tenant))
            })
            .cloned()
            .collect();
        let scopes: Vec<Scope> = [
            self.stream_blocks.global.clone(),
            self.stream_blocks.by_tenant.get(tenant).cloned(),
        ]
        .into_iter()
        .flatten()
        .collect();
        if scopes.is_empty() && redactions.is_empty() {
            return None;
        }
        Some(Box::new(StreamFilter {
            names: self.names.clone(),
            scopes,
            redactions,
            held: String::new(),
        }))
    }

    fn redact(&self, tenant: &str, value: &mut Value) -> Result<Vec<String>, String> {
        self.redact_with(tenant, GuardrailScope::reads_request, |rule| {
            redact_leaves(value, rule, true, true)
        })
    }

    fn redact_response(&self, tenant: &str, answer: &mut Value) -> Result<Vec<String>, String> {
        // The text a caller will read: each choice's message content, refusal and tool calls. The
        // reasoning, its signatures and the envelope are not touched.
        self.redact_with(tenant, GuardrailScope::reads_response, |rule| {
            let mut changed = false;
            if let Some(Value::Array(choices)) = answer.get_mut("choices") {
                for choice in choices {
                    let Some(message) = choice.get_mut("message") else {
                        continue;
                    };
                    for field in ["content", "refusal", "tool_calls", "responses_output"] {
                        if let Some(part) = message.get_mut(field) {
                            changed |= redact_leaves(part, rule, true, false)?;
                        }
                    }
                }
            }
            // A transcript (note 0043) is a bare `text`.
            if let Some(text) = answer.get_mut("text") {
                changed |= redact_leaves(text, rule, true, false)?;
            }
            Ok(changed)
        })
    }
}

impl Guardrails {
    /// Run `apply` for each redaction rule that reads this direction and applies to `tenant`: the
    /// names of the rules that changed something, or the name of the one that would have made a
    /// text too large.
    fn redact_with(
        &self,
        tenant: &str,
        reads: fn(GuardrailScope) -> bool,
        mut apply: impl FnMut(&RedactRule) -> Result<bool, ()>,
    ) -> Result<Vec<String>, String> {
        let mut applied = Vec::new();
        for rule in self.redactions.iter().filter(|rule| {
            reads(rule.scope) && rule.tenants.as_ref().is_none_or(|set| set.contains(tenant))
        }) {
            match apply(rule) {
                Ok(true) => applied.push(rule.name.clone()),
                Ok(false) => {}
                Err(()) => return Err(rule.name.clone()),
            }
        }
        Ok(applied)
    }
}

/// How many bytes of streamed text are held back so a match that straddles two chunks is seen
/// whole (note 0046). A match longer than this can leak its first bytes before it is seen.
const STREAM_HOLDBACK: usize = 256;

/// The most text a stream may hold back. A redaction pattern that can match an arbitrarily long run
/// keeps the start of a match unreleased until it ends; past this the stream is cut (fail closed)
/// rather than held, and rescanned, without bound.
const STREAM_HELD_MAX: usize = 64 * 1024;

/// The filter a streamed answer's text passes through (note 0046): response blocking rules cut the
/// stream when they match, response redaction rules mask what they match, and text is released only
/// once it is `STREAM_HOLDBACK` bytes behind the newest.
struct StreamFilter {
    names: Vec<String>,
    scopes: Vec<Scope>,
    redactions: Vec<RedactRule>,
    held: String,
}

impl StreamFilter {
    /// The rule a match of the held text breaks, if any.
    fn check(&self) -> Result<(), String> {
        let found = self
            .scopes
            .iter()
            .filter_map(|scope| scope.first_rule_text(&self.held))
            .min();
        match found {
            Some(index) => Err(self.names[index].clone()),
            None => Ok(()),
        }
    }

    /// `text` with every match of every redaction rule replaced (bounded in growth).
    fn redact(&self, text: String) -> Result<String, String> {
        let mut text = text;
        for rule in &self.redactions {
            if !rule.any.is_match(&text) {
                continue;
            }
            let limit = text.len() * REDACT_GROWTH + REDACT_GROWTH_SLACK;
            for pattern in &rule.each {
                text = pattern
                    .replace_all(&text, regex::NoExpand(&rule.replacement))
                    .into_owned();
                if text.len() > limit {
                    return Err(rule.name.clone());
                }
            }
        }
        Ok(text)
    }
}

impl crate::gateway::http::StreamTextFilter for StreamFilter {
    fn push(&mut self, text: &str) -> Result<String, String> {
        self.held.push_str(text);
        // Whatever the outcome of a cut, nothing held is ever released afterwards.
        if let Err(rule) = self.check() {
            self.held.clear();
            return Err(rule);
        }
        let mut end = self.held.len().saturating_sub(STREAM_HOLDBACK);
        while !self.held.is_char_boundary(end) {
            end -= 1;
        }
        // Never release the start of a match that the cut would split: it waits for the rest.
        let mut straddling: Option<String> = None;
        loop {
            let before = end;
            for rule in &self.redactions {
                for pattern in &rule.each {
                    for found in pattern.find_iter(&self.held) {
                        if found.start() < end && end < found.end() {
                            end = found.start();
                            straddling = Some(rule.name.clone());
                        }
                    }
                }
            }
            if end == before {
                break;
            }
        }
        if self.held.len() > STREAM_HELD_MAX {
            // A match that will not end: cut the stream rather than hold it without bound.
            self.held.clear();
            return Err(
                straddling.unwrap_or_else(|| self.names.first().cloned().unwrap_or_default())
            );
        }
        let ready: String = self.held.drain(..end).collect();
        self.redact(ready).inspect_err(|_| self.held.clear())
    }

    fn finish(&mut self) -> Result<String, String> {
        // Judge the held text, and empty it either way: after a cut nothing is ever released.
        let outcome = self.check();
        let rest = std::mem::take(&mut self.held);
        outcome?;
        self.redact(rest)
    }
}

/// Keys whose strings are structure, not the message's text: never rewritten (a replaced id or tool
/// name would break the request), and `data` / `file_data`, which carry encoded files.
const REDACT_SKIPPED: [&str; 8] = [
    "id",
    "type",
    "name",
    "role",
    "tool_call_id",
    "encrypted_content",
    "thought_signature",
    "signature",
];

/// Encoded-file fields of a request: never text. (An answer has none, so they are not skipped there.)
const REDACT_FILE_KEYS: [&str; 2] = ["data", "file_data"];

/// A text may grow to at most this many times its length plus a constant by redaction: a pattern
/// that matches a single character with a long replacement, applied by several patterns in turn,
/// would otherwise multiply a request's size.
const REDACT_GROWTH: usize = 4;
const REDACT_GROWTH_SLACK: usize = 4096;

/// Replace every match of `rule` in the string leaves of `value`; whether anything changed, or the
/// rule's name if it would have made a text too large.
fn redact_leaves(
    value: &mut Value,
    rule: &RedactRule,
    structural: bool,
    files: bool,
) -> Result<bool, ()> {
    match value {
        Value::String(text) => {
            // An encoded file (any `data:` URI) is not text.
            if (files && text.starts_with("data:")) || !rule.any.is_match(text) {
                return Ok(false);
            }
            let limit = text.len() * REDACT_GROWTH + REDACT_GROWTH_SLACK;
            let mut current = std::mem::take(text);
            for pattern in &rule.each {
                current = pattern
                    .replace_all(&current, regex::NoExpand(&rule.replacement))
                    .into_owned();
                if current.len() > limit {
                    return Err(());
                }
            }
            *text = current;
            Ok(true)
        }
        Value::Array(items) => {
            // Every item is visited: this is not `any`, which would stop at the first change.
            let mut changed = false;
            for item in items {
                changed |= redact_leaves(item, rule, structural, files)?;
            }
            Ok(changed)
        }
        Value::Object(map) => {
            let mut changed = false;
            for (key, item) in map.iter_mut() {
                if structural
                    && (REDACT_SKIPPED.contains(&key.as_str())
                        || (files && REDACT_FILE_KEYS.contains(&key.as_str())))
                {
                    continue;
                }
                // A tool call's `arguments` is JSON text: read it as JSON, so a match is replaced in
                // the decoded text (never across an escape) and the result stays valid JSON. Inside
                // it every key is the caller's data, not structure.
                if key == "arguments" {
                    if let Value::String(text) = item {
                        if let Ok(mut parsed) = serde_json::from_str::<Value>(text) {
                            if redact_leaves(&mut parsed, rule, false, files)? {
                                *text = parsed.to_string();
                                changed = true;
                            }
                            continue;
                        }
                    }
                }
                changed |= redact_leaves(item, rule, structural, files)?;
            }
            Ok(changed)
        }
        _ => Ok(false),
    }
}

/// The strings of an answer a rule should read, from the `choices` of a normalized chat response
/// (note 0043): the message's text, refusal, tool-call names and arguments, and reasoning text. The
/// answer is shaped by llmshim, so what is skipped is known: signatures and encrypted reasoning
/// (opaque blobs a pattern would match inside by chance), ids, roles and other structure. The parts
/// of a content array are joined (a secret split across two parts is one string), and a tool call's
/// `arguments`, which is JSON text, is parsed so that an escaped secret (`\u0041KIA...`) is read as
/// the text it decodes to.
pub(crate) fn response_view(choices: &Value) -> Value {
    const SKIPPED: [&str; 13] = [
        "thought_signature",
        "signature",
        "encrypted_content",
        "wire_ids",
        "id",
        "item_id",
        "tool_call_id",
        "role",
        "finish_reason",
        "index",
        "type",
        "origin",
        "redacted_data",
    ];
    fn walk(value: &Value) -> Value {
        match value {
            Value::Array(items) => {
                // Content parts: the text of every part is joined into one string, so a secret split
                // across two is whole, whatever else the array holds.
                let texts: Vec<&str> = items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect();
                if texts.len() >= 2 {
                    let mut view = vec![Value::String(texts.concat())];
                    view.extend(items.iter().map(walk));
                    return Value::Array(view);
                }
                Value::Array(items.iter().map(walk).collect())
            }
            Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (key, item) in map {
                    if SKIPPED.contains(&key.as_str()) {
                        continue;
                    }
                    let item = match (key.as_str(), item) {
                        // Tool-call arguments are JSON text: read what it decodes to.
                        ("arguments", Value::String(text)) => serde_json::from_str::<Value>(text)
                            .map(|parsed| walk(&parsed))
                            .unwrap_or_else(|_| Value::String(text.clone())),
                        _ => walk(item),
                    };
                    out.insert(key.clone(), item);
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }
    walk(choices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules(extra: &str) -> Guardrails {
        let text = format!(
            r#"
            version = 1
            [providers.p]
            kind = "openai-compatible"
            base_url = "https://x.example.com/v1"
            [[keys]]
            name = "k"
            tenant = "platform"
            sha256 = "{}"
            [[keys]]
            name = "j"
            tenant = "other"
            sha256 = "{}"
            {extra}
            "#,
            "a".repeat(64),
            "b".repeat(64)
        );
        let config =
            super::super::config::ServeConfig::parse(&text, super::super::config::Format::Toml)
                .unwrap();
        Guardrails::build(&config.guardrails).unwrap()
    }

    #[test]
    fn a_match_names_the_rule_and_nothing_of_the_pattern_or_the_text() {
        let guard = rules(
            "[[guardrails]]\nname = \"cloud-keys\"\nblock_if_matches = [\"AKIA[0-9A-Z]{16}\"]",
        );
        let body =
            json!({"messages": [{"role": "user", "content": "key AKIAABCDEFGHIJKLMNOP here"}]});
        assert_eq!(
            guard.first_match("platform", &body).as_deref(),
            Some("cloud-keys")
        );
        let clean = json!({"messages": [{"role": "user", "content": "hello"}]});
        assert_eq!(guard.first_match("platform", &clean), None);
    }

    #[test]
    fn text_is_found_in_any_shape_but_structure_and_data_urls_are_not_scanned() {
        let guard = rules(
            "[[guardrails]]\nname = \"secret\"\nblock_if_matches = [\"hunter2\", \"gpt-secret\"]",
        );
        // Nested content parts, a system string, tool arguments, an embeddings input array.
        for body in [
            json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "x hunter2"}]}]}),
            json!({"system": "hunter2"}),
            json!({"messages": [{"role": "assistant", "tool_calls": [{"function": {"arguments": "{\"p\":\"hunter2\"}"}}]}]}),
            json!({"input": ["fine", "hunter2"]}),
        ] {
            assert!(guard.first_match("platform", &body).is_some(), "{body}");
        }
        // An inline image is bulk media, not the caller's text.
        let image = json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,hunter2hunter2"}}]}]});
        assert_eq!(guard.first_match("platform", &image), None);
        // Everything else is the caller's: a model, a role or a type that carries the text, a
        // field named `id` or `type` inside a tool input or schema, and object keys.
        for body in [
            json!({"model": "gpt-secret", "messages": []}),
            json!({"messages": [{"role": "hunter2", "content": "x"}]}),
            json!({"messages": [{"role": "user", "content": [{"type": "hunter2", "text": "x"}]}]}),
            json!({"messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "input": {"id": "hunter2"}}]}]}),
            json!({"tools": [{"function": {"parameters": {"properties": {"id": {
                "type": "string", "description": "hunter2"}}}}}]}),
            json!({"tools": [{"function": {"parameters": {"properties": {"hunter2": {}}}}}]}),
        ] {
            assert!(guard.first_match("platform", &body).is_some(), "{body}");
        }
    }

    #[test]
    fn a_request_is_scanned_once_per_scope_and_the_earliest_rule_in_the_file_is_named() {
        let guard = rules(
            "[[guardrails]]\nname = \"first\"\ntenants = [\"platform\"]\nblock_if_matches = [\"alpha\"]\n\
             [[guardrails]]\nname = \"second\"\nblock_if_matches = [\"beta\"]",
        );
        let both = json!({"messages": [{"role": "user", "content": "beta and alpha"}]});
        // Both rules match; the one earlier in the file is named, whichever scope holds it.
        assert_eq!(
            guard.first_match("platform", &both).as_deref(),
            Some("first")
        );
        assert_eq!(guard.first_match("other", &both).as_deref(), Some("second"));
    }

    #[test]
    fn the_worst_case_scan_is_bounded() {
        // 16 rules of 32 patterns against two megabytes of text that nearly matches: the whole scan
        // is a single pass of one combined set, not 64 of them.
        let mut text = format!(
            "version = 1\n[providers.p]\nkind = \"openai-compatible\"\nbase_url = \"https://x.example.com/v1\"\n\
             [[keys]]\nname = \"k\"\ntenant = \"platform\"\nsha256 = \"{}\"\n",
            "a".repeat(64)
        );
        for rule in 0..16 {
            let patterns: Vec<String> = (0..32)
                .map(|n| format!("\"(a|b)*x{rule}y{n}[0-9]{{3}}z\""))
                .collect();
            text.push_str(&format!(
                "[[guardrails]]\nname = \"r{rule}\"\nblock_if_matches = [{}]\n",
                patterns.join(", ")
            ));
        }
        let config =
            super::super::config::ServeConfig::parse(&text, super::super::config::Format::Toml)
                .unwrap();
        let guard = Guardrails::build(&config.guardrails).unwrap();
        let big = json!({"messages": [{"role": "user", "content": "ab".repeat(1 << 20)}]});
        let started = std::time::Instant::now();
        assert_eq!(guard.first_match("platform", &big), None);
        // A generous bound, for a loaded CI runner: the point is seconds, not minutes.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_rule_reads_the_request_the_response_or_both_and_a_response_rule_marks_its_tenants() {
        let guard = rules(
            "[[guardrails]]\nname = \"asks\"\nblock_if_matches = [\"alpha\"]\n\
             [[guardrails]]\nname = \"answers\"\nscope = \"response\"\ntenants = [\"platform\"]\nblock_if_matches = [\"beta\"]\n\
             [[guardrails]]\nname = \"either\"\nscope = \"both\"\nblock_if_matches = [\"gamma\"]",
        );
        let text = |t: &str| json!({"messages": [{"role": "user", "content": t}]});
        // A request-only rule reads requests and never responses; a response rule the reverse.
        assert_eq!(
            guard.first_match("platform", &text("alpha")).as_deref(),
            Some("asks")
        );
        assert_eq!(guard.first_response_match("platform", &text("alpha")), None);
        assert_eq!(guard.first_match("platform", &text("beta")), None);
        assert_eq!(
            guard
                .first_response_match("platform", &text("beta"))
                .as_deref(),
            Some("answers")
        );
        // The response rule is the platform tenant's alone.
        assert_eq!(guard.first_response_match("other", &text("beta")), None);
        // `both` reads both directions.
        assert_eq!(
            guard.first_match("other", &text("gamma")).as_deref(),
            Some("either")
        );
        assert_eq!(
            guard
                .first_response_match("other", &text("gamma"))
                .as_deref(),
            Some("either")
        );
        // Every tenant is under `either` (it reads responses and names no tenant).
        assert!(
            guard.stream_filter("platform").is_some() && guard.stream_filter("other").is_some()
        );
    }

    #[test]
    fn a_response_is_read_as_text_not_as_opaque_blobs_and_split_or_escaped_secrets_are_whole() {
        let choices = json!([{
            "index": 0,
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                // Opaque signature and encrypted blobs are never the answer's text.
                "reasoning": [{"type": "thinking", "text": "plan", "thought_signature":
                    {"data": "QUtJQUFCQ0RFRkdISUpLTE1OT1A=AKIAABCDEFGHIJKLMNOP", "origin": "x"}}],
                "responses_output": [{"encrypted_content": "AKIAABCDEFGHIJKLMNOP"}],
                "content": [{"type": "text", "text": "the key is AKI"}, {"type": "text", "text": "AABCDEFGHIJKLMNOP"}],
                "tool_calls": [{"id": "call_AKIAABCDEFGHIJKLMNOP", "function": {
                    "name": "f", "arguments": "{\"v\":\"\\u0041KIAABCDEFGHIJKLMNOP\"}"}}]
            }
        }]);
        let view = response_view(&choices);
        let text = view.to_string();
        // The signature, the encrypted item and the id are gone; the split and escaped secrets are
        // each whole in the view.
        assert!(
            !text.contains("QUtJ") && !text.contains("encrypted") && !text.contains("call_AKIA"),
            "{text}"
        );
        assert!(text.contains("the key is AKIAABCDEFGHIJKLMNOP"), "{text}");
        assert!(text.contains("AKIAABCDEFGHIJKLMNOP\""), "{text}");
        let guard = rules(
            "[[guardrails]]\nname = \"leak\"\nscope = \"response\"\nblock_if_matches = [\"AKIA[0-9A-Z]{16}\"]",
        );
        assert_eq!(
            guard.first_response_match("platform", &view).as_deref(),
            Some("leak")
        );
        // A clean answer that carries a long opaque signature is not blocked by a broad pattern.
        let broad = rules(
            "[[guardrails]]\nname = \"long\"\nscope = \"response\"\nblock_if_matches = [\"[A-Za-z0-9]{40}\"]",
        );
        let clean = json!([{"message": {"role": "assistant", "content": "hi",
            "reasoning": [{"thought_signature": {"data": "A".repeat(200)}}]}}]);
        assert_eq!(
            broad.first_response_match("platform", &response_view(&clean)),
            None
        );
    }

    #[test]
    fn redaction_rewrites_message_text_and_never_structure() {
        let guard = rules(
            "[[guardrails]]\nname = \"mask\"\nredact_matches = [\"AKIA[0-9A-Z]{16}\", \"secret-[0-9]+\"]\nreplacement = \"[x]\"",
        );
        let mut content = json!([
            {"type": "text", "text": "key AKIAABCDEFGHIJKLMNOP and secret-42 twice secret-7"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AKIAABCDEFGHIJKLMNOP"}},
            {"type": "text", "text": "clean"}
        ]);
        assert_eq!(
            guard.redact("platform", &mut content).unwrap(),
            vec!["mask".to_string()]
        );
        assert_eq!(content[0]["text"], "key [x] and [x] twice [x]");
        // An inline image is left alone, and so is a clean part.
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/png;base64,AKIAABCDEFGHIJKLMNOP"
        );
        assert_eq!(content[2]["text"], "clean");
        // Tool-call arguments are text, but the call's id, type and function name are structure.
        let mut calls = json!([{"id": "secret-1", "type": "function", "function": {
            "name": "secret-2", "arguments": "{\"k\":\"AKIAABCDEFGHIJKLMNOP\"}"}}]);
        assert_eq!(
            guard.redact("platform", &mut calls).unwrap(),
            vec!["mask".to_string()]
        );
        assert_eq!(calls[0]["id"], "secret-1");
        assert_eq!(calls[0]["function"]["name"], "secret-2");
        assert_eq!(calls[0]["function"]["arguments"], "{\"k\":\"[x]\"}");
        // Nothing to redact: nothing reported.
        let mut clean = json!("hello");
        assert!(guard.redact("platform", &mut clean).unwrap().is_empty());
        // A replacement that looks like a regex group reference is inserted literally.
        let literal = rules(
            "[[guardrails]]\nname = \"m\"\nredact_matches = [\"(a)\"]\nreplacement = \"$1$1\"",
        );
        let mut text = json!("a");
        literal.redact("platform", &mut text).unwrap();
        assert_eq!(text, "$1$1");
    }

    #[test]
    fn redaction_keeps_tool_arguments_valid_json_and_leaves_files_and_structure_inside_them() {
        let guard = rules(
            "[[guardrails]]\nname = \"mask\"\nredact_matches = [\"AKIA[0-9A-Z]{16}\"]\nreplacement = \"[x]\"",
        );
        // The arguments are decoded, masked and re-encoded: an escaped secret is found and the text
        // stays JSON. Inside them a key named `name` or `id` is the caller's data, not structure.
        let mut calls = json!([{"function": {"name": "f", "arguments":
            "{\"v\":\"\\u0041KIAABCDEFGHIJKLMNOP\",\"name\":\"AKIAABCDEFGHIJKLMNOP\",\"id\":\"AKIAABCDEFGHIJKLMNOP\"}"}}]);
        assert_eq!(
            guard.redact("platform", &mut calls).unwrap(),
            vec!["mask".to_string()]
        );
        let arguments: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments, json!({"v": "[x]", "name": "[x]", "id": "[x]"}));
        // Any data URI and a file's `data` field are encoded files, never text.
        let mut files = json!([
            {"type": "file", "file": {"file_data": "data:application/pdf;base64,AKIAABCDEFGHIJKLMNOP"}},
            {"type": "input_audio", "input_audio": {"data": "AKIAABCDEFGHIJKLMNOP", "format": "wav"}}
        ]);
        assert!(guard.redact("platform", &mut files).unwrap().is_empty());
    }

    #[test]
    fn a_redaction_that_would_multiply_a_text_is_refused_not_applied() {
        // One pattern for every character, a long replacement, three rules in a row: the text would
        // grow by 60x per pass, far past the bound.
        let guard = rules(
            "[[guardrails]]\nname = \"blow\"\nredact_matches = [\"a\", \"b\", \"c\"]\n\
             replacement = \"0123456789012345678901234567890123456789012345678901234567890123\"",
        );
        let mut text = json!("abc".repeat(5000));
        assert_eq!(guard.redact("platform", &mut text), Err("blow".to_string()));
    }

    #[test]
    fn a_redaction_rules_patterns_also_refuse_what_redaction_could_not_reach() {
        // The rule masks message text; a match anywhere else (a tool description in a passthrough
        // field) is refused rather than sent (note 0044).
        let guard =
            rules("[[guardrails]]\nname = \"mask\"\nredact_matches = [\"AKIA[0-9A-Z]{16}\"]");
        let elsewhere =
            json!({"tools": [{"function": {"description": "key AKIAABCDEFGHIJKLMNOP"}}]});
        assert_eq!(
            guard.first_match("platform", &elsewhere).as_deref(),
            Some("mask")
        );
        let masked = json!({"messages": [{"role": "user", "content": "key [redacted]"}]});
        assert_eq!(guard.first_match("platform", &masked), None);
    }

    #[test]
    fn a_response_redaction_masks_the_text_a_caller_reads_and_nothing_else() {
        let guard = rules(
            "[[guardrails]]\nname = \"mask\"\nscope = \"response\"\nredact_matches = [\"AKIA[0-9A-Z]{16}\"]\nreplacement = \"[x]\"",
        );
        let mut answer = json!({
            "id": "AKIAABCDEFGHIJKLMNOP",
            "model": "AKIAABCDEFGHIJKLMNOP",
            "choices": [{"index": 0, "finish_reason": "stop", "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "key AKIAABCDEFGHIJKLMNOP"}],
                "refusal": "no AKIAABCDEFGHIJKLMNOP",
                "reasoning": [{"text": "thought AKIAABCDEFGHIJKLMNOP",
                               "thought_signature": {"data": "AKIAABCDEFGHIJKLMNOP"}}],
                "tool_calls": [{"id": "call_1", "function": {"name": "f",
                    "arguments": "{\"k\":\"AKIAABCDEFGHIJKLMNOP\"}"}}]}}],
            "usage": {"prompt_tokens": 1}
        });
        assert_eq!(
            guard.redact_response("platform", &mut answer).unwrap(),
            vec!["mask".to_string()]
        );
        let message = &answer["choices"][0]["message"];
        assert_eq!(message["content"][0]["text"], "key [x]");
        assert_eq!(message["refusal"], "no [x]");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            "{\"k\":\"[x]\"}"
        );
        // The envelope, the reasoning and its signature are not the text a caller reads.
        assert_eq!(answer["id"], "AKIAABCDEFGHIJKLMNOP");
        assert_eq!(answer["model"], "AKIAABCDEFGHIJKLMNOP");
        assert_eq!(
            message["reasoning"][0]["text"],
            "thought AKIAABCDEFGHIJKLMNOP"
        );
        // A transcript is a bare `text`.
        let mut transcript = json!({"text": "AKIAABCDEFGHIJKLMNOP spoken"});
        guard.redact_response("platform", &mut transcript).unwrap();
        assert_eq!(transcript["text"], "[x] spoken");
        // A response rule is not applied to a request, nor a request rule to a response.
        let mut request = json!("AKIAABCDEFGHIJKLMNOP");
        assert!(guard.redact("platform", &mut request).unwrap().is_empty());
        let both_directions =
            rules("[[guardrails]]\nname = \"asks\"\nredact_matches = [\"alpha\"]");
        let mut reply = json!({"choices": [{"message": {"content": "alpha"}}]});
        assert!(both_directions
            .redact_response("platform", &mut reply)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn an_answer_is_never_exempt_because_it_starts_like_a_file_and_readable_output_items_are_read()
    {
        let guard = rules(
            "[[guardrails]]\nname = \"mask\"\nscope = \"response\"\nredact_matches = [\"AKIA[0-9A-Z]{16}\"]\nreplacement = \"[x]\"",
        );
        // A model that begins its answer `data:image/...` is not carrying a file: the exemption is for
        // requests, and an answer has no such thing.
        let mut answer = json!({"choices": [{"message": {
            "content": "data:image/png, key AKIAABCDEFGHIJKLMNOP",
            "tool_calls": [{"function": {"name": "f", "arguments": "data:text/plain AKIAABCDEFGHIJKLMNOP"}}],
            "responses_output": [
                {"type": "web_search_call", "action": {"query": "AKIAABCDEFGHIJKLMNOP"}},
                {"type": "reasoning", "encrypted_content": "AKIAABCDEFGHIJKLMNOP"}]}}]});
        assert_eq!(
            guard.redact_response("platform", &mut answer).unwrap(),
            vec!["mask".to_string()]
        );
        let message = &answer["choices"][0]["message"];
        assert_eq!(message["content"], "data:image/png, key [x]");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            "data:text/plain [x]"
        );
        // The readable output item is masked; the opaque encrypted one is not text.
        assert_eq!(message["responses_output"][0]["action"]["query"], "[x]");
        assert_eq!(
            message["responses_output"][1]["encrypted_content"],
            "AKIAABCDEFGHIJKLMNOP"
        );
        // Judged as an answer too: a `data:image/` start does not hide a match from a blocking rule.
        let blocker = rules(
            "[[guardrails]]\nname = \"leak\"\nscope = \"response\"\nblock_if_matches = [\"AKIA[0-9A-Z]{16}\"]",
        );
        let raw = json!([{"message": {"content": "data:image/png, key AKIAABCDEFGHIJKLMNOP"}}]);
        assert_eq!(
            blocker.first_response_match("platform", &raw).as_deref(),
            Some("leak")
        );
        // And a request's inline image is still exempt (the same text, as a request).
        let request = json!({"messages": [{"content": [{"type": "image_url", "image_url": {
            "url": "data:image/png;base64,AKIAABCDEFGHIJKLMNOP"}}]}]});
        let request_blocker =
            rules("[[guardrails]]\nname = \"leak\"\nblock_if_matches = [\"AKIA[0-9A-Z]{16}\"]");
        assert_eq!(request_blocker.first_match("platform", &request), None);
    }

    #[test]
    fn a_secret_split_across_parts_is_refused_even_beside_a_part_that_is_not_text() {
        let blocker = rules(
            "[[guardrails]]\nname = \"leak\"\nscope = \"response\"\nblock_if_matches = [\"AKIA[0-9A-Z]{16}\"]",
        );
        let split = json!([{"message": {"content": [
            {"type": "text", "text": "key AKI"},
            {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}},
            {"type": "text", "text": "AABCDEFGHIJKLMNOP"}]}}]);
        assert_eq!(
            blocker.first_response_match("platform", &split).as_deref(),
            Some("leak")
        );
    }

    #[test]
    fn a_response_redaction_that_would_grow_the_answer_too_far_is_refused_and_both_scope_reads_both(
    ) {
        let blow = rules(
            "[[guardrails]]\nname = \"blow\"\nscope = \"response\"\nredact_matches = [\"a\", \"b\", \"c\"]\n\
             replacement = \"0123456789012345678901234567890123456789012345678901234567890123\"",
        );
        let mut answer = json!({"choices": [{"message": {"content": "abc".repeat(5000)}}]});
        assert_eq!(
            blow.redact_response("platform", &mut answer),
            Err("blow".to_string())
        );
        let both = rules(
            "[[guardrails]]\nname = \"mask\"\nscope = \"both\"\nredact_matches = [\"hunter2\"]",
        );
        let mut request = json!("pw hunter2");
        let mut reply = json!({"choices": [{"message": {"content": "pw hunter2"}}]});
        assert_eq!(both.redact("platform", &mut request).unwrap().len(), 1);
        assert_eq!(
            both.redact_response("platform", &mut reply).unwrap().len(),
            1
        );
        assert_eq!(request, "pw [redacted]");
        assert_eq!(reply["choices"][0]["message"]["content"], "pw [redacted]");
    }

    /// Feed `chunks` through a tenant's stream filter: everything it released, or the rule that cut.
    fn streamed(guard: &Guardrails, tenant: &str, chunks: &[&str]) -> Result<String, String> {
        let mut filter = guard.stream_filter(tenant).expect("a rule applies");
        let mut out = String::new();
        for chunk in chunks {
            out.push_str(&filter.push(chunk)?);
        }
        out.push_str(&filter.finish()?);
        Ok(out)
    }

    #[test]
    fn a_streamed_secret_split_across_chunks_is_masked_whole_and_the_text_is_otherwise_unchanged() {
        let guard = rules(
            "[[guardrails]]\nname = \"mask\"\nscope = \"response\"\nredact_matches = [\"AKIA[0-9A-Z]{16}\"]\nreplacement = \"[x]\"",
        );
        // The key arrives in four pieces, with plenty of text around it: nothing of it is released
        // before it is whole, and the rest comes out exactly as sent.
        let pad = "lorem ipsum ".repeat(60);
        let chunks = [
            pad.as_str(),
            "the key is AK",
            "IAABCD",
            "EFGHIJKL",
            "MNOP and done",
            pad.as_str(),
        ];
        let out = streamed(&guard, "platform", &chunks).unwrap();
        assert!(!out.contains("AKIA") && !out.contains("AK"), "{out}");
        assert_eq!(
            out,
            format!("{pad}the key is [x] and done{pad}"),
            "masked in place, everything else untouched"
        );
        // A short stream (less than the holdback) is released at the end, masked.
        assert_eq!(
            streamed(&guard, "platform", &["AKIAABCDEFGHIJKLMNOP"]).unwrap(),
            "[x]"
        );
        // Multibyte text is cut only at character boundaries.
        let wide = "é".repeat(400);
        assert_eq!(
            streamed(&guard, "platform", &[wide.as_str()]).unwrap(),
            wide
        );
    }

    #[test]
    fn a_streamed_match_of_a_blocking_rule_cuts_the_stream_before_the_secret_is_sent() {
        let guard = rules(
            "[[guardrails]]\nname = \"leak\"\nscope = \"response\"\nblock_if_matches = [\"AKIA[0-9A-Z]{16}\"]",
        );
        let mut filter = guard.stream_filter("platform").unwrap();
        let pad = "lorem ipsum ".repeat(60);
        let mut released = filter.push(&pad).unwrap();
        released.push_str(&filter.push("key AKIAABC").unwrap());
        // The match completes in the next chunk: the stream is cut, and none of the key was released.
        assert_eq!(filter.push("DEFGHIJKLMNOP").unwrap_err(), "leak");
        assert!(!released.contains("AKIA"), "{released}");
        // A clean stream passes and the held text is released at the end.
        assert_eq!(
            streamed(&guard, "platform", &["all ", "clean"]).unwrap(),
            "all clean"
        );
    }

    #[test]
    fn a_cut_stream_releases_nothing_afterwards_and_a_match_that_never_ends_is_cut_not_held() {
        let blocker = rules(
            "[[guardrails]]\nname = \"leak\"\nscope = \"response\"\nblock_if_matches = [\"AKIA[0-9A-Z]{16}\"]",
        );
        let mut filter = blocker.stream_filter("platform").unwrap();
        assert_eq!(filter.push("key AKIAABCDEFGHIJKLMNOP").unwrap_err(), "leak");
        // The held text was discarded with the cut: finishing is empty, not a second error and not a
        // flush of what was held.
        assert_eq!(filter.finish().unwrap(), "");
        // A redaction pattern that can match an unbounded run keeps a match open: past the cap the
        // stream is cut, naming the rule, instead of holding and rescanning without bound.
        let greedy = rules(
            "[[guardrails]]\nname = \"run\"\nscope = \"response\"\nredact_matches = [\"[A-Za-z0-9+/=]{40,}\"]",
        );
        let mut filter = greedy.stream_filter("platform").unwrap();
        let mut cut = None;
        for _ in 0..1000 {
            match filter.push(&"A".repeat(512)) {
                Ok(released) => {
                    assert!(released.is_empty(), "nothing of an open match is released")
                }
                Err(rule) => {
                    cut = Some(rule);
                    break;
                }
            }
        }
        assert_eq!(cut.as_deref(), Some("run"));
        assert_eq!(filter.finish().unwrap(), "");
    }

    #[test]
    fn a_redaction_rule_for_one_tenant_leaves_the_others_text_alone() {
        let guard = rules(
            "[[guardrails]]\nname = \"mask\"\ntenants = [\"platform\"]\nredact_matches = [\"hunter2\"]",
        );
        let mut mine = json!("pw hunter2");
        let mut theirs = json!("pw hunter2");
        assert_eq!(guard.redact("platform", &mut mine).unwrap().len(), 1);
        assert!(guard.redact("other", &mut theirs).unwrap().is_empty());
        assert_eq!(mine, "pw [redacted]");
        assert_eq!(theirs, "pw hunter2");
    }

    #[test]
    fn only_a_request_rule_leaves_a_tenant_free_to_stream() {
        let guard = rules("[[guardrails]]\nname = \"asks\"\nblock_if_matches = [\"alpha\"]");
        assert!(guard.stream_filter("platform").is_none());
        let scoped = rules(
            "[[guardrails]]\nname = \"answers\"\nscope = \"response\"\ntenants = [\"platform\"]\nblock_if_matches = [\"beta\"]",
        );
        assert!(scoped.stream_filter("platform").is_some());
        assert!(scoped.stream_filter("other").is_none());
    }

    #[test]
    fn a_rule_that_names_tenants_applies_to_them_only() {
        let guard = rules(
            "[[guardrails]]\nname = \"scoped\"\ntenants = [\"platform\"]\nblock_if_matches = [\"forbidden\"]",
        );
        let body = json!({"messages": [{"role": "user", "content": "forbidden"}]});
        assert!(guard.first_match("platform", &body).is_some());
        assert_eq!(guard.first_match("other", &body), None);
    }
}

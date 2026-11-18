//! Request guardrails (note 0040): rules that refuse a request whose text matches a pattern.
//!
//! A rule is a set of regular expressions and, optionally, the tenants it applies to. They are
//! compiled once at start; matching is linear in the text (the `regex` engine has no look-around or
//! back-references), so neither a hostile pattern nor hostile text can stall a request. A match
//! returns the rule's name and nothing else: the pattern and the text it matched are never kept.

use super::config::{Guardrail, GuardrailScope};
use crate::gateway::http::RequestGuard;
use regex::{RegexSet, RegexSetBuilder};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The most memory one compiled pattern may take, and the most all the rules' patterns together
/// may take (a shared set is built once per scope, not once per rule).
const PATTERN_SIZE_LIMIT: usize = 1 << 20;
const SET_SIZE_LIMIT: usize = 64 << 20;

/// The only string leaves that are not the caller's text: bulk media. `audio_b64` is a
/// transcription's audio, and an inline image, audio or video is not something a pattern can match.
const BULK_KEY: &str = "audio_b64";
const MEDIA_PREFIXES: [&str; 3] = ["data:image/", "data:audio/", "data:video/"];

pub(crate) fn compile(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .size_limit(PATTERN_SIZE_LIMIT)
        .build()
}

/// One compiled set over the patterns of several rules, and which rule each pattern belongs to.
struct Scope {
    set: RegexSet,
    rule_of_pattern: Vec<usize>,
}

impl Scope {
    fn build(rules: &[(usize, &Guardrail)]) -> Result<Option<Self>, regex::Error> {
        let mut patterns = Vec::new();
        let mut rule_of_pattern = Vec::new();
        for (index, rule) in rules {
            for pattern in &rule.block_if_matches {
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

    /// The first rule (in the file's order) any text under `value` breaks, or `None`.
    fn first_rule(&self, value: &Value) -> Option<usize> {
        match value {
            Value::String(text) => {
                if MEDIA_PREFIXES.iter().any(|prefix| text.starts_with(prefix)) {
                    return None;
                }
                self.set
                    .matches(text)
                    .iter()
                    .map(|pattern| self.rule_of_pattern[pattern])
                    .min()
            }
            Value::Array(items) => items.iter().find_map(|item| self.first_rule(item)),
            // Keys are the caller's text too (a tool-call argument object, a schema), so they are
            // matched as well as the values.
            Value::Object(map) => map.iter().find_map(|(key, item)| {
                self.first_rule(&Value::String(key.clone()))
                    .or_else(|| (key != BULK_KEY).then(|| self.first_rule(item)).flatten())
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
    fn build(rules: &[(usize, &Guardrail)], direction: &str) -> Result<Self, String> {
        let global_rules: Vec<(usize, &Guardrail)> = rules
            .iter()
            .copied()
            .filter(|(_, rule)| rule.tenants.is_none())
            .collect();
        let global = Scope::build(&global_rules).map_err(|_| {
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
            if let Some(scope) = Scope::build(&mine).map_err(|_| {
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

    fn applies_to(&self, tenant: &str) -> bool {
        self.global.is_some() || self.by_tenant.contains_key(tenant)
    }

    fn first_match(&self, tenant: &str, body: &Value) -> Option<usize> {
        let global = self
            .global
            .as_ref()
            .and_then(|scope| scope.first_rule(body));
        let own = self
            .by_tenant
            .get(tenant)
            .and_then(|scope| scope.first_rule(body));
        // The earlier of the two in the file's order.
        [global, own].into_iter().flatten().min()
    }
}

/// The compiled rules, for requests and for responses.
pub struct Guardrails {
    names: Vec<String>,
    request: Direction,
    response: Direction,
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
        Ok(Self {
            names,
            request: Direction::build(&reading(GuardrailScope::reads_request), "request")?,
            response: Direction::build(&reading(GuardrailScope::reads_response), "response")?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.request.is_empty() && self.response.is_empty()
    }
}

impl RequestGuard for Guardrails {
    fn first_match(&self, tenant: &str, body: &Value) -> Option<String> {
        self.request
            .first_match(tenant, body)
            .map(|index| self.names[index].clone())
    }

    fn first_response_match(&self, tenant: &str, body: &Value) -> Option<String> {
        self.response
            .first_match(tenant, &response_view(body))
            .map(|index| self.names[index].clone())
    }

    fn reads_responses(&self, tenant: &str) -> bool {
        self.response.applies_to(tenant)
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
    const SKIPPED: [&str; 14] = [
        "thought_signature",
        "signature",
        "encrypted_content",
        "responses_output",
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
                // Content parts: one string, so a split secret is whole.
                if !items.is_empty()
                    && items
                        .iter()
                        .all(|item| item.get("text").is_some_and(Value::is_string))
                {
                    let joined: String = items
                        .iter()
                        .filter_map(|item| item.get("text").and_then(Value::as_str))
                        .collect();
                    return Value::String(joined);
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
        assert!(guard.reads_responses("platform") && guard.reads_responses("other"));
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
    fn only_a_request_rule_leaves_a_tenant_free_to_stream() {
        let guard = rules("[[guardrails]]\nname = \"asks\"\nblock_if_matches = [\"alpha\"]");
        assert!(!guard.reads_responses("platform"));
        let scoped = rules(
            "[[guardrails]]\nname = \"answers\"\nscope = \"response\"\ntenants = [\"platform\"]\nblock_if_matches = [\"beta\"]",
        );
        assert!(scoped.reads_responses("platform"));
        assert!(!scoped.reads_responses("other"));
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

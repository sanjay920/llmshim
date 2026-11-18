//! Request guardrails (note 0040): rules that refuse a request whose text matches a pattern.
//!
//! A rule is a set of regular expressions and, optionally, the tenants it applies to. They are
//! compiled once at start; matching is linear in the text (the `regex` engine has no look-around or
//! back-references), so neither a hostile pattern nor hostile text can stall a request. A match
//! returns the rule's name and nothing else: the pattern and the text it matched are never kept.

use super::config::Guardrail;
use crate::gateway::http::RequestGuard;
use regex::{RegexSet, RegexSetBuilder};
use serde_json::Value;
use std::collections::BTreeSet;

/// The most memory a compiled pattern set may take.
const SIZE_LIMIT: usize = 1 << 20;

/// Keys whose strings are structure or bulk data, never the caller's text: scanning them would
/// match identifiers, or megabytes of base64 audio.
const SKIPPED_KEYS: [&str; 8] = [
    "model",
    "role",
    "type",
    "id",
    "tool_call_id",
    "call_id",
    "audio_b64",
    "finish_reason",
];

pub(crate) fn compile(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .size_limit(SIZE_LIMIT)
        .build()
}

struct Rule {
    name: String,
    tenants: Option<BTreeSet<String>>,
    patterns: RegexSet,
}

/// The compiled rules.
pub struct Guardrails {
    rules: Vec<Rule>,
}

impl Guardrails {
    /// Compile the file's rules. Validation has already checked them; a failure here (a build
    /// assembled by hand) is reported by rule name, never by pattern.
    pub fn build(rules: &[Guardrail]) -> Result<Self, String> {
        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            let patterns = RegexSetBuilder::new(&rule.block_if_matches)
                .size_limit(SIZE_LIMIT)
                .build()
                .map_err(|_| {
                    format!(
                        "guardrail {} has a pattern that does not compile",
                        rule.name
                    )
                })?;
            compiled.push(Rule {
                name: rule.name.clone(),
                tenants: rule
                    .tenants
                    .as_ref()
                    .map(|tenants| tenants.iter().cloned().collect()),
                patterns,
            });
        }
        Ok(Self { rules: compiled })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Whether any text under `value` matches `set`, skipping structural keys and `data:` URLs.
fn text_matches(value: &Value, set: &RegexSet) -> bool {
    match value {
        Value::String(text) => !text.starts_with("data:") && set.is_match(text),
        Value::Array(items) => items.iter().any(|item| text_matches(item, set)),
        Value::Object(map) => map
            .iter()
            .any(|(key, item)| !SKIPPED_KEYS.contains(&key.as_str()) && text_matches(item, set)),
        _ => false,
    }
}

impl RequestGuard for Guardrails {
    fn first_match(&self, tenant: &str, body: &Value) -> Option<String> {
        self.rules
            .iter()
            .filter(|rule| rule.tenants.as_ref().is_none_or(|set| set.contains(tenant)))
            .find(|rule| text_matches(body, &rule.patterns))
            .map(|rule| rule.name.clone())
    }
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
        // A model id that happens to match, and an inline image, are not the caller's text.
        let structural = json!({"model": "gpt-secret", "messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,hunter2hunter2"}}]}]});
        assert_eq!(guard.first_match("platform", &structural), None);
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

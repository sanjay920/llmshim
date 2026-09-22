//! OpenRouter model-variant suffixes (`:nitro`, `:floor`, …).
//!
//! OpenRouter accepts a suffix appended to a model slug as a routing hint —
//! "run this on the fastest/cheapest/free/exact/online-capable provider that
//! serves this model" — never a distinct model. Catalog metadata (family,
//! context window, price, capabilities) is keyed on the base id; a suffixed
//! id has no row of its own unless one was hand-added. [`strip_suffix`] is
//! the one place that knows the suffix vocabulary, so every metadata lookup
//! goes through it via [`crate::lookup_id`] / [`crate::Catalog::lookup_id`]
//! instead of guessing at the string shape itself.
//!
//! This is a **closed allow-list**, not "strip everything after the last
//! colon". Ollama tags a model as `llama3:8b`; there the colon is part of
//! the model's own identity, not an OpenRouter routing hint, and a
//! shape-based rule would silently break that lookup by turning it into
//! `llama3`. Only an exact match against this list is stripped.

/// Every suffix OpenRouter recognizes as a routing hint, appended to a model
/// slug: <https://openrouter.ai/docs/features/provider-routing>. Keep this in
/// sync with OpenRouter's docs by adding entries here — never by loosening
/// the match to a general "has a colon" rule.
const KNOWN_SUFFIXES: &[&str] = &[":nitro", ":floor", ":free", ":exacto", ":online"];

/// Strip every trailing known OpenRouter variant suffix from `id`, for
/// catalog lookups only — the wire id keeps the suffix untouched.
///
/// Returns `id` unchanged when it carries no known suffix, which is also
/// what protects a non-OpenRouter id with a colon in it (an Ollama tag like
/// `llama3:8b`): `:8b` is not in [`KNOWN_SUFFIXES`], so nothing is stripped.
///
/// OpenRouter allows chaining more than one suffix (e.g. `:free:nitro`), so
/// this strips repeatedly until none remain, rather than stopping after one.
pub fn strip_suffix(id: &str) -> &str {
    let mut base = id;
    loop {
        match KNOWN_SUFFIXES.iter().find(|suffix| base.ends_with(*suffix)) {
            Some(suffix) => base = &base[..base.len() - suffix.len()],
            None => return base,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_known_suffix_is_stripped() {
        for suffix in KNOWN_SUFFIXES {
            let id = format!("deepseek/deepseek-v4.1-flash{suffix}");
            assert_eq!(strip_suffix(&id), "deepseek/deepseek-v4.1-flash");
        }
    }

    #[test]
    fn unknown_suffix_is_left_untouched() {
        // ":beta" is not an OpenRouter routing hint.
        assert_eq!(strip_suffix("vendor/model:beta"), "vendor/model:beta");
    }

    #[test]
    fn non_openrouter_colon_tag_is_left_untouched() {
        // Ollama-style tag: the colon is part of the model's own identity.
        assert_eq!(strip_suffix("llama3:8b"), "llama3:8b");
        assert_eq!(strip_suffix("ollama/llama3:8b"), "ollama/llama3:8b");
    }

    #[test]
    fn chained_suffixes_are_all_stripped() {
        assert_eq!(
            strip_suffix("openai/gpt-oss-20b:free:nitro"),
            "openai/gpt-oss-20b"
        );
    }

    #[test]
    fn id_with_no_suffix_is_unchanged() {
        assert_eq!(
            strip_suffix("deepseek/deepseek-v4.1-flash"),
            "deepseek/deepseek-v4.1-flash"
        );
    }
}

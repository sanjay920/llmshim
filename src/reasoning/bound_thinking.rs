//! Which model may read whose prefix-bound Anthropic thinking.
//!
//! Anthropic records the producing model on every thinking block. Opus 5.5 and
//! Sonnet 5.5 both bind thinking to the conversation prefix, but their
//! documented readers differ, so family-level eligibility is not enough.
//! Opus 5.5 blocks replay to Opus 5.5, Fable 5.1, and Mythos 5.1; Sonnet 5.5
//! blocks replay only to Sonnet 5.5. Sonnet 5.5 itself reads Sonnet 5,
//! Opus 4.8, Haiku 4.5, and earlier — never Opus 5, Opus 5.5, or a Fable/Mythos
//! model. A source or target with no binding rule here keeps the family-level
//! eligibility decided by the caller.

/// Whether a block produced by `source_model` may be sent to `target_model`.
pub(super) fn reader_allows(source_model: &str, target_model: &str) -> bool {
    let opus_55_source_ok = source_model != "claude-opus-5-5"
        || matches!(
            target_model,
            "claude-opus-5-5" | "claude-fable-5-1" | "claude-mythos-5-1"
        );
    let opus_55_target_ok = target_model != "claude-opus-5-5"
        || !(source_model.starts_with("claude-fable") || source_model.starts_with("claude-mythos"));
    let sonnet_55_source_ok =
        source_model != "claude-sonnet-5-5" || target_model == "claude-sonnet-5-5";
    let sonnet_55_target_ok = target_model != "claude-sonnet-5-5"
        || !(source_model.starts_with("claude-fable")
            || source_model.starts_with("claude-mythos")
            || source_model == "claude-opus-5"
            || source_model == "claude-opus-5-5");
    opus_55_source_ok && opus_55_target_ok && sonnet_55_source_ok && sonnet_55_target_ok
}

#[cfg(test)]
mod tests {
    use super::reader_allows;

    #[test]
    fn sonnet_5_5_blocks_reach_only_sonnet_5_5() {
        assert!(reader_allows("claude-sonnet-5-5", "claude-sonnet-5-5"));
        for target in [
            "claude-sonnet-5",
            "claude-opus-4-8",
            "claude-haiku-4-5",
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-fable-5-1",
        ] {
            assert!(
                !reader_allows("claude-sonnet-5-5", target),
                "Sonnet 5.5 blocks reached {target}"
            );
        }
    }

    #[test]
    fn sonnet_5_5_reads_earlier_sonnet_opus_and_haiku_but_not_opus_5_or_fable() {
        for source in ["claude-sonnet-5", "claude-opus-4-8", "claude-haiku-4-5"] {
            assert!(
                reader_allows(source, "claude-sonnet-5-5"),
                "Sonnet 5.5 refused {source}"
            );
        }
        for source in [
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
        ] {
            assert!(
                !reader_allows(source, "claude-sonnet-5-5"),
                "Sonnet 5.5 read {source}"
            );
        }
    }

    #[test]
    fn opus_5_5_readers_are_unchanged_and_unbound_models_are_unrestricted() {
        assert!(reader_allows("claude-opus-5-5", "claude-opus-5-5"));
        assert!(reader_allows("claude-opus-5-5", "claude-fable-5-1"));
        assert!(reader_allows("claude-opus-5-5", "claude-mythos-5-1"));
        assert!(!reader_allows("claude-opus-5-5", "claude-sonnet-5"));
        assert!(!reader_allows("claude-fable-5-1", "claude-opus-5-5"));
        assert!(reader_allows("claude-sonnet-4-6", "claude-opus-5"));
    }
}

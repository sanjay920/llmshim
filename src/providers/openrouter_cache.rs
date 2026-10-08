//! Cache breakpoint families for models routed through OpenRouter.
use crate::cache::BreakpointConfig;

/// Detect the cache breakpoint configuration for an OpenRouter model slug.
///
/// OpenRouter passes through model families that support explicit cache markers.
/// The slug format is `vendor/model[:variant]`, and we route based on the vendor
/// prefix. Other families rely on automatic caching (no markers needed).
pub fn model_cache_config(slug: &str) -> Option<BreakpointConfig> {
    let vendor = slug.split('/').next().unwrap_or("");
    match vendor {
        "anthropic" => Some(BreakpointConfig {
            limit: 4,
            supports_ttl: true,
        }),
        "google" => Some(BreakpointConfig {
            limit: 5,
            supports_ttl: false,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_models_support_explicit_markers() {
        let cfg = model_cache_config("anthropic/claude-sonnet-5.5");
        assert!(cfg.is_some());
        let cfg = cfg.unwrap();
        assert_eq!(cfg.limit, 4);
        assert!(cfg.supports_ttl);
    }

    #[test]
    fn google_gemini_models_support_explicit_markers() {
        let cfg = model_cache_config("google/gemini-3.8-flash");
        assert!(cfg.is_some());
        let cfg = cfg.unwrap();
        assert_eq!(cfg.limit, 5);
        assert!(!cfg.supports_ttl);
    }

    #[test]
    fn other_families_return_none() {
        assert!(model_cache_config("openai/gpt-5.5").is_none());
        assert!(model_cache_config("deepseek/deepseek-v4.1-flash").is_none());
        assert!(model_cache_config("meta-llama/llama-3.1-70b").is_none());
    }

    #[test]
    fn variant_suffixes_dont_affect_routing() {
        let without_variant = model_cache_config("anthropic/claude-sonnet-5.5");
        let with_variant = model_cache_config("anthropic/claude-sonnet-5.5:nitro");
        assert_eq!(without_variant, with_variant);
    }
}

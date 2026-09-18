//! Static model registry — shared between the CLI and proxy.
//!
//! Beyond routing identity (`id`/`provider`/`name`/`label`), each entry can
//! carry **spec metadata**: context window, output ceiling, and per-capability
//! support. This lets consumers read a model's facts from one place instead of
//! hand-maintaining a parallel table that drifts every time a model is added.
//!
//! Honesty rule: unverified facts stay [`Support::Unknown`] / `None`. We never
//! guess a number to fill a cell. Specs are a point-in-time snapshot, pinned by
//! the crate version exactly like the model list itself.
//!
//! Specs are versioned snapshots populated from official provider docs
//! (platform.claude.com, developers.openai.com, ai.google.dev, docs.x.ai).
//! `reasoning` support is additionally cross-checked against the live-verified
//! clamp logic in `src/providers/*.rs`. Provider-specific caveats:
//! - **Gemini**: publishes input and output limits separately (no combined
//!   total), so `context_window_tokens` is the documented input limit and
//!   `max_output_tokens` is the separate output limit.
//! - **Anthropic**: the 1M-token window is the documented default for the
//!   listed models; Haiku 4.5 is 200k. Output is the synchronous Messages API
//!   ceiling (higher via the Batch API beta).
//! - **xAI**: does not publish a per-model max output ceiling (`None`), and
//!   does not state streaming / parallel-tool-call support per model
//!   (`Unknown`, not upgraded from the general API behavior).
//! - `parallel_tool_calls` is `Unknown` for most models — providers rarely
//!   document it per model, and we don't infer it.

use crate::{ModelCapabilities, ModelFamily, Support};

/// A registered model: routing identity plus optional spec metadata.
///
/// `#[non_exhaustive]`: construct via the crate's [`MODELS`] table and read the
/// fields you need. New spec fields will be added over time without a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuiltinModelInfo {
    /// Full routing id, e.g. `"openai/gpt-5.6-sol"`.
    pub id: &'static str,
    /// Provider key, e.g. `"openai"`.
    pub provider: &'static str,
    /// Bare model name sent upstream, e.g. `"gpt-5.6-sol"`.
    pub name: &'static str,
    /// Human-facing label, e.g. `"GPT-5.6 Sol"`.
    pub label: &'static str,

    /// Total context window in tokens (input for Gemini — see module docs), if
    /// published.
    pub context_window_tokens: Option<u32>,
    /// Maximum output tokens the model will emit in one response, if published.
    pub max_output_tokens: Option<u32>,
    /// Per-capability support flags ([`Support::Unknown`] where unverified).
    pub capabilities: ModelCapabilities,
    /// Verified coarse model family, used for safe reasoning replay.
    pub family: Option<ModelFamily>,
}

/// Everything supported, including parallel tool calls.
const CAPS_FULL: ModelCapabilities = ModelCapabilities {
    tools: Support::Supported,
    streaming: Support::Supported,
    images: Support::Supported,
    prompt_cache: Support::Supported,
    structured_output: Support::Supported,
    parallel_tool_calls: Support::Supported,
    reasoning: Support::Supported,
    forced_tool_choice: Support::Unknown,
};
/// tools/streaming/images/prompt_cache/structured_output + reasoning supported;
/// parallel tool calls not documented per model (Unknown). (OpenAI & Gemini.)
const CAPS_STD: ModelCapabilities = ModelCapabilities {
    parallel_tool_calls: Support::Unknown,
    ..CAPS_FULL
};
/// xAI documented models: tools/images/prompt_cache/structured_output +
/// reasoning supported; streaming and parallel tool calls not stated (Unknown).
const CAPS_XAI: ModelCapabilities = ModelCapabilities {
    tools: Support::Supported,
    streaming: Support::Unknown,
    images: Support::Supported,
    prompt_cache: Support::Supported,
    structured_output: Support::Supported,
    parallel_tool_calls: Support::Unknown,
    reasoning: Support::Supported,
    forced_tool_choice: Support::Unknown,
};
/// Supported ChatGPT subscription models; also used for request validation.
pub const CHATGPT_MODELS: &[BuiltinModelInfo] = &[
    BuiltinModelInfo {
        id: "chatgpt/gpt-6-astra",
        provider: "chatgpt",
        family: Some(ModelFamily::Gpt),
        name: "gpt-6-astra",
        label: "GPT-6 Astra (ChatGPT)",
        // Subscription entitlements and limits are account-dependent.
        context_window_tokens: None,
        max_output_tokens: None,
        capabilities: ModelCapabilities {
            tools: Support::Supported,
            streaming: Support::Supported,
            images: Support::Supported,
            reasoning: Support::Supported,
            ..ModelCapabilities::unknown()
        },
    },
    BuiltinModelInfo {
        id: "chatgpt/gpt-5.6-sol",
        provider: "chatgpt",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.6-sol",
        label: "GPT-5.6 Sol (ChatGPT)",
        // Subscription entitlements and limits are account-dependent.
        context_window_tokens: None,
        max_output_tokens: None,
        capabilities: ModelCapabilities {
            tools: Support::Supported,
            streaming: Support::Supported,
            images: Support::Supported,
            reasoning: Support::Supported,
            ..ModelCapabilities::unknown()
        },
    },
    BuiltinModelInfo {
        id: "chatgpt/gpt-5.6-terra",
        provider: "chatgpt",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.6-terra",
        label: "GPT-5.6 Terra (ChatGPT)",
        // Subscription entitlements and limits are account-dependent.
        context_window_tokens: None,
        max_output_tokens: None,
        capabilities: ModelCapabilities {
            tools: Support::Supported,
            streaming: Support::Supported,
            images: Support::Supported,
            reasoning: Support::Supported,
            ..ModelCapabilities::unknown()
        },
    },
    BuiltinModelInfo {
        id: "chatgpt/gpt-5.6-luna",
        provider: "chatgpt",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.6-luna",
        label: "GPT-5.6 Luna (ChatGPT)",
        // Subscription entitlements and limits are account-dependent.
        context_window_tokens: None,
        max_output_tokens: None,
        capabilities: ModelCapabilities {
            tools: Support::Supported,
            streaming: Support::Supported,
            images: Support::Supported,
            reasoning: Support::Supported,
            ..ModelCapabilities::unknown()
        },
    },
];

/// Curated models advertised in the CLI, proxy discovery, and documentation.
/// The advertised list keeps the current model in each retained product tier.
pub const MODELS: &[BuiltinModelInfo] = &[
    BuiltinModelInfo {
        id: "openai/gpt-6-astra",
        provider: "openai",
        family: Some(ModelFamily::Gpt),
        name: "gpt-6-astra",
        label: "GPT-6 Astra",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "openai/gpt-5.6-sol",
        provider: "openai",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.6-sol",
        label: "GPT-5.6 Sol",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "openai/gpt-5.6-terra",
        provider: "openai",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.6-terra",
        label: "GPT-5.6 Terra",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "openai/gpt-5.6-luna",
        provider: "openai",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.6-luna",
        label: "GPT-5.6 Luna",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "anthropic/claude-fable-5-1",
        provider: "anthropic",
        family: Some(ModelFamily::Claude),
        name: "claude-fable-5-1",
        label: "Claude Fable 5.1",
        // Anthropic Models API and official model docs, verified 2026-09-15.
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL.with_forced_tool_choice(Support::Unsupported),
    },
    BuiltinModelInfo {
        id: "anthropic/claude-opus-5",
        provider: "anthropic",
        family: Some(ModelFamily::Claude),
        name: "claude-opus-5",
        label: "Claude Opus 5",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    BuiltinModelInfo {
        id: "anthropic/claude-sonnet-5",
        provider: "anthropic",
        family: Some(ModelFamily::Claude),
        name: "claude-sonnet-5",
        label: "Claude Sonnet 5",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    BuiltinModelInfo {
        id: "anthropic/claude-haiku-4-5-20251001",
        provider: "anthropic",
        family: Some(ModelFamily::Claude),
        name: "claude-haiku-4-5-20251001",
        label: "Claude Haiku 4.5",
        context_window_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        capabilities: CAPS_FULL,
    },
    BuiltinModelInfo {
        id: "gemini/gemini-3.8-flash",
        provider: "gemini",
        family: Some(ModelFamily::Gemini),
        name: "gemini-3.8-flash",
        label: "Gemini 3.8 Flash",
        context_window_tokens: Some(1_048_576),
        max_output_tokens: Some(65_536),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "gemini/gemini-3.5-flash-lite",
        provider: "gemini",
        family: Some(ModelFamily::Gemini),
        name: "gemini-3.5-flash-lite",
        label: "Gemini 3.5 Flash Lite",
        context_window_tokens: Some(1_048_576),
        max_output_tokens: Some(65_536),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "xai/grok-4.6",
        provider: "xai",
        family: Some(ModelFamily::Grok),
        name: "grok-4.6",
        label: "Grok 4.6",
        context_window_tokens: Some(500_000),
        max_output_tokens: None,
        capabilities: CAPS_XAI,
    },
    CHATGPT_MODELS[0],
    CHATGPT_MODELS[1],
    CHATGPT_MODELS[2],
    CHATGPT_MODELS[3],
];

/// Historical metadata remains available to explicit `spec()` lookups without
/// appearing in discovery or model pickers. Routing is provider-owned.
const LEGACY_MODELS: &[BuiltinModelInfo] = &[
    BuiltinModelInfo {
        id: "openai/gpt-5.5",
        provider: "openai",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.5",
        label: "GPT-5.5",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "openai/gpt-5.5-pro",
        provider: "openai",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.5-pro",
        label: "GPT-5.5 Pro",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        // pro: streaming explicitly not supported; no cached-input pricing.
        capabilities: CAPS_STD
            .with_streaming(Support::Unsupported)
            .with_prompt_cache(Support::Unknown),
    },
    BuiltinModelInfo {
        id: "openai/gpt-5.4",
        provider: "openai",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.4",
        label: "GPT-5.4",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "openai/gpt-5.4-pro",
        provider: "openai",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.4-pro",
        label: "GPT-5.4 Pro",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        // pro: structured outputs explicitly not supported; no cached-input row.
        capabilities: CAPS_STD
            .with_structured_output(Support::Unsupported)
            .with_prompt_cache(Support::Unknown),
    },
    BuiltinModelInfo {
        id: "openai/gpt-5.4-mini",
        provider: "openai",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.4-mini",
        label: "GPT-5.4 Mini",
        context_window_tokens: Some(400_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "openai/gpt-5.4-nano",
        provider: "openai",
        family: Some(ModelFamily::Gpt),
        name: "gpt-5.4-nano",
        label: "GPT-5.4 Nano",
        context_window_tokens: Some(400_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "anthropic/claude-fable-5",
        provider: "anthropic",
        family: Some(ModelFamily::Claude),
        name: "claude-fable-5",
        label: "Claude Fable 5",
        // Anthropic Models API and official model docs, verified 2026-09-15.
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    BuiltinModelInfo {
        id: "anthropic/claude-opus-4-8",
        provider: "anthropic",
        family: Some(ModelFamily::Claude),
        name: "claude-opus-4-8",
        label: "Claude Opus 4.8",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    BuiltinModelInfo {
        id: "anthropic/claude-opus-4-7",
        provider: "anthropic",
        family: Some(ModelFamily::Claude),
        name: "claude-opus-4-7",
        label: "Claude Opus 4.7",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    BuiltinModelInfo {
        id: "anthropic/claude-opus-4-6",
        provider: "anthropic",
        family: Some(ModelFamily::Claude),
        name: "claude-opus-4-6",
        label: "Claude Opus 4.6",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    BuiltinModelInfo {
        id: "anthropic/claude-sonnet-4-6",
        provider: "anthropic",
        family: Some(ModelFamily::Claude),
        name: "claude-sonnet-4-6",
        label: "Claude Sonnet 4.6",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    BuiltinModelInfo {
        id: "xai/grok-4.5",
        provider: "xai",
        family: Some(ModelFamily::Grok),
        name: "grok-4.5",
        label: "Grok 4.5",
        context_window_tokens: Some(500_000),
        max_output_tokens: None,
        capabilities: CAPS_XAI,
    },
    BuiltinModelInfo {
        id: "xai/grok-4.3",
        provider: "xai",
        family: Some(ModelFamily::Grok),
        name: "grok-4.3",
        label: "Grok 4.3",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: None,
        capabilities: CAPS_XAI,
    },
    BuiltinModelInfo {
        id: "xai/grok-4.20-multi-agent-beta-0309",
        provider: "xai",
        family: Some(ModelFamily::Grok),
        name: "grok-4.20-multi-agent-beta-0309",
        label: "Grok 4.20 Multi-Agent",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: None,
        capabilities: CAPS_XAI.with_reasoning(Support::Unsupported),
    },
    BuiltinModelInfo {
        id: "xai/grok-4.20-beta-0309-reasoning",
        provider: "xai",
        family: Some(ModelFamily::Grok),
        name: "grok-4.20-beta-0309-reasoning",
        label: "Grok 4.20 Reasoning",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: None,
        capabilities: CAPS_XAI.with_reasoning(Support::Unsupported),
    },
    BuiltinModelInfo {
        id: "xai/grok-4.20-beta-0309-non-reasoning",
        provider: "xai",
        family: Some(ModelFamily::Grok),
        name: "grok-4.20-beta-0309-non-reasoning",
        label: "Grok 4.20",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: None,
        capabilities: CAPS_XAI.with_reasoning(Support::Unsupported),
    },
    BuiltinModelInfo {
        id: "gemini/gemini-3.7-flash",
        provider: "gemini",
        family: Some(ModelFamily::Gemini),
        name: "gemini-3.7-flash",
        label: "Gemini 3.7 Flash",
        context_window_tokens: Some(1_048_576),
        max_output_tokens: Some(65_536),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "gemini/gemini-3.6-flash",
        provider: "gemini",
        family: Some(ModelFamily::Gemini),
        name: "gemini-3.6-flash",
        label: "Gemini 3.6 Flash",
        context_window_tokens: Some(1_048_576),
        max_output_tokens: Some(65_536),
        capabilities: CAPS_STD,
    },
    BuiltinModelInfo {
        id: "gemini/gemini-3.5-flash",
        provider: "gemini",
        family: Some(ModelFamily::Gemini),
        name: "gemini-3.5-flash",
        label: "Gemini 3.5 Flash",
        context_window_tokens: Some(1_048_576),
        max_output_tokens: Some(65_536),
        capabilities: CAPS_FULL,
    },
    BuiltinModelInfo {
        id: "gemini/gemini-3.1-flash-lite",
        provider: "gemini",
        family: Some(ModelFamily::Gemini),
        name: "gemini-3.1-flash-lite",
        label: "Gemini 3.1 Flash Lite",
        context_window_tokens: Some(1_048_576),
        max_output_tokens: Some(65_536),
        capabilities: CAPS_STD,
    },
];

/// Get models filtered to only providers that are registered (keys or OAuth).
pub fn available_models(registered_providers: &[&str]) -> Vec<&'static BuiltinModelInfo> {
    MODELS
        .iter()
        .filter(|m| registered_providers.contains(&m.provider))
        .collect()
}

/// Look up a single model's full spec by full id (`"openai/gpt-5.6-sol"`) or by
/// bare name (`"gpt-5.6-sol"`), including historical metadata.
/// Returns `None` when no metadata is recorded for the model.
pub fn spec(id: &str) -> Option<&'static BuiltinModelInfo> {
    MODELS
        .iter()
        .chain(LEGACY_MODELS)
        .find(|m| m.id == id || m.name == id)
}

/// All verified metadata, including historical entries omitted from discovery.
pub fn all() -> impl Iterator<Item = &'static BuiltinModelInfo> {
    MODELS.iter().chain(LEGACY_MODELS)
}

/// Verified Anthropic reasoning controls. This fallback is intentionally a table
/// of known model lines, not a prefix guess about future models.
pub fn anthropic_reasoning_options(model: &str) -> Vec<crate::ReasoningOption> {
    use crate::{
        EffortLevel::{High, Low, Max, Medium, Xhigh},
        ReasoningOption::*,
    };
    let mut name = model.to_ascii_lowercase().replace(['.', '_'], "-");
    if let Some((base, date)) = name.rsplit_once('-') {
        if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) {
            name = base.into();
        }
    }
    match name.as_str() {
        "claude-opus-4-6" | "claude-sonnet-4-6" => vec![Effort {
            values: vec![Low, Medium, High, Max],
        }],
        "claude-opus-4-7" | "claude-opus-4-8" | "claude-opus-5" | "claude-sonnet-5"
        | "claude-fable-5" | "claude-fable-5-1" => vec![Effort {
            values: vec![Low, Medium, High, Xhigh, Max],
        }],
        "claude-3-7-sonnet" | "claude-sonnet-4" | "claude-opus-4" | "claude-haiku-4"
        | "claude-opus-4-1" | "claude-sonnet-4-5" | "claude-opus-4-5" | "claude-haiku-4-5" => {
            vec![BudgetTokens {
                min: Some(1024),
                max: None,
            }]
        }
        _ => vec![],
    }
}

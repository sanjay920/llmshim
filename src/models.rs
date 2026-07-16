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
//! Spec source (as of 2026-07-16): populated from official provider docs
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
//! - `grok-4-1-fast-*` doc pages are currently delisted (404), so only their
//!   code-derived `reasoning` support is known; all else is `Unknown`/`None`.

/// Whether a model supports a capability. Tri-state so "we haven't verified
/// this yet" is a first-class, honest value rather than a silent `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Support {
    /// The model supports this capability.
    Supported,
    /// The model does not support this capability.
    Unsupported,
    /// Not yet verified. Consumers decide how to treat it (probe, assume, ask).
    #[default]
    Unknown,
}

/// Per-capability support flags for a model. Every field defaults to
/// [`Support::Unknown`].
///
/// Note: reasoning is intentionally a single [`Support`] ("does this model
/// accept a reasoning control at all"). The detailed per-tier mapping is not
/// duplicated here — it lives in the provider transforms and is pinned by the
/// `unit_*` tests. See `docs/src/guides/reasoning.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// Function/tool calling.
    pub tools: Support,
    /// Server-sent streaming responses.
    pub streaming: Support,
    /// Image input.
    pub images: Support,
    /// Provider-side prompt caching.
    pub prompt_cache: Support,
    /// Structured output / JSON-schema-constrained responses.
    pub structured_output: Support,
    /// More than one tool call in a single assistant turn.
    pub parallel_tool_calls: Support,
    /// Accepts a reasoning-effort control (see note above).
    pub reasoning: Support,
}

impl ModelCapabilities {
    /// All-unknown baseline — the honest default before verification. Usable in
    /// `const` context, unlike [`Default::default`].
    pub const fn unknown() -> Self {
        Self {
            tools: Support::Unknown,
            streaming: Support::Unknown,
            images: Support::Unknown,
            prompt_cache: Support::Unknown,
            structured_output: Support::Unknown,
            parallel_tool_calls: Support::Unknown,
            reasoning: Support::Unknown,
        }
    }

    /// Set tool support (const builder).
    pub const fn with_tools(mut self, s: Support) -> Self {
        self.tools = s;
        self
    }
    /// Set streaming support (const builder).
    pub const fn with_streaming(mut self, s: Support) -> Self {
        self.streaming = s;
        self
    }
    /// Set image-input support (const builder).
    pub const fn with_images(mut self, s: Support) -> Self {
        self.images = s;
        self
    }
    /// Set prompt-cache support (const builder).
    pub const fn with_prompt_cache(mut self, s: Support) -> Self {
        self.prompt_cache = s;
        self
    }
    /// Set structured-output support (const builder).
    pub const fn with_structured_output(mut self, s: Support) -> Self {
        self.structured_output = s;
        self
    }
    /// Set parallel-tool-call support (const builder).
    pub const fn with_parallel_tool_calls(mut self, s: Support) -> Self {
        self.parallel_tool_calls = s;
        self
    }
    /// Set reasoning-control support (const builder).
    pub const fn with_reasoning(mut self, s: Support) -> Self {
        self.reasoning = s;
        self
    }
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self::unknown()
    }
}

/// A registered model: routing identity plus optional spec metadata.
///
/// `#[non_exhaustive]`: construct via the crate's [`MODELS`] table and read the
/// fields you need. New spec fields will be added over time without a breaking
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelInfo {
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
};
/// reasoning known-supported (code-derived), everything else unverified — used
/// where the provider's per-model doc page is unavailable.
const CAPS_REASONING_ONLY: ModelCapabilities =
    ModelCapabilities::unknown().with_reasoning(Support::Supported);

pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "openai/gpt-5.6-sol",
        provider: "openai",
        name: "gpt-5.6-sol",
        label: "GPT-5.6 Sol",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    ModelInfo {
        id: "openai/gpt-5.6-terra",
        provider: "openai",
        name: "gpt-5.6-terra",
        label: "GPT-5.6 Terra",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    ModelInfo {
        id: "openai/gpt-5.6-luna",
        provider: "openai",
        name: "gpt-5.6-luna",
        label: "GPT-5.6 Luna",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    ModelInfo {
        id: "openai/gpt-5.5",
        provider: "openai",
        name: "gpt-5.5",
        label: "GPT-5.5",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    ModelInfo {
        id: "openai/gpt-5.5-pro",
        provider: "openai",
        name: "gpt-5.5-pro",
        label: "GPT-5.5 Pro",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        // pro: streaming explicitly not supported; no cached-input pricing.
        capabilities: CAPS_STD
            .with_streaming(Support::Unsupported)
            .with_prompt_cache(Support::Unknown),
    },
    ModelInfo {
        id: "openai/gpt-5.4",
        provider: "openai",
        name: "gpt-5.4",
        label: "GPT-5.4",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    ModelInfo {
        id: "openai/gpt-5.4-pro",
        provider: "openai",
        name: "gpt-5.4-pro",
        label: "GPT-5.4 Pro",
        context_window_tokens: Some(1_050_000),
        max_output_tokens: Some(128_000),
        // pro: structured outputs explicitly not supported; no cached-input row.
        capabilities: CAPS_STD
            .with_structured_output(Support::Unsupported)
            .with_prompt_cache(Support::Unknown),
    },
    ModelInfo {
        id: "openai/gpt-5.4-mini",
        provider: "openai",
        name: "gpt-5.4-mini",
        label: "GPT-5.4 Mini",
        context_window_tokens: Some(400_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    ModelInfo {
        id: "openai/gpt-5.4-nano",
        provider: "openai",
        name: "gpt-5.4-nano",
        label: "GPT-5.4 Nano",
        context_window_tokens: Some(400_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_STD,
    },
    ModelInfo {
        id: "anthropic/claude-opus-4-8",
        provider: "anthropic",
        name: "claude-opus-4-8",
        label: "Claude Opus 4.8",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    ModelInfo {
        id: "anthropic/claude-sonnet-5",
        provider: "anthropic",
        name: "claude-sonnet-5",
        label: "Claude Sonnet 5",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    ModelInfo {
        id: "anthropic/claude-opus-4-7",
        provider: "anthropic",
        name: "claude-opus-4-7",
        label: "Claude Opus 4.7",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    ModelInfo {
        id: "anthropic/claude-opus-4-6",
        provider: "anthropic",
        name: "claude-opus-4-6",
        label: "Claude Opus 4.6",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    ModelInfo {
        id: "anthropic/claude-sonnet-4-6",
        provider: "anthropic",
        name: "claude-sonnet-4-6",
        label: "Claude Sonnet 4.6",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: Some(128_000),
        capabilities: CAPS_FULL,
    },
    ModelInfo {
        id: "anthropic/claude-haiku-4-5-20251001",
        provider: "anthropic",
        name: "claude-haiku-4-5-20251001",
        label: "Claude Haiku 4.5",
        context_window_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
        capabilities: CAPS_FULL,
    },
    ModelInfo {
        id: "gemini/gemini-3.5-flash",
        provider: "gemini",
        name: "gemini-3.5-flash",
        label: "Gemini 3.5 Flash",
        context_window_tokens: Some(1_048_576),
        max_output_tokens: Some(65_536),
        capabilities: CAPS_FULL,
    },
    ModelInfo {
        id: "gemini/gemini-3.1-pro-preview",
        provider: "gemini",
        name: "gemini-3.1-pro-preview",
        label: "Gemini 3.1 Pro",
        context_window_tokens: Some(1_048_576),
        max_output_tokens: Some(65_536),
        capabilities: CAPS_STD,
    },
    ModelInfo {
        id: "gemini/gemini-3.1-flash-lite-preview",
        provider: "gemini",
        name: "gemini-3.1-flash-lite-preview",
        label: "Gemini 3.1 Flash Lite",
        context_window_tokens: Some(1_048_576),
        max_output_tokens: Some(65_536),
        capabilities: CAPS_STD,
    },
    ModelInfo {
        id: "gemini/gemini-3-flash-preview",
        provider: "gemini",
        name: "gemini-3-flash-preview",
        label: "Gemini 3 Flash",
        context_window_tokens: Some(1_048_576),
        max_output_tokens: Some(65_536),
        capabilities: CAPS_STD,
    },
    ModelInfo {
        id: "xai/grok-4.5",
        provider: "xai",
        name: "grok-4.5",
        label: "Grok 4.5",
        context_window_tokens: Some(500_000),
        max_output_tokens: None,
        capabilities: CAPS_XAI,
    },
    ModelInfo {
        id: "xai/grok-4.3",
        provider: "xai",
        name: "grok-4.3",
        label: "Grok 4.3",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: None,
        capabilities: CAPS_XAI,
    },
    // grok-4.20-* models are name-locked: reasoning on/off is encoded in the
    // model name and the API 400s on any reasoning parameter (see
    // `src/providers/xai.rs::is_reasoning_name_locked`).
    ModelInfo {
        id: "xai/grok-4.20-multi-agent-beta-0309",
        provider: "xai",
        name: "grok-4.20-multi-agent-beta-0309",
        label: "Grok 4.20 Multi-Agent",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: None,
        capabilities: CAPS_XAI.with_reasoning(Support::Unsupported),
    },
    ModelInfo {
        id: "xai/grok-4.20-beta-0309-reasoning",
        provider: "xai",
        name: "grok-4.20-beta-0309-reasoning",
        label: "Grok 4.20 Reasoning",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: None,
        capabilities: CAPS_XAI.with_reasoning(Support::Unsupported),
    },
    ModelInfo {
        id: "xai/grok-4.20-beta-0309-non-reasoning",
        provider: "xai",
        name: "grok-4.20-beta-0309-non-reasoning",
        label: "Grok 4.20",
        context_window_tokens: Some(1_000_000),
        max_output_tokens: None,
        capabilities: CAPS_XAI.with_reasoning(Support::Unsupported),
    },
    // grok-4-1-fast-* doc pages are delisted (404); only code-derived reasoning
    // support is known — everything else stays Unknown/None.
    ModelInfo {
        id: "xai/grok-4-1-fast-reasoning",
        provider: "xai",
        name: "grok-4-1-fast-reasoning",
        label: "Grok 4.1 Fast Reasoning",
        context_window_tokens: None,
        max_output_tokens: None,
        capabilities: CAPS_REASONING_ONLY,
    },
    ModelInfo {
        id: "xai/grok-4-1-fast-non-reasoning",
        provider: "xai",
        name: "grok-4-1-fast-non-reasoning",
        label: "Grok 4.1 Fast",
        context_window_tokens: None,
        max_output_tokens: None,
        capabilities: CAPS_REASONING_ONLY,
    },
];

/// Get models filtered to only providers that are registered (have API keys).
pub fn available_models(registered_providers: &[&str]) -> Vec<&'static ModelInfo> {
    MODELS
        .iter()
        .filter(|m| registered_providers.contains(&m.provider))
        .collect()
}

/// Look up a single model's full spec by full id (`"openai/gpt-5.6-sol"`) or by
/// bare name (`"gpt-5.6-sol"`). Returns `None` for unregistered models.
pub fn spec(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.id == id || m.name == id)
}

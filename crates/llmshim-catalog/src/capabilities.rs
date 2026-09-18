/// Whether a model supports a capability. Tri-state so "we haven't verified
/// this yet" is a first-class, honest value rather than a silent `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
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
    /// Accepts a pinned function selection, independently of general tool support.
    pub forced_tool_choice: Support,
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
            forced_tool_choice: Support::Unknown,
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

    pub const fn with_forced_tool_choice(mut self, s: Support) -> Self {
        self.forced_tool_choice = s;
        self
    }
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self::unknown()
    }
}

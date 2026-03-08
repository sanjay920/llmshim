/// Static model registry — shared between CLI and proxy.
pub struct ModelInfo {
    pub id: &'static str,
    pub provider: &'static str,
    pub name: &'static str,
    pub label: &'static str,
}

pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "openai/gpt-5.4",
        provider: "openai",
        name: "gpt-5.4",
        label: "GPT-5.4",
    },
    ModelInfo {
        id: "anthropic/claude-opus-4-6",
        provider: "anthropic",
        name: "claude-opus-4-6",
        label: "Claude Opus 4.6",
    },
    ModelInfo {
        id: "anthropic/claude-sonnet-4-6",
        provider: "anthropic",
        name: "claude-sonnet-4-6",
        label: "Claude Sonnet 4.6",
    },
    ModelInfo {
        id: "anthropic/claude-haiku-4-5-20251001",
        provider: "anthropic",
        name: "claude-haiku-4-5-20251001",
        label: "Claude Haiku 4.5",
    },
    ModelInfo {
        id: "gemini/gemini-3.1-pro-preview",
        provider: "gemini",
        name: "gemini-3.1-pro-preview",
        label: "Gemini 3.1 Pro",
    },
    ModelInfo {
        id: "gemini/gemini-3-flash-preview",
        provider: "gemini",
        name: "gemini-3-flash-preview",
        label: "Gemini 3 Flash",
    },
    ModelInfo {
        id: "gemini/gemini-3.1-flash-lite-preview",
        provider: "gemini",
        name: "gemini-3.1-flash-lite-preview",
        label: "Gemini 3.1 Flash Lite",
    },
    ModelInfo {
        id: "xai/grok-4-1-fast-reasoning",
        provider: "xai",
        name: "grok-4-1-fast-reasoning",
        label: "Grok 4.1 Fast Reasoning",
    },
    ModelInfo {
        id: "xai/grok-4-1-fast-non-reasoning",
        provider: "xai",
        name: "grok-4-1-fast-non-reasoning",
        label: "Grok 4.1 Fast",
    },
];

/// Get models filtered to only providers that are registered (have API keys).
pub fn available_models(registered_providers: &[&str]) -> Vec<&'static ModelInfo> {
    MODELS
        .iter()
        .filter(|m| registered_providers.contains(&m.provider))
        .collect()
}

use crate::error::{Result, ShimError};
use crate::provider::Provider;
use crate::providers::anthropic::Anthropic;
use crate::providers::gemini::Gemini;
use crate::providers::openai::OpenAi;
use crate::providers::xai::Xai;
use std::collections::HashMap;

/// Parses "provider/model" into (provider_key, model_name).
/// Falls back to checking aliases, then defaults.
pub fn parse_model(model: &str, aliases: &HashMap<String, String>) -> Result<(String, String)> {
    // Check aliases first
    let resolved = aliases.get(model).map(|s| s.as_str()).unwrap_or(model);

    if let Some((provider, model_name)) = resolved.split_once('/') {
        Ok((provider.to_string(), model_name.to_string()))
    } else {
        // Try to infer provider from model name
        let lower = resolved.to_lowercase();
        if lower.starts_with("gpt")
            || lower.starts_with("o1")
            || lower.starts_with("o3")
            || lower.starts_with("o4")
        {
            Ok(("openai".to_string(), resolved.to_string()))
        } else if lower.starts_with("claude") {
            Ok(("anthropic".to_string(), resolved.to_string()))
        } else if lower.starts_with("gemini") {
            Ok(("gemini".to_string(), resolved.to_string()))
        } else if lower.starts_with("grok") {
            Ok(("xai".to_string(), resolved.to_string()))
        } else {
            Err(ShimError::UnknownProvider(resolved.to_string()))
        }
    }
}

/// Registry of configured providers.
pub struct Router {
    providers: HashMap<String, Box<dyn Provider>>,
    pub aliases: HashMap<String, String>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            aliases: HashMap::new(),
        }
    }

    pub fn register(mut self, key: &str, provider: Box<dyn Provider>) -> Self {
        self.providers.insert(key.to_string(), provider);
        self
    }

    pub fn alias(mut self, from: &str, to: &str) -> Self {
        self.aliases.insert(from.to_string(), to.to_string());
        self
    }

    /// Returns the keys of all registered providers.
    pub fn provider_keys(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }

    pub fn get(&self, key: &str) -> Result<&dyn Provider> {
        self.providers
            .get(key)
            .map(|p| p.as_ref())
            .ok_or_else(|| ShimError::UnknownProvider(key.to_string()))
    }

    /// Convenience: build a router from env vars with OpenAI + Anthropic.
    pub fn from_env() -> Self {
        let mut router = Router::new();

        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            router = router.register("openai", Box::new(OpenAi::new(key)));
        }
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            router = router.register("anthropic", Box::new(Anthropic::new(key)));
        }
        if let Ok(key) = std::env::var("GEMINI_API_KEY") {
            router = router.register("gemini", Box::new(Gemini::new(key)));
        }
        if let Ok(key) = std::env::var("XAI_API_KEY") {
            router = router.register("xai", Box::new(Xai::new(key)));
        }

        router
    }

    /// Resolve model string to (provider, model_name).
    pub fn resolve(&self, model: &str) -> Result<(&dyn Provider, String)> {
        let (provider_key, model_name) = parse_model(model, &self.aliases)?;
        let provider = self.get(&provider_key)?;
        Ok((provider, model_name))
    }
}

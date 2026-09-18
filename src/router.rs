use crate::breaker::ProviderBreaker;
use crate::config::Route;
use crate::error::{Result, ShimError};
use crate::provider::Provider;
use crate::providers::anthropic::Anthropic;
use crate::providers::chatgpt::{ChatGpt, ChatGptAuth};
use crate::providers::gemini::Gemini;
use crate::providers::openai::OpenAi;
use crate::providers::openai_compat::OpenAiCompatible;
use crate::providers::openrouter::OpenRouter;
use crate::providers::xai::Xai;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

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

/// Reserved pseudo-provider prefix for named routes: `route/<name>`.
///
/// It reuses the existing `provider/model` grammar, so a named route travels
/// through every caller — an OpenAI SDK, the CLI, the proxy's admission control
/// — without any of them learning a new field.
pub const ROUTE_PREFIX: &str = "route/";

/// Registry of configured providers.
pub struct Router {
    providers: HashMap<String, std::sync::Arc<dyn Provider>>,
    pub aliases: HashMap<String, String>,
    /// Caller-defined named routes. llmshim never interprets a name.
    routes: BTreeMap<String, Route>,
    /// Provider health. Separate from rate-limit backoff: see `crate::breaker`.
    breaker: Arc<ProviderBreaker>,
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
            routes: BTreeMap::new(),
            breaker: Arc::new(ProviderBreaker::from_env()),
        }
    }

    pub fn register(mut self, key: &str, provider: Box<dyn Provider>) -> Self {
        self.providers.insert(key.to_string(), provider.into());
        self
    }

    pub fn alias(mut self, from: &str, to: &str) -> Self {
        self.aliases.insert(from.to_string(), to.to_string());
        self
    }

    /// Register a named route. The name is opaque to llmshim.
    pub fn route(mut self, name: &str, route: Route) -> Self {
        self.routes.insert(name.to_string(), route);
        self
    }

    /// Replace the provider-health breaker — how the proxy attaches a
    /// fleet-wide (Redis-coordinated) one to a router built from the env.
    pub fn with_breaker(mut self, breaker: Arc<ProviderBreaker>) -> Self {
        self.breaker = breaker;
        self
    }

    /// The provider-health breaker governing this router's dispatches.
    pub fn breaker(&self) -> &Arc<ProviderBreaker> {
        &self.breaker
    }

    /// Names of the configured routes.
    pub fn route_names(&self) -> Vec<&str> {
        self.routes.keys().map(String::as_str).collect()
    }

    /// The model a `route/<name>` string resolves to, or `None` when the string
    /// is not a named route.
    ///
    /// An unknown name is an **error**. Falling back to a default model would
    /// send traffic somewhere the caller never asked for, which is exactly the
    /// failure a named route exists to prevent.
    pub fn route_target(&self, model: &str) -> Result<Option<&Route>> {
        let Some(name) = model.strip_prefix(ROUTE_PREFIX) else {
            return Ok(None);
        };
        let route = self
            .routes
            .get(name)
            .ok_or_else(|| ShimError::ProviderError {
                status: 400,
                body: format!(
                    "unknown named route: {name:?} (configured: {:?})",
                    self.route_names()
                ),
            })?;
        if route.model.starts_with(ROUTE_PREFIX) {
            return Err(ShimError::ProviderError {
                status: 400,
                body: format!("named route {name:?} targets another route; routes do not chain"),
            });
        }
        Ok(Some(route))
    }

    /// Expand a request addressed to `route/<name>` into its model plus the
    /// route's settings. Settings the request already carries are left alone —
    /// a route is a default, not an override — so a caller can pick the route
    /// and still raise `reasoning_effort` for one call.
    ///
    /// Requests that name no route are returned borrowed and untouched.
    pub fn expand_route<'a>(
        &self,
        request: &'a serde_json::Value,
    ) -> Result<Cow<'a, serde_json::Value>> {
        let Some(model) = request.get("model").and_then(serde_json::Value::as_str) else {
            return Ok(Cow::Borrowed(request));
        };
        let Some(route) = self.route_target(model)? else {
            return Ok(Cow::Borrowed(request));
        };
        let mut expanded = request.clone();
        for (key, value) in &route.settings {
            if key == "model" || expanded.get(key).is_some() {
                continue;
            }
            expanded[key.clone()] = value.clone();
        }
        expanded["model"] = serde_json::json!(route.model);
        Ok(Cow::Owned(expanded))
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

    /// Build a router from provider env vars and a saved ChatGPT login.
    pub fn from_env() -> Self {
        let mut router = Router::new();

        // Only startup's local snapshot is synchronous. No catalog HTTP fetch
        // is ever awaited by a model request.
        if let Ok(catalog) = crate::catalog::global() {
            catalog.refresh_in_background();
        }

        // Named routes are configuration, not discovery: they come from
        // ~/.llmshim/config.toml and nothing synthesizes a default set.
        router.routes = crate::config::load().routes;

        let chatgpt_auth = ChatGptAuth::from_env();
        if chatgpt_auth.auth_path().is_file() {
            router = router.register("chatgpt", Box::new(ChatGpt::new(chatgpt_auth)));
        }

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
        if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
            router = router.register("openrouter", Box::new(OpenRouter::new(key)));
        }
        // Self-hosted OpenAI-compatible servers: the base URL is the config
        // (local vs remote); the API key is optional. Registered only when the
        // base URL is set. Address as `vllm/<served-model>` / `sglang/<served-model>`.
        if let Ok(base) = std::env::var("VLLM_BASE_URL") {
            let key = std::env::var("VLLM_API_KEY").ok().filter(|k| !k.is_empty());
            router = router.register("vllm", Box::new(OpenAiCompatible::new("vllm", base, key)));
        }
        if let Ok(base) = std::env::var("SGLANG_BASE_URL") {
            let key = std::env::var("SGLANG_API_KEY")
                .ok()
                .filter(|k| !k.is_empty());
            router = router.register(
                "sglang",
                Box::new(OpenAiCompatible::new("sglang", base, key)),
            );
        }

        router
    }

    /// Resolve model string to (provider, model_name).
    pub fn resolve(&self, model: &str) -> Result<(&dyn Provider, String)> {
        let (key, model) = self.resolve_key(model)?;
        Ok((self.get(&key)?, model))
    }

    pub fn resolve_owned(&self, model: &str) -> Result<(std::sync::Arc<dyn Provider>, String)> {
        let (key, model) = self.resolve_key(model)?;
        let provider = self
            .providers
            .get(&key)
            .cloned()
            .ok_or(ShimError::UnknownProvider(key))?;
        Ok((provider, model))
    }

    fn resolve_key(&self, model: &str) -> Result<(String, String)> {
        // A named route resolves to its model before anything else, so every
        // caller of `resolve` — including the proxy's admission control — sees
        // the real provider rather than skipping rate limiting on an
        // unrecognized string.
        let routed = self.route_target(model)?.map(|route| route.model.clone());
        let model = routed.as_deref().unwrap_or(model);
        crate::catalog::global().map_err(|error| ShimError::ProviderError {
            status: 400,
            body: format!("invalid model catalog configuration: {error}"),
        })?;
        let requested = self.aliases.get(model).map(String::as_str).unwrap_or(model);
        let metadata = crate::catalog::resolve(requested).filter(|m| {
            requested.split_once('/').is_none_or(|(prefix, _)| {
                prefix == m.provider
                    || crate::catalog::aliases::PROVIDER_ALIASES
                        .iter()
                        .any(|(alias, canonical)| prefix == *alias && m.provider == *canonical)
            })
        });
        let (provider_key, model_name) = match metadata {
            Some(m) => (m.provider.clone(), m.name.clone()),
            None => parse_model(model, &self.aliases)?,
        };
        Ok((provider_key, model_name))
    }
}

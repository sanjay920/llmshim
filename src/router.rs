use crate::breaker::ProviderBreaker;
use crate::config::Route;
use crate::credentials::{
    key_source, stored_base_url, EnvironmentOnly, KeySource, StoredCredentials,
    CREDENTIALED_PROVIDERS,
};
use crate::error::{Result, ShimError};
use crate::provider::Provider;
use crate::providers::anthropic::Anthropic;
use crate::providers::cerebras::Cerebras;
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
        } else if lower == "qwen-3.8-27b" {
            Ok(("cerebras".to_string(), resolved.to_string()))
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

/// Prefix of a model string that names a deployment: one name, several targets (design note
/// 0031). Resolved once, by [`Router::expand_route`], to one concrete `provider/model`.
pub const DEPLOYMENT_PREFIX: &str = "deployment/";

/// Registry of configured providers.
pub struct Router {
    providers: HashMap<String, std::sync::Arc<dyn Provider>>,
    pub aliases: HashMap<String, String>,
    /// Caller-defined named routes. llmshim never interprets a name.
    routes: BTreeMap<String, Route>,
    /// Named deployments (note 0031). Their balancing state is per router: a reload that
    /// builds a new router starts it fresh.
    deployments: BTreeMap<String, crate::deployment::Deployment>,
    /// Provider health. Separate from rate-limit backoff: see `crate::breaker`.
    breaker: Arc<ProviderBreaker>,
    ambient_proxy: bool,
    /// The HTTP client this router's calls use, when it owns one (0021). Without
    /// it the process-wide shared client does.
    http_client: Option<crate::client::ShimClient>,
}

/// The router as the gateway holds it: a value that a reload can replace as a
/// whole (0022). A reader takes a snapshot with [`Self::current`] and keeps it for
/// as long as it needs one consistent router; replacing the router never changes a
/// snapshot already taken.
#[cfg(feature = "gateway")]
#[derive(Clone)]
pub(crate) struct RouterHandle(Arc<std::sync::RwLock<Arc<Router>>>);

#[cfg(feature = "gateway")]
impl RouterHandle {
    pub(crate) fn new(router: Router) -> Self {
        Self::from_arc(Arc::new(router))
    }

    pub(crate) fn from_arc(router: Arc<Router>) -> Self {
        Self(Arc::new(std::sync::RwLock::new(router)))
    }

    /// The router in force now. Hold it for the duration of one decision.
    pub(crate) fn current(&self) -> Arc<Router> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Make `next` the router in force. Snapshots already taken keep theirs.
    #[allow(dead_code)]
    pub(crate) fn replace(&self, next: Router) {
        *self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(next);
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

/// One self-hosted server from `<NAME>_BASE_URL`, `<NAME>_API_KEY` and
/// `<NAME>_WIRE`. `None` when no base URL is set.
fn self_hosted_from_env(name: &str) -> Option<OpenAiCompatible> {
    let upper = name.to_ascii_uppercase();
    let base = std::env::var(format!("{upper}_BASE_URL")).ok()?;
    let key = std::env::var(format!("{upper}_API_KEY"))
        .ok()
        .filter(|k| !k.is_empty());
    let wire = match std::env::var(format!("{upper}_WIRE")).as_deref() {
        Ok("responses") => crate::reasoning::WireFormat::OpenAiResponses,
        _ => crate::reasoning::WireFormat::OpenAiChat,
    };
    Some(OpenAiCompatible::new(name, base, key).with_wire(wire))
}

/// Build one hosted provider on the key it was registered with, on the provider's own address
/// unless a stored `base_url` says otherwise. `None` for a name no arm knows, which the caller
/// treats as "register nothing".
fn hosted_provider(
    provider: &str,
    key: String,
    base_url: Option<String>,
) -> Option<Box<dyn Provider>> {
    macro_rules! on_base {
        ($provider:expr) => {{
            let provider = $provider;
            match base_url {
                Some(url) => Box::new(provider.with_base_url(url)) as Box<dyn Provider>,
                None => Box::new(provider) as Box<dyn Provider>,
            }
        }};
    }
    Some(match provider {
        "openai" => on_base!(OpenAi::new(key)),
        "anthropic" => on_base!(Anthropic::new(key)),
        "gemini" => on_base!(Gemini::new(key)),
        "xai" => on_base!(Xai::new(key)),
        "openrouter" => on_base!(OpenRouter::new(key)),
        "cerebras" => on_base!(Cerebras::new(key)),
        _ => return None,
    })
}

impl Router {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            aliases: HashMap::new(),
            routes: BTreeMap::new(),
            deployments: BTreeMap::new(),
            breaker: Arc::new(ProviderBreaker::from_env()),
            ambient_proxy: true,
            http_client: None,
        }
    }

    /// Give this router an HTTP client of its own, built from explicit settings
    /// ([`crate::client::ShimClient::from_settings`]) instead of the process-wide
    /// one built from the environment. A router that refuses ambient proxies
    /// (`without_ambient_proxy`) refuses a client that honours them when it is used,
    /// whichever order the two were set in.
    pub fn with_http_client(mut self, client: crate::client::ShimClient) -> Self {
        self.http_client = Some(client);
        self
    }

    pub(crate) fn http_client(&self) -> Option<&crate::client::ShimClient> {
        self.http_client.as_ref()
    }

    /// Refuse proxy configuration from the host environment for scoped credentials.
    pub fn without_ambient_proxy(mut self) -> Self {
        self.ambient_proxy = false;
        self
    }

    pub(crate) fn ambient_proxy(&self) -> bool {
        self.ambient_proxy
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

    /// Register a deployment: `deployment/<name>` then resolves, per request, to one of its
    /// targets (design note 0031).
    pub fn deployment(mut self, name: &str, deployment: crate::deployment::Deployment) -> Self {
        self.deployments.insert(name.to_string(), deployment);
        self
    }

    /// Where a request addressed to a deployment would go now, without picking: no counter
    /// advances and no probe is reserved. For a caller that must know the probable target before
    /// the real pick (the Responses wire's replay filter); `None` for a model that is not a
    /// deployment.
    pub fn peek_deployment(&self, request: &serde_json::Value) -> Option<String> {
        let name = request
            .get("model")?
            .as_str()?
            .strip_prefix(DEPLOYMENT_PREFIX)?;
        let deployment = self.deployments.get(name)?;
        let now = std::time::Instant::now();
        let usable = |target: &str| match self.resolve(target) {
            Ok((provider, _)) => {
                !self.breaker.should_skip(provider.instance())
                    || self.breaker.probe_due(provider.instance(), now)
            }
            Err(_) => true,
        };
        Some(
            deployment
                .peek(&usable, crate::deployment::conversation_hash(request))
                .model
                .clone(),
        )
    }

    /// The configured deployment names, in order.
    pub fn deployment_names(&self) -> Vec<&str> {
        self.deployments.keys().map(String::as_str).collect()
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
                retry_after: None,
            })?;
        if route.model.starts_with(ROUTE_PREFIX) {
            return Err(ShimError::ProviderError {
                status: 400,
                body: format!("named route {name:?} targets another route; routes do not chain"),
                retry_after: None,
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
        if let Some(name) = model.strip_prefix(DEPLOYMENT_PREFIX) {
            let deployment =
                self.deployments
                    .get(name)
                    .ok_or_else(|| ShimError::ProviderError {
                        status: 400,
                        body: format!(
                            "unknown deployment: {name:?} (configured: {:?})",
                            self.deployment_names()
                        ),
                        retry_after: None,
                    })?;
            // A target whose circuit is open is skipped; one whose cooldown has passed is
            // probed (a real request goes through, because an open circuit closes only when a
            // dispatch records a success). Local health only: the shared view is asynchronous.
            let now = std::time::Instant::now();
            // Read-only: asking about every target must not consume a probe slot for one that is not
            // chosen (it would never report, and the target would stay half-open).
            let usable = |target: &str| match self.resolve(target) {
                Ok((provider, _)) => {
                    let name = provider.instance();
                    !self.breaker.should_skip(name) || self.breaker.probe_due(name, now)
                }
                // A target that no longer resolves is the file's problem, not health.
                Err(_) => true,
            };
            let target = deployment.pick(&usable, crate::deployment::conversation_hash(request));
            // The probe is reserved for the target that was picked, whose request will report it.
            if let Ok((provider, _)) = self.resolve(&target.model) {
                if self.breaker.should_skip(provider.instance()) {
                    let _ = self.breaker.try_admit_probe(provider.instance(), now);
                }
            }
            let mut expanded = request.clone();
            expanded["model"] = serde_json::json!(target.model);
            return Ok(Cow::Owned(expanded));
        }
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

    /// The models registered providers serve beyond the curated list, as
    /// `(provider key, model)` pairs in a stable order.
    pub fn served_models(&self) -> Vec<(String, String)> {
        let mut served: Vec<(String, String)> = self
            .providers
            .iter()
            .flat_map(|(key, provider)| {
                provider
                    .served_models()
                    .into_iter()
                    .map(move |model| (key.clone(), model))
            })
            .collect();
        served.sort();
        served
    }

    pub fn get(&self, key: &str) -> Result<&dyn Provider> {
        self.providers
            .get(key)
            .map(|p| p.as_ref())
            .ok_or_else(|| ShimError::UnknownProvider(key.to_string()))
    }

    /// Build a router from provider env vars and a saved ChatGPT login, and
    /// schedule one background refresh of the model catalog.
    ///
    /// This is the daemon's constructor. The proxy starts once and serves for
    /// days, so a single fetch at startup is what keeps its prices and
    /// capabilities current for the rest of its life. A process that starts
    /// many times a day, or must run air-gapped, wants
    /// [`Router::from_env_without_catalog_refresh`] instead and decides for
    /// itself when — or whether — to call
    /// [`Router::refresh_catalog_in_background`].
    pub fn from_env() -> Self {
        let router = Self::from_env_without_catalog_refresh();
        router.refresh_catalog_in_background();
        router
    }

    /// [`Router::from_env`] minus the catalog refresh: building a router makes
    /// no network call.
    ///
    /// The catalog is still there — the vendored snapshot, whatever an earlier
    /// refresh cached on disk, and the local override files — so `resolve` and
    /// pricing work exactly as they do after `from_env`; the data is simply
    /// never newer than the disk. For a short-lived embedder that is the right
    /// default: a CLI that phones home for a price list before the user has
    /// typed anything is doing something nobody asked for, and a machine with
    /// no route out should not have to discover `LLMSHIM_CATALOG_OFFLINE` to
    /// stop it. The fetch becomes something the caller asks for.
    pub fn from_env_without_catalog_refresh() -> Self {
        Self::from_credentials_without_catalog_refresh(&EnvironmentOnly)
    }

    /// [`Router::from_env_without_catalog_refresh`] with somewhere to fall back to for a
    /// provider the environment has no key for: an embedder's own saved login
    /// ([`crate::credentials`]).
    ///
    /// The environment still wins, so this only ever registers a provider that would otherwise
    /// have been absent. `stored` is asked by provider name, never by variable name, so a
    /// caller holds no provider-specific knowledge — which is the whole reason the seam is here
    /// rather than in the caller.
    pub fn from_credentials_without_catalog_refresh(stored: &dyn StoredCredentials) -> Self {
        Self::from_credentials_with_env(stored, &|name| std::env::var(name).ok())
    }

    /// [`Router::from_credentials_without_catalog_refresh`] with the environment lookup
    /// injected, so a caller can decide what counts as exported without mutating the process
    /// environment. The credential precedence is already tested this way at the `key_for`
    /// seam; this exposes the same seam one level up, where a stored address is read.
    pub fn from_credentials_with_env(
        stored: &dyn StoredCredentials,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Self {
        Self::from_credential_sources(stored, environment, true)
    }

    /// A tenant router never reads host routes, exported keys, or host OAuth files.
    pub fn from_scoped_credentials(stored: &dyn StoredCredentials) -> Self {
        Self::from_credential_sources(stored, &|_| None, false).without_ambient_proxy()
    }

    fn from_credential_sources(
        stored: &dyn StoredCredentials,
        environment: &dyn Fn(&str) -> Option<String>,
        host_configuration: bool,
    ) -> Self {
        let mut router = Router::new();

        // Named routes are configuration, not discovery: they come from
        // ~/.llmshim/config.toml and nothing synthesizes a default set.
        if host_configuration {
            router.routes = crate::config::load().routes;
        }

        if host_configuration {
            let chatgpt_auth = ChatGptAuth::from_env();
            if chatgpt_auth.auth_path().is_file() {
                router = router.register("chatgpt", Box::new(ChatGpt::new(chatgpt_auth)));
            }
        }

        for provider in CREDENTIALED_PROVIDERS {
            let Some(source) = key_source(provider, environment, stored) else {
                continue;
            };
            // A stored address belongs only to the stored key: an exported key is never sent
            // to it, and an invalid stored address registers nothing rather than fall back to
            // the provider's own host, which the key was never meant for.
            let base_url = match &source {
                KeySource::Environment(_) => None,
                KeySource::Stored(_) => match stored.base_url_for(provider) {
                    None => None,
                    Some(raw) => match stored_base_url(&raw) {
                        Some(url) => Some(url),
                        None => continue,
                    },
                },
            };
            if let Some(built) = hosted_provider(provider, source.into_key(), base_url) {
                router = router.register(provider, built);
            }
        }
        // Self-hosted OpenAI-compatible servers: the base URL is the config
        // (local vs remote); the API key is optional. Registered only when the
        // base URL is set. Address as `vllm/<served-model>` / `sglang/<served-model>`.
        // `<NAME>_WIRE=responses` speaks the Responses API to a server that
        // serves it; anything else is Chat Completions.
        for name in ["vllm", "sglang"] {
            if let Some(provider) = host_configuration
                .then(|| self_hosted_from_env(name))
                .flatten()
            {
                router = router.register(name, Box::new(provider));
            }
        }

        router
    }

    /// Refresh the shared model catalog from the network, detached from every
    /// request: only local policy is read synchronously at startup, no snapshot
    /// is built before a model is looked up, and no catalog HTTP fetch is ever
    /// awaited by a model request.
    ///
    /// The refresh rides the caller's Tokio runtime and never creates one, so
    /// `None` means nothing was scheduled: there is no runtime on this thread,
    /// `LLMSHIM_CATALOG_OFFLINE=1` is set, or the local catalog configuration
    /// is invalid. The catalog is process-wide — it is what every router in
    /// the process resolves against — so refreshing through one router
    /// refreshes it for all of them.
    pub fn refresh_catalog_in_background(
        &self,
    ) -> Option<tokio::task::JoinHandle<crate::catalog::RefreshOutcome>> {
        crate::catalog::global().ok()?.refresh_in_background()
    }

    /// Resolve model string to (provider, model_name).
    pub fn resolve(&self, model: &str) -> Result<(&dyn Provider, String)> {
        let (key, model) = self.resolve_key(model)?;
        Ok((self.get(&key)?, model))
    }

    /// The routing id a request addresses once routes and aliases are resolved:
    /// `provider/model` in the router's canonical spelling (the id `GET /v1/models`
    /// lists). What a per-key model allowlist is matched against (note 0028).
    #[cfg(feature = "gateway")]
    pub(crate) fn routing_id(&self, model: &str) -> Result<(String, String)> {
        self.resolve_key(model)
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
        // A deployment is resolved once, by `expand_route`, and its pick written into the request;
        // resolving the name here would pick a second time (note 0031).
        if model.starts_with(DEPLOYMENT_PREFIX) {
            return Err(ShimError::ProviderError {
                status: 400,
                body: "a deployment name is resolved when the request is prepared, not here".into(),
                retry_after: None,
            });
        }
        // A named route resolves to its model before anything else, so every
        // caller of `resolve` — including the proxy's admission control — sees
        // the real provider rather than skipping rate limiting on an
        // unrecognized string.
        let routed = self.route_target(model)?.map(|route| route.model.clone());
        let model = routed.as_deref().unwrap_or(model);
        crate::catalog::global().map_err(|error| ShimError::ProviderError {
            status: 400,
            body: format!("invalid model catalog configuration: {error}"),
            retry_after: None,
        })?;
        let requested = self.aliases.get(model).map(String::as_str).unwrap_or(model);
        // A named instance of a hosted provider (`openai-b/...`, note 0032) has a registry key that
        // is not the catalog's name for the vendor: ask the catalog about the kind, and keep the
        // instance as the provider key, so canonical spelling and aliases work for the second
        // account as they do for the first.
        if let Some((prefix, rest)) = requested.split_once('/') {
            if let Some(provider) = self.providers.get(prefix) {
                let kind = provider.name();
                if kind != prefix {
                    if let Some(m) = crate::catalog::resolve(&format!("{kind}/{rest}"))
                        .filter(|m| m.provider == kind)
                    {
                        return Ok((prefix.to_string(), m.name.clone()));
                    }
                }
            }
        }
        // Deliberately `resolve`, never `lookup_id`: a hit here replaces the
        // returned model name with the catalog's spelling (`m.name`), and
        // that name goes straight onto the wire. `lookup_id` would match a
        // suffixed OpenRouter id (`deepseek/deepseek-v4.1-flash:nitro`)
        // against its base catalog row and silently drop the `:nitro` from
        // the outbound request. A suffixed id already falls through to
        // `parse_model` below, which preserves it — exactly what we want.
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

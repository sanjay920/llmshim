//! Assembling the server application from a validated configuration.
//!
//! Nothing here reads the process environment, and nothing prints or exits:
//! `build_app` takes the configuration and a function that resolves
//! `{ env = ... }` references, and returns either the application or every
//! problem found. The caller owns the listener, the signal handling and the
//! exit code.

use super::config::{read_secret, Issue, ProviderKind, ServeConfig};
use super::keys::build_key_store;
use crate::breaker::BreakerConfig;
use crate::gateway::http::{app_with_parts, GatewayState, LocalParts};
use crate::gateway::GatewayConfig;
use crate::provider::Provider;
use crate::providers::{
    anthropic::Anthropic, cerebras::Cerebras, gemini::Gemini, openai::OpenAi,
    openai_compat::OpenAiCompatible, openrouter::OpenRouter, xai::Xai,
};
use crate::proxy::ratelimit::{InMemoryRateLimiter, ProviderLimit, RateLimitConfig};
use crate::proxy::wire::{DefaultReceiptStore, Receipts};
use crate::router::Router;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Idempotency answers are kept this long unless `state.idempotency_ttl_secs`
/// says otherwise (the gateway's existing default).
const DEFAULT_IDEMPOTENCY_TTL_SECS: u64 = 300;

/// A server ready to be bound to its listener.
pub struct Built {
    pub app: axum::Router,
    pub address: SocketAddr,
    /// Provider names, for the startup banner.
    pub providers: Vec<String>,
    pub caller_keys: usize,
    pub authenticated: bool,
}

/// Build the application. Must be called inside a Tokio runtime, because the
/// scheduler starts its lane tasks.
pub fn build_app(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Built, Vec<Issue>> {
    let mut issues = Vec::new();

    let address = match parse_address(&config.server.host, config.server.port) {
        Ok(address) => address,
        Err(message) => {
            issues.push(Issue {
                path: "server.host".into(),
                message,
            });
            SocketAddr::from(([127, 0, 0, 1], 0))
        }
    };

    let keystore = match build_key_store(config, env) {
        Ok(store) => Some(store),
        Err(found) => {
            issues.extend(found);
            None
        }
    };

    let router = build_router(config, env, &mut issues);

    // The rules that compare secrets (a caller token that is also an upstream
    // key) belong to assembly too, so a caller of `build_app` cannot skip them.
    // Run only once nothing else failed, because they repeat the missing-secret
    // issues found above.
    if issues.is_empty() {
        issues.extend(config.check_references(env));
    }

    let (Some(keystore), true) = (keystore, issues.is_empty()) else {
        return Err(issues);
    };

    // Nothing is created on disk until the configuration is known to be good,
    // and the directory is private, as the receipt store makes its own.
    let receipts_root = config
        .state
        .receipts_dir
        .clone()
        .unwrap_or_else(Receipts::default_root);
    if let Err(error) = create_private_directory(&receipts_root) {
        return Err(vec![Issue {
            path: "state.receipts_dir".into(),
            message: format!(
                "cannot create {}: {}",
                receipts_root.display(),
                error.kind()
            ),
        }]);
    }

    let mut gateway = GatewayConfig::default();
    if let Some(max) = config.limits.max_concurrency {
        gateway.max_concurrency_per_provider = max as usize;
    }
    if let Some(ms) = config.limits.queue_timeout_ms {
        gateway.max_wait = Duration::from_millis(ms);
    }
    let rate_limits = RateLimitConfig::with_limits(
        ProviderLimit {
            rpm: config.limits.rpm,
            tpm: config.limits.tpm,
        },
        config.limits.providers.iter().map(|(name, limit)| {
            (
                name.clone(),
                ProviderLimit {
                    rpm: limit.rpm,
                    tpm: limit.tpm,
                },
            )
        }),
    );
    let idempotency_ttl_secs = config
        .state
        .idempotency_ttl_secs
        .unwrap_or(DEFAULT_IDEMPOTENCY_TTL_SECS);

    let authenticated = keystore.is_enforced();
    let providers = config.providers.keys().cloned().collect();
    let state = GatewayState::from_parts(
        router,
        None,
        LocalParts {
            config: gateway,
            breaker: crate::proxy::health::build_breaker_with(BreakerConfig::default(), None),
            limiter: Arc::new(InMemoryRateLimiter::new(rate_limits.clone())),
            rate_limits,
            keystore,
            idempotency_ttl_secs,
        },
    );

    let app = app_with_parts(
        state,
        // No browser origin is trusted. Browsers are not callers of a gateway.
        crate::proxy::origin::OriginPolicy::default(),
        crate::proxy::lifetime::LogicalRequestDeadlines::from_lookup(&|_| None, "unused", "unused"),
        DefaultReceiptStore::at(receipts_root),
    );
    Ok(Built {
        app,
        address,
        providers,
        caller_keys: config.keys.len(),
        authenticated,
    })
}

/// Create `path` and any missing parents with mode 0700 (unix). An existing
/// directory is left as it is.
fn create_private_directory(path: &std::path::Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn parse_address(host: &str, port: u16) -> Result<SocketAddr, String> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<std::net::IpAddr>()
        .map(|ip| SocketAddr::new(ip, port))
        .map_err(|_| "must be an IPv4 or IPv6 address".to_string())
}

fn build_router(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
    issues: &mut Vec<Issue>,
) -> Router {
    let mut router = Router::new();
    for (name, provider) in &config.providers {
        let at = format!("providers.{name}");
        let key = match &provider.api_key {
            Some(secret) => match read_secret(secret, env) {
                Ok(value) => Some(value),
                Err(message) => {
                    issues.push(Issue {
                        path: format!("{at}.api_key"),
                        message,
                    });
                    continue;
                }
            },
            None => None,
        };
        let base_url = provider.base_url.clone();
        let built: Box<dyn Provider> = match (provider.kind, key) {
            (ProviderKind::OpenaiCompatible, key) => Box::new(OpenAiCompatible::new(
                name.clone(),
                base_url.unwrap_or_default(),
                key,
            )),
            (kind, Some(key)) => hosted(kind, key, base_url),
            // Rejected by validation; unreachable for a loaded configuration.
            (_, None) => continue,
        };
        router = router.register(name, built);
    }
    for (name, route) in &config.routes {
        router = router.route(
            name,
            crate::config::Route {
                model: route.model.clone(),
                settings: route.settings.clone(),
            },
        );
    }
    router
}

fn hosted(kind: ProviderKind, key: String, base_url: Option<String>) -> Box<dyn Provider> {
    macro_rules! on_base {
        ($provider:expr) => {{
            let provider = $provider;
            match base_url {
                Some(url) => Box::new(provider.with_base_url(url)) as Box<dyn Provider>,
                None => Box::new(provider) as Box<dyn Provider>,
            }
        }};
    }
    match kind {
        ProviderKind::Openai => on_base!(OpenAi::new(key)),
        ProviderKind::Anthropic => on_base!(Anthropic::new(key)),
        ProviderKind::Gemini => on_base!(Gemini::new(key)),
        ProviderKind::Xai => on_base!(Xai::new(key)),
        ProviderKind::Cerebras => on_base!(Cerebras::new(key)),
        ProviderKind::Openrouter => on_base!(OpenRouter::new(key)),
        // Handled by the caller; kept total so a new kind cannot be forgotten.
        ProviderKind::OpenaiCompatible => unreachable!("self-hosted servers are built by name"),
    }
}

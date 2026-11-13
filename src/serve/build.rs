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
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;

/// Idempotency answers are kept this long unless `state.idempotency_ttl_secs`
/// says otherwise (the gateway's existing default).
const DEFAULT_IDEMPOTENCY_TTL_SECS: u64 = 300;

/// A server ready to be bound to its listener.
pub struct Built {
    pub app: axum::Router,
    pub address: SocketAddr,
    /// The operational endpoints on a listener of their own, when `[admin]` is
    /// configured. The main application then answers 404 for those paths.
    pub admin: Option<(axum::Router, SocketAddr)>,
    /// Provider names, for the startup banner.
    pub providers: Vec<String>,
    pub caller_keys: usize,
    pub authenticated: bool,
    /// Whether the admin listener (when there is one) requires its token.
    pub admin_authenticated: bool,
}

/// The digest of the admin token, if one is configured, after the rules a
/// caller token must meet. `build_app` and `serve --check` share this, so a
/// token that `--check` accepts is one the server starts with.
pub fn admin_token_digest(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Vec<u8>>, Issue> {
    let Some(secret) = config.admin.as_ref().and_then(|admin| admin.token.as_ref()) else {
        return Ok(None);
    };
    let issue = |message| Issue {
        path: "admin.token".into(),
        message,
    };
    let token = read_secret(secret, env).map_err(issue)?;
    if let Some(message) = super::keys::token_problem(&token) {
        return Err(issue(message));
    }
    Ok(Some(Sha256::digest(token.as_bytes()).to_vec()))
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

    let admin_token = match admin_token_digest(config, env) {
        Ok(digest) => digest,
        Err(issue) => {
            issues.push(issue);
            None
        }
    };
    let admin_address = match &config.admin {
        Some(admin) => match parse_address(&admin.host, admin.port) {
            Ok(address) => Some(address),
            Err(message) => {
                issues.push(Issue {
                    path: "admin.host".into(),
                    message,
                });
                None
            }
        },
        None => None,
    };

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

    let admin_authenticated = admin_token.is_some();
    let admin_state = state.clone();
    let app = app_with_parts(
        state,
        // No browser origin is trusted. Browsers are not callers of a gateway.
        crate::proxy::origin::OriginPolicy::default(),
        crate::proxy::lifetime::LogicalRequestDeadlines::from_lookup(&|_| None, "unused", "unused"),
        DefaultReceiptStore::at(receipts_root),
    );
    let (app, admin) = match admin_address {
        Some(admin_address) => {
            let routes = crate::gateway::http::admin_routes(admin_state);
            let routes = match admin_token {
                Some(digest) => routes.layer(axum::middleware::from_fn(
                    move |request: Request, next: Next| {
                        let digest = digest.clone();
                        async move { require_admin_token(&digest, request, next).await }
                    },
                )),
                None => routes,
            };
            (
                app.layer(axum::middleware::from_fn(hide_admin_paths)),
                Some((routes, admin_address)),
            )
        }
        None => (app, None),
    };
    Ok(Built {
        app,
        admin,
        address,
        providers,
        caller_keys: config.keys.len(),
        authenticated,
        admin_authenticated,
    })
}

/// The main listener does not serve operational endpoints once they have a
/// listener of their own.
async fn hide_admin_paths(request: Request, next: Next) -> Response {
    if crate::gateway::http::ADMIN_PATHS.contains(&request.uri().path()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(request).await
}

/// `/metrics` and `/v1/gateway/stats` need the admin token; probes stay open.
async fn require_admin_token(expected: &[u8], request: Request, next: Next) -> Response {
    if matches!(request.uri().path(), "/health" | "/ready") {
        return next.run(request).await;
    }
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|token| !token.is_empty());
    let accepted = presented
        .map(|token| bool::from(Sha256::digest(token.as_bytes()).as_slice().ct_eq(expected)))
        .unwrap_or(false);
    if accepted {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
        )
            .into_response()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::config::Format;

    fn config(admin: &str) -> ServeConfig {
        let text = format!(
            "version = 1\n[server]\nallow_unauthenticated = true\n\
             [providers.openai]\nkind = \"openai\"\napi_key = {{ env = \"UPSTREAM\" }}\n{admin}"
        );
        ServeConfig::parse(&text, Format::Toml).unwrap()
    }

    fn with(value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |name| (name == "ADMIN").then(|| value.to_string())
    }

    #[test]
    fn the_admin_token_meets_the_rules_a_caller_token_does() {
        let admin = "[admin]\nport = 9090\ntoken = { env = \"ADMIN\" }";
        let config = config(admin);
        for (value, expected) in [
            ("short", "at least 16 characters"),
            ("has a space inside it 0123", "visible ASCII"),
            ("\u{feff}admin-token-0123456789", "visible ASCII"),
        ] {
            let issue = admin_token_digest(&config, &with(value)).unwrap_err();
            assert_eq!(issue.path, "admin.token");
            assert!(issue.message.contains(expected), "{issue:?}");
            assert!(!issue.message.contains(value), "{issue:?}");
        }
        let digest = admin_token_digest(&config, &with("admin-token-0123456789"))
            .unwrap()
            .unwrap();
        assert_eq!(digest, Sha256::digest(b"admin-token-0123456789").to_vec());
        let unset = admin_token_digest(&config, &|_| None).unwrap_err();
        assert!(unset.message.contains("ADMIN is not set"), "{unset:?}");
    }

    #[test]
    fn no_token_configured_is_not_an_error_here() {
        let config = config("[admin]\nport = 9090\nallow_unauthenticated = true");
        assert_eq!(admin_token_digest(&config, &|_| None).unwrap(), None);
    }
}

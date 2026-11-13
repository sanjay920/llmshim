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
    /// Whether this replica shares state with a fleet through Redis.
    pub fleet: bool,
    /// Provider names, for the startup banner.
    pub providers: Vec<String>,
    pub caller_keys: usize,
    pub authenticated: bool,
}

/// Build the application. Must be called inside a Tokio runtime, because the
/// scheduler starts its lane tasks.
pub async fn build_app(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Built, Vec<Issue>> {
    let mut issues = config.feature_issues();

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

    let redis_url = match &config.fleet {
        Some(fleet) => match read_secret(&fleet.redis_url, env) {
            Ok(url) if super::config::is_redis_url(&url) => Some(url),
            Ok(_) => {
                issues.push(Issue {
                    path: "fleet.redis_url".into(),
                    message: "must start with redis:// or rediss://".into(),
                });
                None
            }
            Err(message) => {
                issues.push(Issue {
                    path: "fleet.redis_url".into(),
                    message,
                });
                None
            }
        },
        None => None,
    };

    let receipts_root = config
        .state
        .receipts_dir
        .clone()
        .unwrap_or_else(Receipts::default_root);
    if let Err(error) = std::fs::create_dir_all(&receipts_root) {
        issues.push(Issue {
            path: "state.receipts_dir".into(),
            message: format!(
                "cannot create {}: {}",
                receipts_root.display(),
                error.kind()
            ),
        });
    }

    // The log is opened now so a path that cannot be written fails the start.
    let access_log = match config
        .logging
        .access_log
        .as_deref()
        .map(super::access_log::AccessLog::open)
    {
        Some(Ok(log)) => Some(std::sync::Arc::new(log)),
        Some(Err(message)) => {
            issues.push(Issue {
                path: "logging.access_log".into(),
                message,
            });
            None
        }
        None => None,
    };

    let admin_token = match config.admin.as_ref().and_then(|admin| admin.token.as_ref()) {
        Some(secret) => match read_secret(secret, env) {
            Ok(token) if token.chars().any(char::is_whitespace) => {
                issues.push(Issue {
                    path: "admin.token".into(),
                    message: "must not contain whitespace".into(),
                });
                None
            }
            Ok(token) if token.chars().count() < super::keys::MIN_TOKEN_LEN => {
                issues.push(Issue {
                    path: "admin.token".into(),
                    message: format!("must be at least {} characters", super::keys::MIN_TOKEN_LEN),
                });
                None
            }
            Ok(token) => Some(Sha256::digest(token.as_bytes()).to_vec()),
            Err(message) => {
                issues.push(Issue {
                    path: "admin.token".into(),
                    message,
                });
                None
            }
        },
        None => None,
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

    let (Some(keystore), true) = (keystore, issues.is_empty()) else {
        return Err(issues);
    };

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
    let retained = crate::gateway::attempt::default_retained_accounting_attempts();
    let breaker_config = BreakerConfig::default();
    let fleet = redis_url.is_some();
    let state = match redis_url {
        None => GatewayState::from_parts(
            router,
            None,
            LocalParts {
                config: gateway,
                breaker: crate::proxy::health::build_breaker_with(breaker_config, None),
                limiter: Arc::new(InMemoryRateLimiter::new(rate_limits.clone())),
                rate_limits,
                keystore,
                idempotency_ttl_secs,
                max_retained_accounting_attempts: retained,
            },
        ),
        Some(url) => match fleet_state(
            router,
            &url,
            gateway,
            rate_limits,
            keystore,
            idempotency_ttl_secs,
            retained,
        )
        .await
        {
            Ok(state) => state,
            Err(message) => {
                return Err(vec![Issue {
                    path: "fleet.redis_url".into(),
                    message,
                }])
            }
        },
    };

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
    // Outermost, so the log sees every outcome, including a request refused
    // before it reaches a handler and the 404 for a hidden admin path. It
    // resolves the caller with a store of its own built from the same file.
    let app = match access_log {
        Some(log) => match build_key_store(config, env) {
            Ok(keys) => {
                let keys = Arc::new(keys);
                app.layer(axum::middleware::from_fn(
                    move |request: Request, next: Next| {
                        super::access_log::record_request(log.clone(), keys.clone(), request, next)
                    },
                ))
            }
            Err(issues) => return Err(issues),
        },
        None => app,
    };
    Ok(Built {
        app,
        admin,
        address,
        providers,
        fleet,
        caller_keys: config.keys.len(),
        authenticated,
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

/// Build the fleet (Redis-backed) state. Without the `serve-redis` feature this
/// cannot be reached: `feature_issues` has already refused the configuration.
#[cfg(feature = "serve-redis")]
async fn fleet_state(
    router: Router,
    url: &str,
    gateway: GatewayConfig,
    rate_limits: RateLimitConfig,
    keystore: crate::gateway::auth::KeyStore,
    idempotency_ttl_secs: u64,
    max_retained_accounting_attempts: usize,
) -> Result<Arc<GatewayState>, String> {
    use crate::gateway::distributed::Tuning;
    use crate::gateway::http::DistributedParts;
    use crate::proxy::ratelimit::build_limiter_with;
    let parts = DistributedParts {
        tuning: Tuning::defaults(&gateway),
        breaker: crate::proxy::health::build_breaker_with(BreakerConfig::default(), Some(url)),
        limiter: build_limiter_with(rate_limits.clone(), Some(url)),
        config: gateway,
        rate_limits,
        keystore,
        idempotency_ttl_secs,
        max_retained_accounting_attempts,
    };
    GatewayState::distributed_from_parts(router, None, url, parts)
        .await
        .map_err(|error| format!("cannot start fleet mode: {}", redact(&error, url)))
}

#[cfg(not(feature = "serve-redis"))]
async fn fleet_state(
    _router: Router,
    _url: &str,
    _gateway: GatewayConfig,
    _rate_limits: RateLimitConfig,
    _keystore: crate::gateway::auth::KeyStore,
    _idempotency_ttl_secs: u64,
    _max_retained_accounting_attempts: usize,
) -> Result<Arc<GatewayState>, String> {
    Err("fleet mode needs a binary built with --features serve-redis".into())
}

/// Remove the Redis URL, and any password inside it, from an error message.
#[cfg_attr(not(feature = "serve-redis"), allow(dead_code))]
fn redact(message: &str, url: &str) -> String {
    let mut out = message.replace(url, "<redis_url>");
    if let Some(password) = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('@'))
        .and_then(|(userinfo, _)| userinfo.split_once(':'))
        .map(|(_, password)| password)
        .filter(|password| !password.is_empty())
    {
        out = out.replace(password, "<password>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn errors_never_carry_the_redis_url_or_its_password() {
        let url = "redis://user:s3cretpass@cache.internal:6379/0";
        let message = format!("failed to connect to {url}: bad auth s3cretpass");
        let out = redact(&message, url);
        assert!(
            !out.contains("s3cretpass") && !out.contains("cache.internal"),
            "{out}"
        );
        assert_eq!(redact("Connection refused", url), "Connection refused");
    }
}

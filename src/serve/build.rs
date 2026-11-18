//! Assembling the server application from a validated configuration.
//!
//! Nothing here reads the process environment, and nothing prints or exits:
//! `build_app` takes the configuration and a function that resolves
//! `{ env = ... }` references, and returns either the application or every
//! problem found. The caller owns the listener, the signal handling and the
//! exit code.

use super::config::{read_secret, Issue, ProviderKind, ServeConfig};
use super::keys::build_key_store;
use super::reload::ReloadHandles;
use crate::breaker::BreakerConfig;
use crate::gateway::auth::{KeyStore, LiveKeys};
use crate::gateway::http::{app_with_parts, GatewayState, LocalParts};
use crate::gateway::usage::{KeyNames, TenantNames};
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
/// The back-off after an upstream 429 when the file sets none (the same 5 s the
/// gateway defaults to).
const DEFAULT_PENALTY_SECS: u64 = 5;
/// How long a request waits for the receipt directory's lock when the file sets
/// none (the gateway's own default).
const DEFAULT_RECEIPT_LOCK_TIMEOUT_MS: u64 = 2000;

/// A server ready to be bound to its listener.
pub struct Built {
    pub app: axum::Router,
    pub address: SocketAddr,
    /// The operational endpoints on a listener of their own, when `[admin]` is
    /// configured. The main application then answers 404 for those paths.
    pub admin: Option<(axum::Router, SocketAddr)>,
    /// Whether this replica shares state with a fleet through Redis.
    pub fleet: bool,
    /// The fleet state whose queue workers are not running yet. They lease jobs
    /// from peers, so they start only after both listeners are bound
    /// ([`start_fleet_workers`]); until then a failure to start leaves nothing
    /// leased.
    pub unstarted_fleet: Option<Arc<GatewayState>>,
    /// Provider names, for the startup banner.
    pub providers: Vec<String>,
    pub caller_keys: usize,
    pub authenticated: bool,
    /// Whether the admin listener (when there is one) requires its token.
    pub admin_authenticated: bool,
    /// The log files this server writes, to be closed at shutdown.
    pub logs: Logs,
    /// What a [`super::Reloader`] replaces while the server runs.
    pub reload: ReloadHandles,
    /// The inbound limits the listeners are started with (0021).
    pub listener: ListenerLimits,
}

/// The limits of the inbound listeners, from `[server]` (0021).
#[derive(Debug, Clone, Copy)]
pub struct ListenerLimits {
    pub max_connections: usize,
    pub header_timeout: Duration,
}

impl ListenerLimits {
    /// The defaults the listener has always had.
    pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;
    pub const DEFAULT_HEADER_TIMEOUT_MS: u64 = 15_000;
}

/// The log destinations the server writes. Each queues lines for a writer
/// thread, so [`Logs::close`] must run at shutdown or lines still queued are
/// lost.
pub struct Logs(
    Vec<super::jsonl::JsonLines>,
    Option<Arc<super::telemetry::Telemetry>>,
);

impl Logs {
    /// Write everything queued and stop the writers (and send the spans still
    /// queued, within a bound). Call once, after the listeners have stopped.
    pub fn close(&self) {
        for log in &self.0 {
            log.close();
        }
        if let Some(telemetry) = &self.1 {
            telemetry.shutdown();
        }
    }
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

/// Start the queue workers of a fleet replica (`Built::unstarted_fleet`, taken
/// out of the `Built`). Call once, after every listener is bound.
pub fn start_fleet_workers(unstarted: Option<Arc<GatewayState>>) {
    #[cfg(feature = "serve-redis")]
    if let Some(state) = unstarted {
        state.start_workers();
    }
    #[cfg(not(feature = "serve-redis"))]
    drop(unstarted);
}

/// The fleet's Redis URL, if `[fleet]` is configured, after the check that it
/// is a Redis URL. Shared by `build_app` and `serve --check`, so a URL that
/// `--check` accepts is one the server can start with. The error never carries
/// the URL.
pub fn fleet_redis_url(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>, Issue> {
    let Some(fleet) = &config.fleet else {
        return Ok(None);
    };
    let issue = |message: String| Issue {
        path: "fleet.redis_url".into(),
        message,
    };
    let url = read_secret(&fleet.redis_url, env).map_err(issue)?;
    if !super::config::is_redis_url(&url) {
        return Err(issue("must start with redis:// or rediss://".into()));
    }
    Ok(Some(url))
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

    // One key store, shared by the gateway, the access log and the certificate
    // binding, so a reload replaces what all three consult in a single step.
    let keys = match build_key_store(config, env) {
        Ok(store) => Some(LiveKeys::new(store)),
        Err(found) => {
            issues.extend(found);
            None
        }
    };
    let tenants = TenantNames::new(config.keys.iter().map(|key| key.tenant.clone()));
    let key_names = KeyNames::new(super::keys::key_names(config, env));
    key_names.add_tags(super::keys::key_tags(config, env));

    let router = build_router(config, env, &mut issues);

    let redis_url = match fleet_redis_url(config, env) {
        Ok(url) => url,
        Err(issue) => {
            issues.push(issue);
            None
        }
    };

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

    let (Some(keys), true) = (keys, issues.is_empty()) else {
        return Err(issues);
    };
    let keystore = KeyStore::Live(keys.clone());

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

    // The logs are opened last, with the receipts directory, so a configuration
    // that fails earlier creates no file; a path that cannot be written fails
    // the start.
    let mut logs = Vec::new();
    let access_log = match config
        .logging
        .access_log
        .as_deref()
        .map(|target| super::jsonl::JsonLines::open(target, "access"))
    {
        Some(Ok(log)) => {
            logs.push(log.clone());
            Some(std::sync::Arc::new(log))
        }
        Some(Err(message)) => {
            return Err(vec![Issue {
                path: "logging.access_log".into(),
                message,
            }]);
        }
        None => None,
    };

    // Started with the logs, after the configuration is accepted: it spawns a
    // thread, so a configuration that fails earlier starts nothing.
    let telemetry = match &config.telemetry {
        Some(table) => super::telemetry::start(table, env).map_err(|issue| vec![issue])?,
        None => None,
    };

    // Opened with the access log, after the configuration is accepted. Tenants
    // are named from this configuration, because the policy scope that crosses
    // Redis in a fleet carries only a hash of the tenant.
    let usage_log = match config
        .logging
        .usage_log
        .as_deref()
        .map(|target| super::jsonl::JsonLines::open(target, "usage"))
    {
        Some(Ok(log)) => {
            logs.push(log.clone());
            Some(log)
        }
        Some(Err(message)) => {
            return Err(vec![Issue {
                path: "logging.usage_log".into(),
                message,
            }]);
        }
        None => None,
    };
    // Attempt spans ride the usage recorder, so it exists when either is wanted.
    let usage = if usage_log.is_some() || telemetry.is_some() {
        // An open server (`allow_unauthenticated`) attributes every request to
        // the tenant "anonymous", which no key names.
        if !keystore.is_enforced() {
            tenants.add(["anonymous".to_owned()]);
        }
        let sink: Arc<dyn crate::gateway::usage::UsageSink> = match usage_log {
            Some(log) => Arc::new(UsageLog(log)),
            None => Arc::new(NoUsageLog),
        };
        Some(crate::gateway::usage::UsageSetup {
            sink,
            tenants: tenants.clone(),
            keys: key_names.clone(),
            spans: telemetry
                .clone()
                .map(|t| t as Arc<dyn crate::gateway::usage::AttemptSpans>),
        })
    } else {
        None
    };

    let mut gateway = GatewayConfig::default();
    if let Some(max) = config.limits.max_concurrency {
        gateway.max_concurrency_per_provider = max as usize;
    }
    if let Some(ms) = config.limits.queue_timeout_ms {
        gateway.max_wait = Duration::from_millis(ms);
    }
    let rate_limits = rate_limit_config(config);
    let idempotency_ttl_secs = config
        .state
        .idempotency_ttl_secs
        .unwrap_or(DEFAULT_IDEMPOTENCY_TTL_SECS);

    // Always set: `serve` never falls back to `LLMSHIM_PENALTY_SECS`.
    let penalty = Some(Duration::from_secs(
        config.limits.penalty_secs.unwrap_or(DEFAULT_PENALTY_SECS),
    ));
    let authenticated = keystore.is_enforced();
    let providers = config.providers.keys().cloned().collect();
    let retained = crate::gateway::attempt::default_retained_accounting_attempts();
    let breaker_config = BreakerConfig::default();
    let fleet = redis_url.is_some();
    let mut unstarted_fleet = None;
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
                usage,
                // A reload can re-limit a tenant, and the bucket it already has
                // must follow. Validation requires the keys of one tenant to
                // agree on their limits, so no two of them can pull one bucket
                // in different directions.
                retune_tenant_limits: true,
                penalty,
            },
        ),
        Some(url) => match fleet_state(
            router,
            &url,
            FleetInputs {
                gateway,
                rate_limits,
                keystore,
                idempotency_ttl_secs,
                max_retained_accounting_attempts: retained,
                usage,
                penalty,
                outage: config.fleet.as_ref().and_then(|fleet| {
                    fleet.outage_admission.as_ref().map(|outage| {
                        (
                            outage.spend_lease_fraction,
                            outage.rate_fraction,
                            outage.max_concurrency as usize,
                            outage.recover_after_secs,
                        )
                    })
                }),
            },
        )
        .await
        {
            Ok(state) => {
                unstarted_fleet = Some(state.clone());
                state
            }
            Err(message) => {
                return Err(vec![Issue {
                    path: "fleet.redis_url".into(),
                    message,
                }])
            }
        },
    };

    // Guardrails (note 0040): compiled once, here; a change in the file is refused by a reload.
    if !config.guardrails.is_empty() {
        match super::guardrails::Guardrails::build(&config.guardrails) {
            Ok(guard) => state.set_guardrails(Arc::new(guard)),
            Err(message) => {
                return Err(vec![Issue {
                    path: "guardrails".into(),
                    message,
                }])
            }
        }
    }

    let admin_authenticated = admin_token.is_some();
    let admin_state = state.clone();
    let allow = super::allowlist::AllowTable::new(super::keys::allowlists(config, env));
    let trusted: Arc<[super::allowlist::Cidr]> = config
        .server
        .trusted_proxies
        .iter()
        .filter_map(|text| super::allowlist::Cidr::parse(text).ok())
        .collect::<Vec<_>>()
        .into();
    let clock = config
        .control_plane
        .as_ref()
        .map(super::snapshot::SnapshotClock::new);
    let reload = ReloadHandles::new(
        keys.clone(),
        tenants,
        key_names.clone(),
        allow.clone(),
        state.router_handle(),
        {
            let state = state.clone();
            Arc::new(move |limits| state.apply_limits(limits))
        },
        fleet.then(|| {
            let state = state.clone();
            Arc::new(move || state.sync_fleet_workers()) as Arc<dyn Fn() + Send + Sync>
        }),
        clock.clone(),
    );
    let reload_status = reload.status_handle();
    // Workload tokens (0017): built after the configuration is accepted, so a
    // file that fails earlier fetches nothing. The keys are fetched lazily, on the
    // first token, never at startup.
    let jwt = super::jwt_auth::JwtAuth::new(config, env, Arc::new(KeyStore::Live(keys.clone())))?;
    let jwks_status = jwt.clone();
    let app = app_with_parts(
        state,
        // No browser origin is trusted. Browsers are not callers of a gateway.
        crate::proxy::origin::OriginPolicy::default(),
        crate::proxy::lifetime::LogicalRequestDeadlines::from_lookup(&|_| None, "unused", "unused"),
        DefaultReceiptStore::at(
            receipts_root,
            Duration::from_millis(
                config
                    .state
                    .receipt_lock_timeout_ms
                    .unwrap_or(DEFAULT_RECEIPT_LOCK_TIMEOUT_MS),
            ),
        ),
    );
    // Inside the access log (which then records the 403 with the key's name) and
    // outside the gateway, so a refused caller reaches no cached replay, receipt,
    // queue or budget.
    let app = app.layer(axum::middleware::from_fn(
        move |request: Request, next: Next| {
            super::allowlist::enforce(allow.clone(), trusted.clone(), request, next)
        },
    ));
    // With `refuse_when_stale` and a snapshot older than `max_age_secs`: new inference
    // requests are refused; the operational endpoints keep answering (0024).
    let app = match (&clock, config.control_plane.as_ref()) {
        (Some(clock), Some(control_plane)) if control_plane.refuse_when_stale => {
            let clock = clock.clone();
            app.layer(axum::middleware::from_fn(
                move |request: Request, next: Next| {
                    let clock = clock.clone();
                    async move { stale_gate(clock, request, next).await }
                },
            ))
        }
        _ => app,
    };
    let status_clock = clock.clone();
    let (app, admin) = match admin_address {
        Some(admin_address) => {
            // The admin routes never read the key store, but the internal header
            // is stripped here too, so it is never present on any listener.
            let routes = crate::gateway::http::admin_routes(admin_state)
                .route(
                    "/v1/serve/config",
                    axum::routing::get(move || {
                        let status = reload_status.snapshot();
                        let snapshot = status_clock.as_ref().map(|clock| clock.status());
                        let jwks = jwks_status.as_ref().map(|jwt| jwt.statuses());
                        async move {
                            let mut body = serde_json::to_value(status).unwrap_or_default();
                            if let (Some(object), Some(snapshot)) = (body.as_object_mut(), snapshot)
                            {
                                object.insert(
                                    "snapshot".into(),
                                    serde_json::to_value(snapshot).unwrap_or_default(),
                                );
                            }
                            if let (Some(object), Some(jwks)) = (body.as_object_mut(), jwks) {
                                object.insert(
                                    "jwks".into(),
                                    serde_json::to_value(jwks).unwrap_or_default(),
                                );
                            }
                            axum::Json(body)
                        }
                    }),
                )
                .layer(axum::middleware::from_fn(super::mtls::strip_peer_header));
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
    let app = match (access_log, telemetry.clone()) {
        (None, None) if jwt.is_none() => app,
        (log, telemetry) => {
            let keys = Arc::new(KeyStore::Live(keys.clone()));
            let names = key_names.clone();
            app.layer(axum::middleware::from_fn(
                move |request: Request, next: Next| {
                    super::access_log::record_request(
                        log.clone(),
                        telemetry.clone(),
                        keys.clone(),
                        names.clone(),
                        jwt.clone(),
                        request,
                        next,
                    )
                },
            ))
        }
    };
    // Bind the verified client certificate's name to each request, and strip the
    // internal header from everything a caller sends. Outermost, so nothing (not
    // the access log, not the gateway) sees a value the TLS layer did not set.
    // The layer runs whether or not client certificates are required: without
    // them there is no certificate to bind, so it only strips, and the defence
    // does not depend on what the configuration validation guarantees about
    // `client_san`. It asks the same live key store as the gateway which workloads
    // exist, so a reload that adds or removes one applies to connections that are
    // already open.
    let peers = Arc::new(KeyStore::Live(keys.clone()));
    let app = app.layer(axum::middleware::from_fn(
        move |request: Request, next: Next| {
            super::mtls::bind_peer_identity(peers.clone(), request, next)
        },
    ));
    Ok(Built {
        app,
        admin,
        address,
        providers,
        fleet,
        unstarted_fleet,
        caller_keys: config.keys.len(),
        authenticated,
        admin_authenticated,
        logs: Logs(logs, telemetry),
        reload,
        listener: ListenerLimits {
            max_connections: config
                .server
                .max_connections
                .map_or(ListenerLimits::DEFAULT_MAX_CONNECTIONS, |n| n as usize),
            header_timeout: Duration::from_millis(
                config
                    .server
                    .header_timeout_ms
                    .unwrap_or(ListenerLimits::DEFAULT_HEADER_TIMEOUT_MS),
            ),
        },
    })
}

/// Refuse a new inference request while the policy snapshot is stale and the file says to
/// (0024). The operational endpoints and model discovery are not inference.
async fn stale_gate(
    clock: super::snapshot::SnapshotClock,
    request: Request,
    next: Next,
) -> Response {
    if !clock.refusing()
        || crate::gateway::http::ADMIN_PATHS.contains(&request.uri().path())
        || request.uri().path() == "/v1/models"
    {
        return next.run(request).await;
    }
    let message =
        "The policy snapshot is older than control_plane.max_age_secs; retry after it is refreshed";
    let mut response = match crate::proxy::wire::native_route(request.uri().path()) {
        Some(route) => crate::proxy::wire::fail(
            route.wire,
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            message,
        ),
        None => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": {
                "code": "policy_snapshot_stale", "message": message}})),
        )
            .into_response(),
    };
    response.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        axum::http::HeaderValue::from_static("30"),
    );
    response
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
    build_router_with(config, env, issues, None)
}

/// The router a reload replaces the running one with (0022): the providers and
/// routes of `config`, sharing the running router's client (and so its
/// connection pool) and its breaker (so provider health survives a key
/// rotation). `[upstream]` is fixed at start, so the client is never rebuilt.
pub(crate) fn build_router_from(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
    issues: &mut Vec<Issue>,
    running: &Router,
) -> Router {
    build_router_with(config, env, issues, Some(running))
}

#[cfg(test)]
pub(crate) fn build_router_for_test(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
    issues: &mut Vec<Issue>,
) -> Router {
    build_router(config, env, issues)
}

/// The provider limits the file sets (`[limits] rpm`, `tpm` and
/// `[limits.providers.*]`): built at start and again by a reload (0022).
pub(crate) fn rate_limit_config(config: &ServeConfig) -> RateLimitConfig {
    RateLimitConfig::with_limits(
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
    )
}

fn build_router_with(
    config: &ServeConfig,
    env: &dyn Fn(&str) -> Option<String>,
    issues: &mut Vec<Issue>,
    running: Option<&Router>,
) -> Router {
    let mut router = Router::new();
    if let Some(running) = running {
        if !running.ambient_proxy() {
            router = router.without_ambient_proxy();
        }
        if let Some(client) = running.http_client() {
            router = router.with_http_client(client.clone());
        }
        router = router.with_breaker(running.breaker().clone());
    }
    // The router's own client, from `[upstream]` and the library's defaults, never
    // from the environment (0021).
    if running.is_none() {
        match config
            .upstream
            .settings(config.limits.max_concurrency)
            .and_then(|settings| {
                crate::client::ShimClient::from_settings(settings).map(|client| (settings, client))
            }) {
            Ok((settings, client)) => {
                if !settings.ambient_proxy {
                    router = router.without_ambient_proxy();
                }
                router = router.with_http_client(client);
            }
            Err(message) => issues.push(Issue {
                path: "upstream".into(),
                message: message.into(),
            }),
        }
    }
    let mut shared = CloudShared::default();
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
        if provider.kind.is_cloud() {
            // A transport that cannot be built is an error, never a provider left
            // out; a build without the feature was refused by `feature_issues`.
            match cloud_provider(name, provider, key.as_deref(), &mut shared) {
                Ok(built) => router = router.register(name, built),
                Err(found) => issues.extend(found),
            }
            continue;
        }
        let base_url = provider.base_url.clone();
        let built: Box<dyn Provider> = match (provider.kind, key) {
            (ProviderKind::OpenaiCompatible, key) => Box::new(OpenAiCompatible::new(
                name.clone(),
                base_url.unwrap_or_default(),
                key,
            )),
            (kind, Some(key)) => hosted(name, kind, key, base_url),
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
    for (name, deployment) in &config.deployments {
        use crate::deployment::{Deployment, Strategy, Target};
        use crate::serve::config::DeploymentStrategy;
        router = router.deployment(
            name,
            Deployment::new(
                match deployment.strategy {
                    DeploymentStrategy::RoundRobin => Strategy::RoundRobin,
                    DeploymentStrategy::Weighted => Strategy::Weighted,
                    DeploymentStrategy::Ordered => Strategy::Ordered,
                    DeploymentStrategy::Conversation => Strategy::Conversation,
                },
                deployment
                    .targets
                    .iter()
                    .map(|target| Target {
                        model: target.model.clone(),
                        weight: target.weight.unwrap_or(1),
                    })
                    .collect(),
            ),
        );
    }
    router
}

/// Build a cloud transport from its table (0020). The fields were validated with
/// the transports' own rules, so a failure here is a defect in a hand-built
/// configuration, reported by path rather than skipped. Nothing is resolved or
/// sent: credentials are loaded when a request is dispatched.
#[cfg(feature = "serve-cloud")]
fn cloud_provider(
    name: &str,
    provider: &crate::serve::config::Provider,
    key: Option<&str>,
    shared: &mut CloudShared,
) -> Result<Box<dyn Provider>, Vec<Issue>> {
    use crate::providers::{
        azure::{Azure, AzureIdentity},
        bedrock::{AwsProfile, Bedrock},
        vertex::{GoogleIdentity, Vertex},
    };
    let at = format!("providers.{name}");
    let issue = |field: &str, message: &str| {
        vec![Issue {
            path: format!("{at}.{field}"),
            message: message.to_owned(),
        }]
    };
    let missing = |field: &str| issue(field, "is required");
    match provider.kind {
        ProviderKind::Bedrock => {
            let profile = provider
                .profile
                .as_deref()
                .ok_or_else(|| missing("profile"))?;
            let region = provider
                .region
                .as_deref()
                .ok_or_else(|| missing("region"))?;
            let mut bedrock = Bedrock::new(AwsProfile::new(profile, region));
            if let (Some(config), Some(credentials)) =
                (&provider.config_file, &provider.credentials_file)
            {
                bedrock = bedrock.with_profile_files(config.clone(), credentials.clone());
            }
            Ok(Box::new(bedrock))
        }
        ProviderKind::VertexGemini | ProviderKind::VertexAnthropic => {
            // The two Vertex kinds name one identity (validation requires their
            // fields to be equal), so they share one value and with it one token
            // cache, as `docs/vertex.md` registers them.
            let identity = match &shared.vertex {
                Some(identity) => identity.clone(),
                None => {
                    let identity = GoogleIdentity::service_account(
                        provider
                            .project
                            .as_deref()
                            .ok_or_else(|| missing("project"))?,
                        provider
                            .region
                            .as_deref()
                            .ok_or_else(|| missing("region"))?,
                        provider
                            .binding
                            .as_deref()
                            .ok_or_else(|| missing("binding"))?,
                        provider
                            .service_account_file
                            .clone()
                            .ok_or_else(|| missing("service_account_file"))?,
                    );
                    shared.vertex = Some(identity.clone());
                    identity
                }
            };
            Ok(if provider.kind == ProviderKind::VertexGemini {
                Box::new(Vertex::gemini(identity))
            } else {
                Box::new(Vertex::anthropic(identity))
            })
        }
        ProviderKind::Azure => {
            let identity = AzureIdentity::api_key(
                provider
                    .binding
                    .as_deref()
                    .ok_or_else(|| missing("binding"))?,
                key.ok_or_else(|| missing("api_key"))?,
            )
            // The message names the setting, never the key or the transport's text.
            .map_err(|_| issue("api_key", "is not usable as an Azure key (no whitespace)"))?;
            let mut azure = Azure::new(
                provider
                    .endpoint
                    .as_deref()
                    .ok_or_else(|| missing("endpoint"))?,
                identity,
            )
            .map_err(|_| issue("endpoint", "is not an accepted Azure endpoint"))?;
            for (deployment, model) in &provider.deployments {
                azure = azure.with_deployment(deployment, model).map_err(|_| {
                    issue(
                        &format!("deployments.{deployment}"),
                        "is not an accepted deployment",
                    )
                })?;
            }
            Ok(Box::new(azure))
        }
        _ => unreachable!("only cloud kinds are built here"),
    }
}

/// What the cloud providers of one configuration share between them.
#[derive(Default)]
struct CloudShared {
    #[cfg(feature = "serve-cloud")]
    vertex: Option<crate::providers::vertex::GoogleIdentity>,
}

/// Without the feature `feature_issues` has already refused the configuration.
#[cfg(not(feature = "serve-cloud"))]
fn cloud_provider(
    name: &str,
    _provider: &crate::serve::config::Provider,
    _key: Option<&str>,
    _shared: &mut CloudShared,
) -> Result<Box<dyn Provider>, Vec<Issue>> {
    Err(vec![Issue {
        path: format!("providers.{name}"),
        message: "needs a binary built with --features serve-cloud".into(),
    }])
}

fn hosted(
    name: &str,
    kind: ProviderKind,
    key: String,
    base_url: Option<String>,
) -> Box<dyn Provider> {
    macro_rules! on_base {
        ($provider:expr) => {{
            // Each entry is a named instance of its kind: admitted by its own name (note 0032).
            let provider = $provider.with_instance(name);
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
        ProviderKind::OpenaiCompatible
        | ProviderKind::Bedrock
        | ProviderKind::VertexGemini
        | ProviderKind::VertexAnthropic
        | ProviderKind::Azure => unreachable!("built by the caller"),
    }
}

/// What fleet state is built from, besides the router and the Redis URL.
#[cfg_attr(not(feature = "serve-redis"), allow(dead_code))]
struct FleetInputs {
    gateway: GatewayConfig,
    rate_limits: RateLimitConfig,
    keystore: crate::gateway::auth::KeyStore,
    idempotency_ttl_secs: u64,
    max_retained_accounting_attempts: usize,
    usage: Option<crate::gateway::usage::UsageSetup>,
    penalty: Option<Duration>,
    /// `[fleet.outage_admission]`: lease fraction, rate fraction, concurrency,
    /// seconds of steady health before leaving the outage path.
    outage: Option<(f64, f64, usize, u64)>,
}

/// Build the fleet (Redis-backed) state. Without the `serve-redis` feature this
/// cannot be reached: `feature_issues` has already refused the configuration.
#[cfg(feature = "serve-redis")]
async fn fleet_state(
    router: Router,
    url: &str,
    inputs: FleetInputs,
) -> Result<Arc<GatewayState>, String> {
    use crate::gateway::distributed::Tuning;
    use crate::gateway::http::DistributedParts;
    use crate::proxy::ratelimit::build_limiter_with;
    let parts = DistributedParts {
        tuning: Tuning::defaults(&inputs.gateway),
        breaker: crate::proxy::health::build_breaker_with(BreakerConfig::default(), Some(url)),
        limiter: build_limiter_with(inputs.rate_limits.clone(), Some(url)),
        config: inputs.gateway,
        rate_limits: inputs.rate_limits,
        keystore: inputs.keystore,
        idempotency_ttl_secs: inputs.idempotency_ttl_secs,
        max_retained_accounting_attempts: inputs.max_retained_accounting_attempts,
        usage: inputs.usage,
        penalty: inputs.penalty,
        outage: inputs.outage.map(
            |(spend_lease_fraction, rate_fraction, max_concurrency, recover_after_secs)| {
                crate::gateway::http::OutageParts {
                    spend_lease_fraction,
                    rate_fraction,
                    max_concurrency,
                    recover_after_secs,
                }
            },
        ),
    };
    GatewayState::distributed_unstarted(router, None, url, parts)
        .await
        .map_err(|error| format!("cannot start fleet mode: {}", redact(&error, url)))
}

#[cfg(not(feature = "serve-redis"))]
async fn fleet_state(
    _router: Router,
    _url: &str,
    _inputs: FleetInputs,
) -> Result<Arc<GatewayState>, String> {
    Err("fleet mode needs a binary built with --features serve-redis".into())
}

/// Writes each usage record as a JSON line.
/// Attempt spans without a usage log: records are built, and go nowhere.
struct NoUsageLog;

impl crate::gateway::usage::UsageSink for NoUsageLog {
    fn record(&self, _record: &crate::gateway::usage::UsageRecord) {}
}

struct UsageLog(super::jsonl::JsonLines);

impl crate::gateway::usage::UsageSink for UsageLog {
    fn record(&self, record: &crate::gateway::usage::UsageRecord) {
        self.0.write(record);
    }
}

/// Remove the Redis URL, and any password inside it, from an error message.
#[cfg_attr(not(feature = "serve-redis"), allow(dead_code))]
fn redact(message: &str, url: &str) -> String {
    let mut out = message.replace(url, "<redis_url>");
    // Split at the LAST '@': a host never contains one, so everything before it
    // is the user info even when the password holds an unescaped '@'.
    if let Some(password) = url
        .split_once("://")
        .and_then(|(_, rest)| rest.rsplit_once('@'))
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

    #[test]
    fn a_password_with_an_at_sign_is_removed_whole() {
        let url = "redis://user:p@ss:word@cache.internal:6379/0";
        let out = redact("bad auth p@ss:word for cache.internal", url);
        assert!(!out.contains("p@ss"), "{out}");
        assert!(!out.contains(":word"), "{out}");
        assert!(out.contains("<password>"), "{out}");
    }

    #[test]
    fn the_admin_token_meets_the_rules_a_caller_token_does() {
        let admin = "[admin]\nport = 9090\ntoken = { env = \"ADMIN\" }";
        let config = config(admin);
        for (value, expected) in [
            ("short", "at least 22 characters"),
            ("has a space inside it 0123", "visible ASCII"),
            // A byte-order mark is refused earlier, by the shared secret reader (note 0033).
            ("\u{feff}admin-token-0123456789", "not a single line"),
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

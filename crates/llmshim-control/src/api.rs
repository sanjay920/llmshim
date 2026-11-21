//! The control plane's HTTP API (design note 0026): JSON over the store, one bearer token,
//! secrets only in the answers that create them.

use crate::sso::{Principal, Refusal, Role, Sso, BREAK_GLASS_ACTOR};
use crate::{Credential, Error, KeyRecord, LimitsUpdate, NewKey, Store};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Every route, `(method, path)`, in the router's own syntax. The OpenAPI document is
/// compared with this in both directions, so neither can gain a route without the other.
pub const ROUTES: &[(&str, &str)] = &[
    ("post", "/v1/orgs"),
    ("get", "/v1/orgs"),
    ("post", "/v1/orgs/{org}/teams"),
    ("get", "/v1/orgs/{org}/teams"),
    ("put", "/v1/teams/{team}/budget"),
    ("delete", "/v1/teams/{team}/budget"),
    ("post", "/v1/teams/{team}/projects"),
    ("get", "/v1/teams/{team}/projects"),
    ("post", "/v1/projects/{project}/keys"),
    ("get", "/v1/orgs/{org}/keys"),
    ("get", "/v1/keys/{key}"),
    ("post", "/v1/keys/{key}/revoke"),
    ("post", "/v1/keys/{key}/rotate"),
    ("patch", "/v1/keys/{key}/limits"),
    ("get", "/v1/whoami"),
    ("get", "/v1/history"),
    ("get", "/v1/history/verify"),
    ("get", "/v1/gateways"),
    ("post", "/v1/usage/batch"),
    ("get", "/v1/requests"),
    ("get", "/v1/requests/summary"),
    ("get", "/v1/requests/facets"),
    ("get", "/v1/requests/misses"),
    ("get", "/v1/requests/{attempt}/structure"),
    ("get", "/v1/requests/{attempt}/payload"),
    ("get", "/v1/requests/{attempt}/payload/diff"),
    ("post", "/v1/orgs/{org}/snapshots"),
    ("get", "/v1/orgs/{org}/snapshot"),
];

/// Shortest bearer token accepted, as for the data plane's callers.
pub const MIN_TOKEN_LEN: usize = 22;

/// Requests that may be waiting for, or holding, the store at once. The store is one connection behind one lock, so
/// every request is serialized; more than this many in line means the database is slower than the callers, and the
/// answer is an immediate 503 rather than a growing pile of blocked threads.
const STORE_QUEUE: usize = 16;
/// The same for usage batches. A batch waits behind at most `STORE_QUEUE` reads and `INGEST_QUEUE` other batches.
const INGEST_QUEUE: usize = 64;
/// The longest one request waits for its turn before it is told the control plane is busy.
const STORE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct AppState {
    store: Mutex<Store>,
    /// Admission to the store for everything but the gateways' usage batches: `STORE_QUEUE` permits, held until the
    /// work is done.
    gate: Arc<tokio::sync::Semaphore>,
    /// The gateways' usage batches have a line of their own, so a burst of dashboard reads cannot take their turn.
    ingest_gate: Arc<tokio::sync::Semaphore>,
    store_wait: std::time::Duration,
    /// The shared token's digest; `None` when SSO is on and break-glass is off.
    token_digest: Option<[u8; 32]>,
    signing_key_file: Option<PathBuf>,
    /// The identity provider (note 0049); `None` keeps the one-token behaviour exactly.
    sso: Option<Sso>,
    /// The digest of the gateways' ingest token (note 0051), set once. Only `POST /v1/usage/batch`
    /// accepts it, and nothing else accepts the shared token or a person's token there.
    ingest_digest: std::sync::OnceLock<[u8; 32]>,
    /// The static admin pages and what they are told (note 0051, slice 2).
    ui: crate::ui::Ui,
    /// Set while a history chain check runs: it reads the whole history under the store's lock, so one
    /// at a time (a second asks to try again) and only for the admin role.
    verifying: std::sync::atomic::AtomicBool,
    /// The gateways whose status the UI shows (note 0051, slice 4); set once, at start.
    gateways: std::sync::OnceLock<crate::gateways::Gateways>,
    /// Whether a payload may be opened by a caller whose identity was not verified (one shared token, break-glass): off
    /// unless the operator says so. A payload view is recorded under the actor's name, and with a shared token that name
    /// is a label the caller typed.
    allow_unverified_payloads: std::sync::atomic::AtomicBool,
}

/// Holds the flag set; clears it when dropped, however the check ends.
struct Claim<'a>(&'a std::sync::atomic::AtomicBool);

impl<'a> Claim<'a> {
    fn take(flag: &'a std::sync::atomic::AtomicBool) -> Option<Self> {
        use std::sync::atomic::Ordering;
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

impl AppState {
    /// `token` must meet [`token_problem`].
    pub fn new(store: Store, token: &str, signing_key_file: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            store: Mutex::new(store),
            gate: Arc::new(tokio::sync::Semaphore::new(STORE_QUEUE)),
            ingest_gate: Arc::new(tokio::sync::Semaphore::new(INGEST_QUEUE)),
            store_wait: STORE_WAIT,
            token_digest: Some(Sha256::digest(token.as_bytes()).into()),
            signing_key_file,
            sso: None,
            ingest_digest: Default::default(),
            ui: crate::ui::Ui::new(None, true),
            verifying: Default::default(),
            gateways: Default::default(),
            allow_unverified_payloads: Default::default(),
        })
    }

    /// With single sign-on (note 0049). `token` is the break-glass token: used only if the issuer
    /// file leaves `break_glass` on, and then as an admin whose actor is `break-glass`.
    pub fn with_sso(
        store: Store,
        token: Option<&str>,
        signing_key_file: Option<PathBuf>,
        sso: Sso,
    ) -> Arc<Self> {
        let token = token.filter(|_| sso.break_glass());
        let ui = crate::ui::Ui::new(sso.ui(), sso.break_glass());
        Arc::new(Self {
            store: Mutex::new(store),
            gate: Arc::new(tokio::sync::Semaphore::new(STORE_QUEUE)),
            ingest_gate: Arc::new(tokio::sync::Semaphore::new(INGEST_QUEUE)),
            store_wait: STORE_WAIT,
            token_digest: token.map(|token| Sha256::digest(token.as_bytes()).into()),
            signing_key_file,
            sso: Some(sso),
            ingest_digest: Default::default(),
            ui,
            verifying: Default::default(),
            gateways: Default::default(),
            allow_unverified_payloads: Default::default(),
        })
    }
}

impl AppState {
    /// Let a caller whose identity is not verified (a shared token, break-glass) open payloads. Off by default.
    pub fn allow_unverified_payload_views(&self, allow: bool) {
        self.allow_unverified_payloads
            .store(allow, std::sync::atomic::Ordering::Relaxed);
    }

    /// Show these gateways' status at `GET /v1/gateways`. Set once; a second call is ignored.
    pub fn set_gateways(&self, gateways: crate::gateways::Gateways) {
        let _ = self.gateways.set(gateways);
    }

    /// Accept this token (at least 22 visible characters) for the gateways' usage push, and for nothing
    /// else. Set once; a second call is ignored.
    pub fn set_ingest_token(&self, token: &str) {
        let _ = self
            .ingest_digest
            .set(Sha256::digest(token.as_bytes()).into());
    }

    /// Delete the request ledger's rows older than `before_ms` (the retention).
    pub fn purge_requests(&self, before_ms: i64) -> crate::Result<u64> {
        self.store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .purge_requests(before_ms)
    }
}

/// The role each route needs under single sign-on (note 0049); `None` for the one route any
/// authenticated caller may use (`whoami`: how an operator whose provider's mapping produced no role
/// can see that). A `(method, path)` that is not here
/// needs `Admin`, so a route added without a line is closed, not open; a test requires a line for
/// every entry of [`ROUTES`].
pub const ROLES: &[(&str, &str, Option<Role>)] = &[
    ("post", "/v1/orgs", Some(Role::Admin)),
    ("get", "/v1/orgs", Some(Role::Read)),
    ("post", "/v1/orgs/{org}/teams", Some(Role::Admin)),
    ("get", "/v1/orgs/{org}/teams", Some(Role::Read)),
    ("put", "/v1/teams/{team}/budget", Some(Role::Admin)),
    ("delete", "/v1/teams/{team}/budget", Some(Role::Admin)),
    ("post", "/v1/teams/{team}/projects", Some(Role::Admin)),
    ("get", "/v1/teams/{team}/projects", Some(Role::Read)),
    ("post", "/v1/projects/{project}/keys", Some(Role::KeyIssuer)),
    ("get", "/v1/orgs/{org}/keys", Some(Role::Read)),
    ("get", "/v1/keys/{key}", Some(Role::Read)),
    ("post", "/v1/keys/{key}/revoke", Some(Role::KeyIssuer)),
    ("post", "/v1/keys/{key}/rotate", Some(Role::KeyIssuer)),
    ("patch", "/v1/keys/{key}/limits", Some(Role::KeyIssuer)),
    ("get", "/v1/whoami", None),
    ("get", "/v1/history", Some(Role::Read)),
    ("get", "/v1/history/verify", Some(Role::Admin)),
    ("get", "/v1/gateways", Some(Role::Read)),
    ("get", "/v1/requests", Some(Role::Read)),
    ("get", "/v1/requests/summary", Some(Role::Read)),
    ("get", "/v1/requests/facets", Some(Role::Read)),
    ("get", "/v1/requests/misses", Some(Role::Read)),
    ("get", "/v1/requests/{attempt}/structure", Some(Role::Read)),
    // A captured payload is customer data (note 0052, slice 5b): admin only, and every view is recorded.
    ("get", "/v1/requests/{attempt}/payload", Some(Role::Admin)),
    // The same two payloads side by side: customer data twice over, so the same role and the same record.
    (
        "get",
        "/v1/requests/{attempt}/payload/diff",
        Some(Role::Admin),
    ),
    ("post", "/v1/orgs/{org}/snapshots", Some(Role::Admin)),
    ("get", "/v1/orgs/{org}/snapshot", Some(Role::Read)),
];

/// The routes that take the gateways' ingest token and nothing else (note 0051): they are not in
/// [`ROLES`], because no person and no role may call them.
pub const INGEST_ROUTES: &[(&str, &str)] = &[("post", "/v1/usage/batch")];

/// The path of the batch ingest, for the middleware.
const INGEST_PATH: &str = "/v1/usage/batch";

fn required_role(method: &str, path: &str) -> Option<Role> {
    // A HEAD needs what the GET does.
    let method = if method.eq_ignore_ascii_case("head") {
        "get"
    } else {
        method
    };
    ROLES
        .iter()
        .find(|(m, p, _)| m.eq_ignore_ascii_case(method) && *p == path)
        .map_or(Some(Role::Admin), |(_, _, role)| *role)
}

/// Why `token` cannot be the API token, if it cannot.
pub fn token_problem(token: &str) -> Option<String> {
    if !token.chars().all(|c| ('!'..='~').contains(&c)) {
        return Some("must contain only visible ASCII characters".into());
    }
    (token.chars().count() < MIN_TOKEN_LEN)
        .then(|| format!("must be at least {MIN_TOKEN_LEN} characters"))
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let retry = self.code.starts_with("store_");
        let mut response = (
            self.status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response();
        if retry {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        }
        response
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::NotFound(what) => Self::new(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("{what} not found"),
            ),
            Error::Invalid(message) => {
                Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
            }
            Error::Conflict(message) => Self::new(StatusCode::CONFLICT, "conflict", message),
            Error::Timeout => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "store_timeout",
                "the database statement ran past its time budget and was stopped; nothing was written",
            ),
            Error::Snapshot(detail) => {
                // The detail may name a key file's contents' shape; it stays in the log.
                eprintln!("control: snapshot signing failed: {detail}");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "the snapshot could not be signed (signing_key)",
                )
            }
            Error::Database(detail) => {
                eprintln!("control: database error: {detail}");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal error",
                )
            }
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// A JSON body, read by hand so that every malformed or unexpected one is the API's own 400
/// `invalid_request` (axum's extractor answers 422 for some).
struct Body<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> axum::extract::FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, _: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "the body could not be read",
                )
            })?;
        serde_json::from_slice(&bytes).map(Body).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("the body is not what this route takes: {error}"),
            )
        })
    }
}

async fn with_store<T: Send + 'static>(
    state: &Arc<AppState>,
    work: impl FnOnce(&mut Store) -> crate::Result<T> + Send + 'static,
) -> ApiResult<T> {
    run_in_store(state, state.gate.clone(), work).await
}

/// As [`with_store`], on the usage batches' own line.
async fn with_store_ingest<T: Send + 'static>(
    state: &Arc<AppState>,
    work: impl FnOnce(&mut Store) -> crate::Result<T> + Send + 'static,
) -> ApiResult<T> {
    run_in_store(state, state.ingest_gate.clone(), work).await
}

async fn run_in_store<T: Send + 'static>(
    state: &Arc<AppState>,
    gate: Arc<tokio::sync::Semaphore>,
    work: impl FnOnce(&mut Store) -> crate::Result<T> + Send + 'static,
) -> ApiResult<T> {
    let busy = || {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "store_busy",
            "the control plane's database is busy; retry shortly",
        )
    };
    // The whole wait (for a permit and then for the connection) is bounded in length (the permits) and in time (one
    // deadline for both), so a slow query holds up its own line and nothing more: callers past it get a 503 and a
    // Retry-After, and work whose caller has already been told so never runs.
    let deadline = std::time::Instant::now() + state.store_wait;
    let permit = tokio::time::timeout(state.store_wait, gate.acquire_owned())
        .await
        .map_err(|_| busy())?
        .map_err(|_| busy())?;
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut store = loop {
            match state.store.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(None);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        };
        work(&mut store).map_err(Some)
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    })?
    .map_err(|error| match error {
        Some(error) => ApiError::from(error),
        None => busy(),
    })
}

fn actor(headers: &HeaderMap) -> ApiResult<String> {
    match headers.get("x-llmshim-actor") {
        None => Ok("api".into()),
        Some(value) => {
            let text = value.to_str().unwrap_or("");
            if (1..=128).contains(&text.len())
                && text.chars().all(|c| ('!'..='~').contains(&c) || c == ' ')
            {
                Ok(text.to_string())
            } else {
                Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "x-llmshim-actor must be 1 to 128 visible characters",
                ))
            }
        }
    }
}

fn unauthorized() -> Response {
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

/// The shared token against what was presented, in constant time: the length of what was
/// presented does not matter.
fn matches_shared(digest: &Option<[u8; 32]>, presented: &str) -> bool {
    let Some(expected) = digest else {
        return false;
    };
    let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
    let mut difference = 0u8;
    for (a, b) in digest.iter().zip(expected.iter()) {
        difference |= a ^ b;
    }
    !presented.is_empty() && difference == 0
}

async fn authenticate(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        // The scheme is case-insensitive (RFC 7235); the token is not.
        .and_then(|value| {
            let (scheme, rest) = value.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(rest)
        })
        .map(str::trim)
        .unwrap_or("")
        .to_owned();
    // The admin pages are public static files (note 0051): a page has to load before anyone can sign in,
    // and they hold no data. Only `/ui` and what is under it; the API under `/v1` is never reached this way.
    if is_ui_path(request.uri().path()) {
        return state.ui.respond(request.method(), request.uri());
    }
    // The usage push has a credential of its own, in both modes: the ingest token, and nothing but the
    // ingest token. It cannot be used anywhere else, and no other credential works here.
    if request.uri().path() == INGEST_PATH {
        // With no ingest token configured nothing can authenticate here: 401, the same answer as a wrong
        // token, so the route says nothing to a caller without the credential.
        return match state.ingest_digest.get() {
            Some(expected) if matches_shared(&Some(*expected), &presented) => {
                next.run(request).await
            }
            _ => unauthorized(),
        };
    }
    let Some(sso) = &state.sso else {
        // One token, no identity provider: exactly as before (note 0026).
        if !matches_shared(&state.token_digest, &presented) {
            return unauthorized();
        }
        return next.run(request).await;
    };
    // Single sign-on (note 0049). The actor is derived from the credential below; whatever the
    // caller put in the header is discarded first, so it can never be read as an identity.
    request.headers_mut().remove("x-llmshim-actor");
    let principal = if crate::sso::is_token(&presented) {
        // Shaped like a token: verified as one and never compared with the shared token.
        match sso.authenticate(&presented).await {
            Ok(principal) => principal,
            Err(Refusal::Unauthorized) => return unauthorized(),
            Err(Refusal::Unavailable) => {
                let mut response = (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"error": {"code": "unavailable",
                        "message": "the identity provider's keys are unavailable"}})),
                )
                    .into_response();
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, HeaderValue::from_static("30"));
                return response;
            }
        }
    } else if matches_shared(&state.token_digest, &presented) {
        Principal {
            actor: BREAK_GLASS_ACTOR.to_owned(),
            role: Some(Role::Admin),
            via: "break_glass",
        }
    } else {
        return unauthorized();
    };
    let needed = match request.extensions().get::<axum::extract::MatchedPath>() {
        Some(path) => required_role(request.method().as_str(), path.as_str()),
        // No route matched: the fallback answers 404, which needs no role.
        None => None,
    };
    if let Some(needed) = needed {
        if principal.role.is_none_or(|role| role < needed) {
            return ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                format!("this needs the {} role", needed.name()),
            )
            .into_response();
        }
    }
    // The actor is visible ASCII by construction; if it were ever not, the request stops here
    // rather than go on to a handler that would read a caller-typed header instead.
    let Ok(actor) = HeaderValue::from_str(&principal.actor) else {
        return ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
        .into_response();
    };
    request.headers_mut().insert("x-llmshim-actor", actor);
    request.extensions_mut().insert(principal);
    next.run(request).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameBody {
    name: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum CredentialBody {
    Secret,
    ClientSan { san: String },
    Jwt { table: String, subject: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyBody {
    name: String,
    credential: Option<CredentialBody>,
    #[serde(default)]
    tier: u8,
    rpm: Option<u32>,
    tpm: Option<u32>,
    budget_usd: Option<f64>,
    budget_window_secs: Option<u64>,
    #[serde(default)]
    budget_allow_unpriced: bool,
    allowed_cidrs: Option<Vec<String>>,
    allowed_models: Option<Vec<String>>,
    #[serde(default)]
    tags: std::collections::BTreeMap<String, String>,
    expires_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsBody {
    rpm: Option<u32>,
    tpm: Option<u32>,
    budget_usd: Option<f64>,
    budget_window_secs: Option<u64>,
    expires_at: Option<String>,
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<u32>,
}

fn created(record: KeyRecord, secret: Option<String>) -> Response {
    let mut response = (
        StatusCode::CREATED,
        Json(json!({"key": record, "secret": secret})),
    )
        .into_response();
    // The only answers that carry a secret: never cached.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn create_org(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Body(b): Body<NameBody>,
) -> ApiResult<Response> {
    let actor = actor(&h)?;
    let org = with_store(&s, move |st| st.create_org(&b.name, &actor)).await?;
    Ok((StatusCode::CREATED, Json(org)).into_response())
}

async fn list_orgs(State(s): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let orgs = with_store(&s, |st| st.orgs()).await?;
    Ok(Json(json!({"orgs": orgs})))
}

async fn create_team(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path(org): Path<String>,
    Body(b): Body<NameBody>,
) -> ApiResult<Response> {
    let actor = actor(&h)?;
    let team = with_store(&s, move |st| st.create_team(&org, &b.name, &actor)).await?;
    Ok((StatusCode::CREATED, Json(team)).into_response())
}

async fn list_teams(
    State(s): State<Arc<AppState>>,
    Path(org): Path<String>,
) -> ApiResult<Json<Value>> {
    let teams = with_store(&s, move |st| st.teams_checked(&org)).await?;
    Ok(Json(json!({"teams": teams})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamBudgetBody {
    budget_usd: f64,
    budget_window_secs: Option<u64>,
    #[serde(default)]
    budget_allow_unpriced: bool,
}

async fn set_team_budget(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path(team): Path<String>,
    Body(b): Body<TeamBudgetBody>,
) -> ApiResult<Json<crate::Team>> {
    let actor = actor(&h)?;
    let budget = crate::TeamBudget {
        budget_usd: b.budget_usd,
        budget_window_secs: b.budget_window_secs,
        budget_allow_unpriced: b.budget_allow_unpriced,
    };
    let team = with_store(&s, move |st| {
        st.set_team_budget(&team, Some(budget), &actor)
    })
    .await?;
    Ok(Json(team))
}

async fn clear_team_budget(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path(team): Path<String>,
) -> ApiResult<Json<crate::Team>> {
    let actor = actor(&h)?;
    let team = with_store(&s, move |st| st.set_team_budget(&team, None, &actor)).await?;
    Ok(Json(team))
}

async fn create_project(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path(team): Path<String>,
    Body(b): Body<NameBody>,
) -> ApiResult<Response> {
    let actor = actor(&h)?;
    let project = with_store(&s, move |st| st.create_project(&team, &b.name, &actor)).await?;
    Ok((StatusCode::CREATED, Json(project)).into_response())
}

async fn list_projects(
    State(s): State<Arc<AppState>>,
    Path(team): Path<String>,
) -> ApiResult<Json<Value>> {
    let projects = with_store(&s, move |st| st.projects(&team)).await?;
    Ok(Json(json!({"projects": projects})))
}

async fn create_key(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path(project): Path<String>,
    Body(b): Body<KeyBody>,
) -> ApiResult<Response> {
    let actor = actor(&h)?;
    let credential = match b.credential {
        None | Some(CredentialBody::Secret) => Credential::Secret,
        Some(CredentialBody::ClientSan { san }) => Credential::ClientSan(san),
        Some(CredentialBody::Jwt { table, subject }) => Credential::Jwt { table, subject },
    };
    let new = NewKey {
        project_id: project,
        name: b.name,
        credential,
        tier: b.tier,
        rpm: b.rpm,
        tpm: b.tpm,
        budget_usd: b.budget_usd,
        budget_window_secs: b.budget_window_secs,
        budget_allow_unpriced: b.budget_allow_unpriced,
        allowed_cidrs: b.allowed_cidrs,
        allowed_models: b.allowed_models,
        tags: b.tags,
        expires_at: b.expires_at,
    };
    let made = with_store(&s, move |st| st.create_key(new, &actor)).await?;
    Ok(created(made.record, made.secret))
}

async fn list_keys(
    State(s): State<Arc<AppState>>,
    Path(org): Path<String>,
) -> ApiResult<Json<Value>> {
    let keys = with_store(&s, move |st| st.keys_checked(&org)).await?;
    Ok(Json(json!({"keys": keys})))
}

async fn get_key(
    State(s): State<Arc<AppState>>,
    Path(key): Path<String>,
) -> ApiResult<Json<KeyRecord>> {
    Ok(Json(with_store(&s, move |st| st.key(&key)).await?))
}

async fn revoke_key(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path(key): Path<String>,
) -> ApiResult<Json<KeyRecord>> {
    let actor = actor(&h)?;
    Ok(Json(
        with_store(&s, move |st| st.revoke_key(&key, &actor)).await?,
    ))
}

async fn rotate_key(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path(key): Path<String>,
    Body(b): Body<NameBody>,
) -> ApiResult<Response> {
    let actor = actor(&h)?;
    let made = with_store(&s, move |st| st.rotate_key(&key, &b.name, &actor)).await?;
    Ok(created(made.record, made.secret))
}

async fn update_limits(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path(key): Path<String>,
    Body(raw): Body<Value>,
) -> ApiResult<Json<KeyRecord>> {
    let actor = actor(&h)?;
    // A whole-limits update: every field is named, `null` clears one. A body that leaves one
    // out is refused, so `{"rpm": 5}` cannot silently remove a budget or an expiry.
    let object = raw.as_object().ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "the body must be an object",
        )
    })?;
    for field in [
        "rpm",
        "tpm",
        "budget_usd",
        "budget_window_secs",
        "expires_at",
    ] {
        if !object.contains_key(field) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("name every limit; send null to clear one ({field} is missing)"),
            ));
        }
    }
    let b: LimitsBody = serde_json::from_value(raw).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("the body is not what this route takes: {error}"),
        )
    })?;
    let update = LimitsUpdate {
        rpm: b.rpm,
        tpm: b.tpm,
        budget_usd: b.budget_usd,
        budget_window_secs: b.budget_window_secs,
        expires_at: b.expires_at,
    };
    Ok(Json(
        with_store(&s, move |st| st.update_key_limits(&key, update, &actor)).await?,
    ))
}

async fn whoami(
    headers: HeaderMap,
    principal: Option<axum::Extension<Principal>>,
) -> ApiResult<Json<Value>> {
    Ok(Json(match principal {
        Some(axum::Extension(principal)) => json!({
            "actor": principal.actor,
            "role": principal.role.map(Role::name),
            "via": principal.via,
        }),
        // One shared token: every holder is an admin, and the actor is the header's label.
        None => json!({"actor": actor(&headers)?, "role": "admin", "via": "shared_token"}),
    }))
}

/// The most a usage batch body may be: 500 records are a few hundred kilobytes.
const INGEST_BODY_LIMIT: usize = 2 << 20;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestBody {
    records: Vec<Value>,
}

async fn ingest_usage(
    State(s): State<Arc<AppState>>,
    Json(body): Json<IngestBody>,
) -> ApiResult<Json<Value>> {
    let counts = with_store_ingest(&s, move |st| st.ingest_requests(&body.records)).await?;
    Ok(Json(json!(counts)))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RequestParams {
    from: Option<String>,
    to: Option<String>,
    tenant: Option<String>,
    key: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    outcome: Option<String>,
    request_id: Option<String>,
    min_cost_usd: Option<f64>,
    cursor: Option<String>,
    limit: Option<u32>,
    group_by: Option<String>,
}

fn bad_param(name: &str, why: &str) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        format!("{name} {why}"),
    )
}

impl RequestParams {
    fn query(&self) -> ApiResult<crate::requests::RequestQuery> {
        let time = |name: &str, value: &Option<String>| -> ApiResult<Option<i64>> {
            value
                .as_deref()
                .map(|v| {
                    chrono::DateTime::parse_from_rfc3339(v)
                        .map(|t| t.timestamp_millis())
                        .map_err(|_| bad_param(name, "must be an RFC 3339 time"))
                })
                .transpose()
        };
        for (name, value) in [
            ("tenant", &self.tenant),
            ("key", &self.key),
            ("model", &self.model),
            ("provider", &self.provider),
            ("outcome", &self.outcome),
            ("request_id", &self.request_id),
        ] {
            if value
                .as_deref()
                .is_some_and(|v| v.is_empty() || v.len() > 256)
            {
                return Err(bad_param(name, "must be 1 to 256 characters"));
            }
        }
        if self.min_cost_usd.is_some_and(|c| !c.is_finite() || c < 0.0) {
            return Err(bad_param("min_cost_usd", "must be a non-negative number"));
        }
        let after = match &self.cursor {
            Some(cursor) => Some(
                crate::requests::decode_cursor(cursor)
                    .ok_or_else(|| bad_param("cursor", "is not one this API issued"))?,
            ),
            None => None,
        };
        let limit = self.limit.unwrap_or(100);
        if !(1..=500).contains(&limit) {
            return Err(bad_param("limit", "must be 1 to 500"));
        }
        Ok(crate::requests::RequestQuery {
            from_ms: time("from", &self.from)?,
            to_ms: time("to", &self.to)?,
            tenant: self.tenant.clone(),
            key: self.key.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            outcome: self.outcome.clone(),
            request_id: self.request_id.clone(),
            min_cost_usd: self.min_cost_usd,
            after,
            limit,
        })
    }
}

async fn list_requests(
    State(s): State<Arc<AppState>>,
    Query(params): Query<RequestParams>,
) -> ApiResult<Json<Value>> {
    let query = params.query()?;
    let page = with_store(&s, move |st| st.query_requests(&query)).await?;
    Ok(Json(json!(page)))
}

async fn requests_summary(
    State(s): State<Arc<AppState>>,
    Query(params): Query<RequestParams>,
) -> ApiResult<Json<Value>> {
    let query = params.query()?;
    let group = match params.group_by.as_deref() {
        None => None,
        Some(name) => Some(crate::requests::Group::parse(name).ok_or_else(|| {
            bad_param(
                "group_by",
                "must be hour, day, tenant, key, model or provider",
            )
        })?),
    };
    let summary = with_store(&s, move |st| st.requests_summary(&query, group)).await?;
    Ok(Json(crate::requests::summary_json(&summary)))
}

async fn requests_facets(
    State(s): State<Arc<AppState>>,
    Query(params): Query<RequestParams>,
) -> ApiResult<Json<Value>> {
    let query = params.query()?;
    let facets = with_store(&s, move |st| st.request_facets(query.from_ms)).await?;
    Ok(Json(json!(facets)))
}

/// Where an attempt's prompt first differed from the same client's request before it (note 0052): the stored
/// segment list (kinds, keyed fingerprints and sizes: no text) and the comparison. A request whose client did not
/// capture, or whose structure has aged out, is a 404 that says so.
async fn request_structure(
    State(s): State<Arc<AppState>>,
    Path(attempt): Path<String>,
) -> ApiResult<Json<Value>> {
    let found = with_store(&s, move |st| {
        let Some(row) = st.request_structure(&attempt)? else {
            return Ok(None);
        };
        let previous = st.previous_structure(&row)?;
        Ok(Some((row, previous)))
    })
    .await?;
    let Some((row, previous)) = found else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "no_structure",
            "no prompt structure is stored for this request: its client does not capture it, or it has aged out",
        ));
    };
    let when = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    };
    Ok(Json(json!({
        "attempt_id": row.attempt_id,
        "segments": row.segments,
        "previous": previous.as_ref().map(|p| json!({
            "attempt_id": p.attempt_id,
            "ts": when(p.ts_ms),
            "gap_secs": (row.ts_ms - p.ts_ms) / 1000,
            // Older than the shorter cache lifetime: the entry may have expired whatever the prompt said.
            "within_short_window": row.ts_ms - p.ts_ms <= crate::structure::SHORT_WINDOW_MS,
        })),
        "diff": previous.as_ref().map(|p| crate::structure::diff(&p.segments, &row.segments)),
    })))
}

/// A captured request and answer, opened by a gateway (note 0052, slice 5b). Customer data, so: the admin role only;
/// a row in the change history, written **before** anything is fetched, says who asked for which attempt; the
/// control plane holds only an opaque reference and cannot open it, so a gateway does, through its admin listener; the
/// answer is never cached; and nothing of it is stored, logged or put in an error here.
async fn request_payload(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    principal: Option<axum::Extension<Principal>>,
    Path(attempt): Path<String>,
) -> ApiResult<Response> {
    let actor = actor(&headers)?;
    let actor_label = actor.clone();
    // How the actor was established, for the record: a verified person, the break-glass token, or (with one shared
    // token and no identity provider) a label the caller gave.
    let via = principal.map_or("shared_token", |axum::Extension(p)| p.via);
    // Before anything is looked up or recorded: a caller whose identity is not verified is refused.
    payload_identity(&s, via)?;
    let asked = attempt.clone();
    let Some((reference, expires_ms)) =
        with_store(&s, move |st| st.payload_reference(&asked)).await?
    else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "no_payload",
            "no payload was captured for this request: its client does not capture payloads",
        ));
    };
    if expires_ms < chrono::Utc::now().timestamp_millis() {
        return Err(ApiError::new(
            StatusCode::GONE,
            "payload_expired",
            "this payload has passed its retention and is deleted",
        ));
    }
    // The record of the view comes first: a view with no row cannot exist, whatever happens next.
    let viewed = attempt.clone();
    with_store(&s, move |st| st.record_payload_view(&viewed, &actor, via)).await?;
    let Some(gateways) = s.gateways.get() else {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "gateways_not_configured",
            "payloads are opened through the gateways' admin listeners: start the control plane with --gateways-file",
        ));
    };
    match gateways
        .open_payload(&reference, false, &actor_label, via)
        .await
    {
        Ok(opened) => Ok(no_store(Json(
            json!({"attempt_id": attempt, "payload": opened["payload"]}),
        ))),
        Err(error) => Err(open_error(error)),
    }
}

/// A payload is opened only by a person the identity provider verified, so the record names who: with one shared token the
/// actor is a label the caller typed. The operator can accept that (`--allow-payload-views-without-sso`).
fn payload_identity(s: &AppState, via: &str) -> ApiResult<()> {
    if via == "oidc"
        || s.allow_unverified_payloads
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "identity_required",
        "payloads are opened only by a person signed in through the identity provider, so the record names who; \
         start the control plane with --allow-payload-views-without-sso to accept a shared token's label instead",
    ))
}

/// A response that nothing between here and the viewer may keep or reinterpret: it holds customer text.
fn no_store(body: impl IntoResponse) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Why no gateway could open a payload, as the answer a viewer gets.
fn open_error(error: crate::gateways::OpenError) -> ApiError {
    use crate::gateways::OpenError;
    match error {
        OpenError::NotFound => ApiError::new(
            StatusCode::NOT_FOUND,
            "payload_gone",
            "no gateway has this payload: it was deleted, or its write did not complete",
        ),
        OpenError::BadReference => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "payload_unreadable",
            "no gateway could open this payload: it was captured under another capture key",
        ),
        OpenError::NotConfigured | OpenError::Unavailable => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "gateways_unavailable",
            "no gateway could be reached to open this payload; try again",
        ),
    }
}

/// The longest one segment's text a diff sends: a system prompt of a few thousand words fits; more is cut and said to be.
const DIFF_TEXT_LIMIT: usize = 64 * 1024;

fn diff_segment(segment: Option<&Value>) -> Value {
    let Some(segment) = segment else {
        return Value::Null;
    };
    let text = segment.get("text").and_then(Value::as_str);
    let cut = text.is_some_and(|t| t.len() > DIFF_TEXT_LIMIT);
    let text = text.map(|t| {
        let mut end = t.len().min(DIFF_TEXT_LIMIT);
        while !t.is_char_boundary(end) {
            end -= 1;
        }
        t[..end].to_owned()
    });
    json!({"kind": segment.get("kind"), "text": text, "cut": cut})
}

/// What changed, where the cached prefix did (note 0052, slice 5c): for a request that **changed** an earlier part of
/// the prompt than the request before it (the first difference is found from the stored fingerprints), the segments
/// around that place from both requests' captured prompts, side by side. This opens **two** payloads, so it has every
/// guarantee the payload route has, twice: the admin role, a `payload.viewed` row for each attempt written **before**
/// anything is fetched, gateways that open the references, `no-store`, and nothing kept. A request that only grew,
/// repeated or shortened its prompt opens nothing and records nothing: there is nothing to show.
async fn request_payload_diff(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    principal: Option<axum::Extension<Principal>>,
    path: Path<String>,
) -> ApiResult<Response> {
    // Every answer of this route is kept by nothing between here and the viewer, whatever it holds.
    Ok(match payload_diff(state, headers, principal, path).await {
        Ok(response) => no_store(response),
        Err(error) => no_store(error),
    })
}

/// Whether an opened prompt is the one the fingerprints named: the same number of segments, the same kinds, and a
/// text of the size each fingerprint recorded (the folded tail has no text). A gateway whose walker has since changed,
/// or a reference to another document, is not shown as if it were this prompt.
fn matches_stored(opened: &[Value], stored: &[crate::structure::Segment]) -> bool {
    opened.len() == stored.len()
        && opened.iter().zip(stored).all(|(one, segment)| {
            one["kind"] == segment.kind
                && one["text"]
                    .as_str()
                    .is_none_or(|text| text.len() as u64 == segment.bytes)
        })
}

async fn payload_diff(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    principal: Option<axum::Extension<Principal>>,
    Path(attempt): Path<String>,
) -> ApiResult<Response> {
    let actor = actor(&headers)?;
    let actor_label = actor.clone();
    let via = principal.map_or("shared_token", |axum::Extension(p)| p.via);
    payload_identity(&s, via)?;
    let asked = attempt.clone();
    let found = with_store(&s, move |st| {
        let Some(current) = st.request_structure(&asked)? else {
            return Ok(None);
        };
        let previous = st.previous_structure(&current)?;
        let current_ref = st.payload_reference(&current.attempt_id)?;
        let previous_ref = match &previous {
            Some(p) => st.payload_reference(&p.attempt_id)?,
            None => None,
        };
        Ok(Some((current, previous, current_ref, previous_ref)))
    })
    .await?;
    let Some((current, previous, current_ref, previous_ref)) = found else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "no_structure",
            "no prompt structure is stored for this request: its client does not capture it, or it has aged out",
        ));
    };
    let Some(previous) = previous else {
        return Ok(
            Json(json!({"attempt_id": current.attempt_id, "relation": "no_previous"}))
                .into_response(),
        );
    };
    let diff = crate::structure::diff(&previous.segments, &current.segments);
    let base = json!({"attempt_id": current.attempt_id, "previous_attempt_id": previous.attempt_id, "relation": diff.relation});
    let Some(first) = diff
        .first_change
        .as_ref()
        .filter(|_| diff.relation == "changed")
    else {
        // Nothing changed earlier than it grew: nothing to compare, nothing opened, nothing recorded.
        return Ok(Json(base).into_response());
    };
    let now = chrono::Utc::now().timestamp_millis();
    let usable = |found: &Option<(String, i64)>| {
        found
            .as_ref()
            .filter(|(_, expires)| *expires >= now)
            .map(|(r, _)| r.clone())
    };
    let (Some(current_ref), Some(previous_ref)) = (usable(&current_ref), usable(&previous_ref))
    else {
        let mut answer = base;
        answer["unavailable"] = json!("payload_not_captured_or_expired");
        return Ok(Json(answer).into_response());
    };
    // Both views are recorded first: a view with no row cannot exist, whatever happens next.
    let (a, b) = (current.attempt_id.clone(), previous.attempt_id.clone());
    with_store(&s, move |st| {
        st.record_payload_view(&a, &actor, via)?;
        st.record_payload_view(&b, &actor, via)
    })
    .await?;
    let Some(gateways) = s.gateways.get() else {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "gateways_not_configured",
            "payloads are opened through the gateways' admin listeners: start the control plane with --gateways-file",
        ));
    };
    let (now_open, before_open) = tokio::join!(
        gateways.open_payload(&current_ref, true, &actor_label, via),
        gateways.open_payload(&previous_ref, true, &actor_label, via)
    );
    let (now_open, before_open) = (
        now_open.map_err(open_error)?,
        before_open.map_err(open_error)?,
    );
    let (now_segments, before_segments) = (
        now_open["segments"].as_array(),
        before_open["segments"].as_array(),
    );
    let (Some(now_segments), Some(before_segments)) = (now_segments, before_segments) else {
        // A request cut at the capture limit cannot be cut into segments.
        let mut answer = base;
        answer["unavailable"] = json!("request_cut");
        return Ok(no_store(Json(answer)));
    };
    // The opened prompts must be the ones the fingerprints compared, and the segment where they differ must have text.
    if !matches_stored(now_segments, &current.segments)
        || !matches_stored(before_segments, &previous.segments)
    {
        let mut answer = base;
        answer["unavailable"] = json!("segments_do_not_match");
        return Ok(no_store(Json(answer)));
    }
    let has_text = |segments: &[Value]| {
        segments
            .get(first.index)
            .is_some_and(|s| s["text"].is_string())
    };
    if !has_text(now_segments) || !has_text(before_segments) {
        let mut answer = base;
        answer["unavailable"] = json!("segment_not_shown");
        return Ok(no_store(Json(answer)));
    }
    // The segments around the first difference, from both: the context that makes a changed line readable.
    let from = first.index.saturating_sub(2);
    let to = (first.index + 2).min(
        now_segments
            .len()
            .max(before_segments.len())
            .saturating_sub(1),
    );
    let window: Vec<Value> = (from..=to)
        .map(|index| {
            let (now_one, before_one) = (
                diff_segment(now_segments.get(index)),
                diff_segment(before_segments.get(index)),
            );
            // Decided on the whole texts: two segments that first differ past the limit a text is cut at still differ.
            let changed = now_segments.get(index).map(|s| (&s["kind"], &s["text"]))
                != before_segments.get(index).map(|s| (&s["kind"], &s["text"]));
            json!({"index": index, "changed": changed, "current": now_one, "previous": before_one})
        })
        .collect();
    let mut answer = base;
    answer["first_change"] = json!(first);
    answer["window"] = json!(window);
    Ok(no_store(Json(answer)))
}

/// Across a window: how consecutive requests of each client relate, and where the changed ones first differ.
async fn requests_misses(
    State(s): State<Arc<AppState>>,
    Query(params): Query<RequestParams>,
) -> ApiResult<Json<Value>> {
    let query = params.query()?;
    let to = query
        .to_ms
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis() + 1);
    let from = query.from_ms.unwrap_or(to - 7 * 86_400_000);
    let misses = with_store(&s, move |st| st.structure_misses(from, to)).await?;
    Ok(Json(json!(misses)))
}

async fn history(
    State(s): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<Value>> {
    let limit = q.limit.unwrap_or(100);
    if !(1..=1000).contains(&limit) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "limit must be 1 to 1000",
        ));
    }
    let rows = with_store(&s, move |st| st.history_rows(limit)).await?;
    Ok(Json(json!({"history": rows})))
}

/// Check the change history's hash chain (note 0047) and say so: `ok` with how many rows it vouches for and
/// the head hash to keep elsewhere, or not `ok` with the first row that fails and why. It reads the whole
/// history once, as `audit-verify` does.
async fn history_verify(State(s): State<Arc<AppState>>) -> ApiResult<Json<Value>> {
    let Some(_claim) = Claim::take(&s.verifying) else {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "busy",
            "a history check is already running; try again when it has finished",
        ));
    };
    let checked = with_store(&s, |st| st.verify_history()).await?;
    Ok(Json(match checked {
        Ok(good) => {
            json!({"ok": true, "records": good.records, "unchained": good.unchained, "head": good.head})
        }
        Err(broken) => json!({"ok": false, "row": broken.id, "reason": broken.reason}),
    }))
}

/// Each configured gateway's status, read from its admin listener (note 0051, slice 4). With no gateways
/// file the answer says so, and is not an error: the control plane works without it.
async fn gateways(State(s): State<Arc<AppState>>) -> Json<Value> {
    match s.gateways.get() {
        Some(gateways) => Json(gateways.status().await),
        None => Json(json!({"configured": false, "gateways": []})),
    }
}

async fn publish(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path(org): Path<String>,
) -> ApiResult<Response> {
    let actor = actor(&h)?;
    let Some(file) = s.signing_key_file.clone() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "signing_key_unconfigured",
            "the server was started without --signing-key-file",
        ));
    };
    // Read now, never kept: rotating the signing key needs no restart. Off the async workers
    // (a path on a hung mount must not stall them) and refused when other users can read it.
    let seed = tokio::task::spawn_blocking(move || read_seed(&file))
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error",
            )
        })??;
    let published = with_store(&s, move |st| st.publish_snapshot(&org, &seed, &actor)).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"generation": published.generation})),
    )
        .into_response())
}

/// The signing seed, from its file. On Unix a file group- or world-readable is refused: whoever
/// can read it can forge snapshots.
fn read_seed(path: &std::path::Path) -> ApiResult<String> {
    let fail = |what: &str| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("the signing key could not be used ({what}) (signing_key)"),
        )
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|error| {
                eprintln!(
                    "control: cannot read the signing key file: {}",
                    error.kind()
                );
                fail("unreadable")
            })?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            eprintln!("control: the signing key file is readable by group or others; chmod 600 it");
            return Err(fail("its file is readable by others"));
        }
    }
    std::fs::read_to_string(path).map_err(|error| {
        eprintln!(
            "control: cannot read the signing key file: {}",
            error.kind()
        );
        fail("unreadable")
    })
}

/// Why the signing seed file cannot be used as it is, if it cannot: on Unix, readable by group
/// or others. `None` when it does not exist yet (it is judged when it is read).
pub fn seed_file_problem(path: &std::path::Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            if metadata.permissions().mode() & 0o077 != 0 {
                return Some(format!(
                    "{} is readable by group or others; make it owner-only (chmod 600)",
                    path.display()
                ));
            }
        }
    }
    let _ = path;
    None
}

async fn latest(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path(org): Path<String>,
) -> ApiResult<Response> {
    let latest = with_store(&s, move |st| {
        st.teams_checked(&org)?;
        st.latest_snapshot(&org)
    })
    .await?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no snapshot has been published",
        )
    })?;
    let etag = format!("\"{}\"", latest.generation);
    if h.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response
            .headers_mut()
            .insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
        return Ok(response);
    }
    let mut response = (StatusCode::OK, latest.envelope).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    Ok(response)
}

async fn no_route() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "no_such_route", "no such route")
}

/// `/ui` and anything under it, exactly: `/uix` and `/ui/../v1/orgs` are not the pages.
fn is_ui_path(path: &str) -> bool {
    path == "/ui" || path.starts_with("/ui/")
}

/// The API's router. Every `/v1` route sits behind the bearer token; `/ui` is the static pages.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/orgs", post(create_org).get(list_orgs))
        .route("/v1/orgs/{org}/teams", post(create_team).get(list_teams))
        .route(
            "/v1/teams/{team}/budget",
            put(set_team_budget).delete(clear_team_budget),
        )
        .route(
            "/v1/teams/{team}/projects",
            post(create_project).get(list_projects),
        )
        .route("/v1/projects/{project}/keys", post(create_key))
        .route("/v1/orgs/{org}/keys", get(list_keys))
        .route("/v1/keys/{key}", get(get_key))
        .route("/v1/keys/{key}/revoke", post(revoke_key))
        .route("/v1/keys/{key}/rotate", post(rotate_key))
        .route("/v1/keys/{key}/limits", patch(update_limits))
        .route("/v1/whoami", get(whoami))
        .route("/v1/history", get(history))
        .route("/v1/history/verify", get(history_verify))
        .route("/v1/gateways", get(gateways))
        .route(
            "/v1/usage/batch",
            post(ingest_usage).layer(axum::extract::DefaultBodyLimit::max(INGEST_BODY_LIMIT)),
        )
        .route("/v1/requests", get(list_requests))
        .route("/v1/requests/summary", get(requests_summary))
        .route("/v1/requests/facets", get(requests_facets))
        .route("/v1/requests/misses", get(requests_misses))
        .route("/v1/requests/{attempt}/structure", get(request_structure))
        .route("/v1/requests/{attempt}/payload", get(request_payload))
        .route(
            "/v1/requests/{attempt}/payload/diff",
            get(request_payload_diff),
        )
        .route("/v1/orgs/{org}/snapshots", post(publish))
        .route("/v1/orgs/{org}/snapshot", get(latest))
        .fallback(no_route)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_claim_is_single_flight_and_released_when_dropped() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        let first = super::Claim::take(&flag).expect("free");
        assert!(super::Claim::take(&flag).is_none(), "taken twice");
        drop(first);
        assert!(super::Claim::take(&flag).is_some(), "not released");
    }

    use super::*;

    #[tokio::test]
    async fn a_full_queue_for_the_store_is_a_503_with_retry_after_not_a_hang() {
        let mut state = AppState::new(
            Store::open_in_memory().unwrap(),
            "a-token-of-sufficient-length-0123",
            None,
        );
        Arc::get_mut(&mut state).unwrap().store_wait = std::time::Duration::from_millis(50);
        // Every permit is out: the next request waits its bounded time and is refused.
        let held = state
            .gate
            .clone()
            .acquire_many_owned(STORE_QUEUE as u32)
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let refused = with_store(&state, |st| st.orgs()).await.err().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(refused.code, "store_busy");
        let response = refused.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "5");
        // Released, the store answers again.
        drop(held);
        assert!(with_store(&state, |st| st.orgs()).await.is_ok());
    }

    fn quick_state() -> Arc<AppState> {
        let mut state = AppState::new(
            Store::open_in_memory().unwrap(),
            "a-token-of-sufficient-length-0123",
            None,
        );
        Arc::get_mut(&mut state).unwrap().store_wait = std::time::Duration::from_millis(80);
        state
    }

    #[tokio::test]
    async fn usage_batches_have_their_own_line_when_the_reads_are_full() {
        let state = quick_state();
        let _reads = state
            .gate
            .clone()
            .acquire_many_owned(STORE_QUEUE as u32)
            .await
            .unwrap();
        assert!(with_store(&state, |st| st.orgs()).await.is_err());
        assert!(with_store_ingest(&state, |st| st.orgs()).await.is_ok());
    }

    #[tokio::test]
    async fn work_is_not_run_when_the_connection_is_not_free_before_the_deadline() {
        let state = quick_state();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Another request holds the connection for longer than anyone will wait.
        let holder = state.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let hold = std::thread::spawn(move || {
            let _guard = holder.store.lock().unwrap();
            held_tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(400));
        });
        held_rx.recv().unwrap();
        let flag = ran.clone();
        let refused = with_store(&state, move |st| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            st.orgs()
        })
        .await
        .err()
        .unwrap();
        assert_eq!(refused.code, "store_busy");
        hold.join().unwrap();
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        // A permit is not lost to the refusal.
        assert!(with_store(&state, |st| st.orgs()).await.is_ok());
    }

    #[test]
    fn the_statement_budget_is_a_503_with_retry_after() {
        let response = ApiError::from(Error::Timeout).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "5");
    }

    #[test]
    fn a_route_with_no_line_in_the_table_needs_admin() {
        assert_eq!(
            required_role("get", "/v1/not-yet-classified"),
            Some(Role::Admin)
        );
        assert_eq!(required_role("DELETE", "/v1/orgs"), Some(Role::Admin));
        assert_eq!(required_role("GET", "/v1/orgs"), Some(Role::Read));
        // A HEAD needs what the GET does; whoami needs no role.
        assert_eq!(required_role("HEAD", "/v1/orgs"), Some(Role::Read));
        assert_eq!(required_role("get", "/v1/whoami"), None);
    }
}

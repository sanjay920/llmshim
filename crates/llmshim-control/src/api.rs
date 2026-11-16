//! The control plane's HTTP API (design note 0026): JSON over the store, one bearer token,
//! secrets only in the answers that create them.

use crate::{Credential, Error, KeyRecord, LimitsUpdate, NewKey, Store};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
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
    ("post", "/v1/teams/{team}/projects"),
    ("get", "/v1/teams/{team}/projects"),
    ("post", "/v1/projects/{project}/keys"),
    ("get", "/v1/orgs/{org}/keys"),
    ("get", "/v1/keys/{key}"),
    ("post", "/v1/keys/{key}/revoke"),
    ("post", "/v1/keys/{key}/rotate"),
    ("patch", "/v1/keys/{key}/limits"),
    ("get", "/v1/history"),
    ("post", "/v1/orgs/{org}/snapshots"),
    ("get", "/v1/orgs/{org}/snapshot"),
];

/// Shortest bearer token accepted, as for the data plane's callers.
pub const MIN_TOKEN_LEN: usize = 22;

pub struct AppState {
    store: Mutex<Store>,
    token_digest: [u8; 32],
    signing_key_file: Option<PathBuf>,
}

impl AppState {
    /// `token` must meet [`token_problem`].
    pub fn new(store: Store, token: &str, signing_key_file: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            store: Mutex::new(store),
            token_digest: Sha256::digest(token.as_bytes()).into(),
            signing_key_file,
        })
    }
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
        (
            self.status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
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
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        work(&mut store)
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    })?
    .map_err(ApiError::from)
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

async fn authenticate(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    // Digests compared in constant time: the length of what was presented does not matter.
    let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
    let mut difference = 0u8;
    for (a, b) in digest.iter().zip(state.token_digest.iter()) {
        difference |= a ^ b;
    }
    if presented.is_empty() || difference != 0 {
        let mut response = StatusCode::UNAUTHORIZED.into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }
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
    Body(b): Body<LimitsBody>,
) -> ApiResult<Json<KeyRecord>> {
    let actor = actor(&h)?;
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
    // Read now, never kept: rotating the signing key needs no restart.
    let seed = std::fs::read_to_string(&file).map_err(|error| {
        eprintln!(
            "control: cannot read the signing key file: {}",
            error.kind()
        );
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the signing key could not be read (signing_key)",
        )
    })?;
    let published = with_store(&s, move |st| st.publish_snapshot(&org, &seed, &actor)).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"generation": published.generation})),
    )
        .into_response())
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

/// The API's router. Every route sits behind the bearer token.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/orgs", post(create_org).get(list_orgs))
        .route("/v1/orgs/{org}/teams", post(create_team).get(list_teams))
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
        .route("/v1/history", get(history))
        .route("/v1/orgs/{org}/snapshots", post(publish))
        .route("/v1/orgs/{org}/snapshot", get(latest))
        .fallback(no_route)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate,
        ))
        .with_state(state)
}

//! Captured payloads through the control plane (design note 0052, slice 5b): an opaque reference stored with the
//! ledger row, never the text; one admin-only route that records the view in the change history **before** it
//! asks a gateway to open the reference; nothing cached, nothing kept. The gateway is a real loopback server.

mod common;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::routing::post;
use http_body_util::BodyExt;
use llmshim_control::api::{router, AppState, ROLES};
use llmshim_control::gateways::Gateways;
use llmshim_control::sso::Role;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

const SHARED: &str = "control-token-0123456789abcdef";
const GATEWAY_TOKEN: &str = "gateway-admin-token-0123456789";
const SECRET_TEXT: &str = "THE-CUSTOMER-PROMPT-TEXT";

#[derive(Clone)]
struct Mock {
    asked: Arc<AtomicUsize>,
}

async fn gateway() -> (String, Arc<AtomicUsize>) {
    async fn open(
        State(mock): State<Mock>,
        headers: HeaderMap,
        axum::Json(body): axum::Json<Value>,
    ) -> (StatusCode, axum::Json<Value>) {
        mock.asked.fetch_add(1, Ordering::SeqCst);
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if token != format!("Bearer {GATEWAY_TOKEN}") {
            return (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
        }
        match body["reference"].as_str() {
            Some("good_ref") => (
                StatusCode::OK,
                axum::Json(json!({"payload": {"v": 1, "wire": "openai-chat",
                    "request": {"text": SECRET_TEXT, "bytes": 24, "truncated": false},
                    "response": {"text": "pong", "bytes": 4, "truncated": false}}})),
            ),
            Some("gone_ref") => (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"error": "not_found"})),
            ),
            Some("other_key_ref") => (
                StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(json!({"error": "bad_reference"})),
            ),
            _ => (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({}))),
        }
    }
    let asked = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route("/v1/serve/payload/open", post(open))
        .with_state(Mock {
            asked: asked.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (origin, asked)
}

fn token_file() -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!("llmshim-payload-token-{}", uuid::Uuid::new_v4()));
    std::fs::write(&path, GATEWAY_TOKEN).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn other_token_file() -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!("llmshim-admin-token-{}", uuid::Uuid::new_v4()));
    std::fs::write(&path, "a-different-admin-token-0123456789").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn gateways(origin: &str) -> Gateways {
    Gateways::from_bytes(
        json!({"gateways": [{"name": "g", "admin_url": origin, "token_file": other_token_file(), "payload_token_file": token_file(), "allow_insecure_http": true}]})
            .to_string()
            .as_bytes(),
    )
    .unwrap()
}

fn record(id: &str, reference: Option<&str>, expires: &str) -> Value {
    let mut r = json!({
        "event": "usage", "ts": "2026-11-19T10:00:00Z", "attempt_id": id, "tenant": "t", "key": "k",
        "request_id": format!("req-{id}"), "provider": "openai", "model": "m", "kind": "completion",
        "outcome": "completed", "duration_ms": 10, "input_tokens": 1, "output_tokens": 1,
        "cache_read_tokens": 0, "cache_write_tokens": 0, "usage_reported": true, "admission": "fleet",
    });
    if let Some(reference) = reference {
        r["payload"] = json!({"reference": reference, "bytes": 24, "expires_at": expires});
    }
    r
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, HeaderMap, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {SHARED}"))
                .header("x-llmshim-actor", "operator@example.test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn app_with(records: &[Value], gateways: Option<Gateways>) -> axum::Router {
    let mut store = common::store();
    assert_eq!(store.ingest_requests(records).unwrap().rejected, 0);
    let state = AppState::new(store, SHARED, None);
    state.allow_unverified_payload_views(true);
    if let Some(gateways) = gateways {
        state.set_gateways(gateways);
    }
    router(state)
}

async fn views(app: &axum::Router) -> Vec<Value> {
    let (_, _, history) = get(app, "/v1/history?limit=100").await;
    history["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["action"] == "payload.viewed")
        .cloned()
        .collect()
}

#[tokio::test]
async fn an_admin_reads_a_payload_through_a_gateway_and_the_view_is_recorded() {
    let (origin, asked) = gateway().await;
    let app = app_with(
        &[record("a1", Some("good_ref"), "2099-01-01T00:00:00Z")],
        Some(gateways(&origin)),
    );
    let (status, headers, body) = get(&app, "/v1/requests/a1/payload").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["attempt_id"], "a1");
    assert_eq!(body["payload"]["request"]["text"], SECRET_TEXT);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    // The view is in the change history with who and which attempt, and no text and no reference.
    let seen = views(&app).await;
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0]["subject"], "a1");
    // The record says how far its "who" can be trusted: here a label given with the one shared token.
    assert_eq!(
        seen[0]["detail"],
        json!({"attempt_id": "a1", "via": "shared_token"})
    );
    // With the one shared token the actor is the label the caller gave; under single sign-on it is the verified
    // subject (note 0049), which the history rows of every other write already use.
    assert_eq!(seen[0]["actor"], "operator@example.test");
    let history = get(&app, "/v1/history?limit=100").await.2.to_string();
    assert!(
        !history.contains(SECRET_TEXT) && !history.contains("good_ref"),
        "{history}"
    );
    // The chain still checks out with the new rows in it.
    assert_eq!(get(&app, "/v1/history/verify").await.2["ok"], true);
}

#[tokio::test]
async fn the_view_is_recorded_before_the_gateway_is_asked_and_even_when_it_cannot_answer() {
    // No gateways file: the route cannot work, and says so, and the attempt to read was still recorded.
    let app = app_with(
        &[record("b1", Some("good_ref"), "2099-01-01T00:00:00Z")],
        None,
    );
    let (status, _, body) = get(&app, "/v1/requests/b1/payload").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_IMPLEMENTED, Some("gateways_not_configured"))
    );
    assert_eq!(views(&app).await.len(), 1);
    // A gateway that is down: 502, and recorded.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let app = app_with(
        &[record("b2", Some("good_ref"), "2099-01-01T00:00:00Z")],
        Some(gateways(&closed)),
    );
    let (status, _, body) = get(&app, "/v1/requests/b2/payload").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::BAD_GATEWAY, Some("gateways_unavailable"))
    );
    assert_eq!(views(&app).await.len(), 1);
}

#[tokio::test]
async fn a_payload_that_is_gone_unreadable_missing_or_expired_says_which_and_the_error_never_holds_the_text(
) {
    let (origin, asked) = gateway().await;
    let app = app_with(
        &[
            record("g1", Some("gone_ref"), "2099-01-01T00:00:00Z"),
            record("k1", Some("other_key_ref"), "2099-01-01T00:00:00Z"),
            record("n1", None, ""),
            record("e1", Some("good_ref"), "2020-01-01T00:00:00Z"),
        ],
        Some(gateways(&origin)),
    );
    let code = |body: &Value| body["error"]["code"].as_str().map(str::to_owned);
    let (status, _, body) = get(&app, "/v1/requests/g1/payload").await;
    assert_eq!(
        (status, code(&body).as_deref()),
        (StatusCode::NOT_FOUND, Some("payload_gone"))
    );
    let (status, _, body) = get(&app, "/v1/requests/k1/payload").await;
    assert_eq!(
        (status, code(&body).as_deref()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("payload_unreadable"))
    );
    let before = asked.load(Ordering::SeqCst);
    // Nothing was captured, or it has expired: no gateway is asked and no view is recorded (nothing was read).
    let (status, _, body) = get(&app, "/v1/requests/n1/payload").await;
    assert_eq!(
        (status, code(&body).as_deref()),
        (StatusCode::NOT_FOUND, Some("no_payload"))
    );
    let (status, _, body) = get(&app, "/v1/requests/e1/payload").await;
    assert_eq!(
        (status, code(&body).as_deref()),
        (StatusCode::GONE, Some("payload_expired"))
    );
    assert_eq!(
        get(&app, "/v1/requests/missing/payload").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(asked.load(Ordering::SeqCst), before);
    assert_eq!(
        views(&app).await.len(),
        2,
        "only the two that reached a gateway"
    );
}

#[tokio::test]
async fn the_ledger_lists_that_a_payload_exists_never_its_reference() {
    let app = app_with(
        &[
            record("l1", Some("secret_reference_value"), "2099-01-01T00:00:00Z"),
            record("l2", Some("old_reference"), "2020-01-01T00:00:00Z"),
            // A payload field that is not what a gateway sends: the row is kept, the field is not.
            record("l3", Some("not valid!"), "2099-01-01T00:00:00Z"),
        ],
        None,
    );
    let (_, _, page) = get(&app, "/v1/requests?limit=10").await;
    let text = page.to_string();
    assert!(
        !text.contains("secret_reference_value") && !text.contains("old_reference"),
        "{text}"
    );
    let rows = page["requests"].as_array().unwrap();
    let by = |id: &str| rows.iter().find(|r| r["attempt_id"] == id).unwrap();
    assert_eq!(by("l1")["payload"]["bytes"], 24);
    assert_eq!(by("l1")["payload"]["expired"], false);
    assert_eq!(by("l2")["payload"]["expired"], true);
    assert!(by("l3")["payload"].is_null());
    assert_eq!(rows.len(), 3);
}

#[test]
fn only_an_admin_may_read_a_payload() {
    let role = ROLES
        .iter()
        .find(|(m, p, _)| *m == "get" && *p == "/v1/requests/{attempt}/payload")
        .map(|(_, _, r)| *r);
    assert_eq!(role, Some(Some(Role::Admin)));
}

#[tokio::test]
async fn without_a_verified_identity_a_payload_is_refused_before_any_gateway_is_asked() {
    let (origin, asked) = gateway().await;
    let mut store = common::store();
    assert_eq!(
        store
            .ingest_requests(&[record("a1", Some("good_ref"), "2099-01-01T00:00:00Z")])
            .unwrap()
            .rejected,
        0
    );
    let state = AppState::new(store, SHARED, None);
    state.set_gateways(gateways(&origin));
    let app = router(state);
    let (status, _, body) = get(&app, "/v1/requests/a1/payload").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "identity_required", "{body}");
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert!(views(&app).await.is_empty());
}

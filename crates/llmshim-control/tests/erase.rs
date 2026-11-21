//! Deleting a tenant's or a key's request data on request, and keeping tenants apart (design note 0056). On SQLite,
//! and on Postgres when `LLMSHIM_CONTROL_TEST_POSTGRES_URL` is set.

mod common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use llmshim_control::api::{router, AppState};
use serde_json::{json, Value};
use tower::ServiceExt;

const SHARED: &str = "control-token-0123456789abcdef";

fn fingerprint(tag: &str) -> String {
    let hex: String = tag.bytes().map(|b| format!("{b:02x}")).collect();
    format!("{hex:0<16}")
}

fn structure(system: &str) -> Value {
    json!([
        {"kind": "system", "hash": fingerprint(system), "bytes": 10},
        {"kind": "user", "hash": fingerprint("u"), "bytes": 10},
    ])
}

fn record(id: &str, ts: &str, tenant: &str, key: &str, system: &str, payload: bool) -> Value {
    let mut r = json!({
        "event": "usage", "ts": ts, "attempt_id": id, "tenant": tenant, "key": key,
        "request_id": format!("req-{id}"), "provider": "openai", "model": "gpt-6-sol", "kind": "completion",
        "outcome": "completed", "duration_ms": 10, "input_tokens": 1000, "output_tokens": 5,
        "uncached_input_tokens": 100, "cache_read_tokens": 900, "cache_write_tokens": 0, "cost_usd": 0.01,
        "cost_source": "provider", "usage_reported": true, "admission": "fleet", "structure": structure(system),
    });
    if payload {
        r["payload"] = json!({"reference": format!("ref_{id}"), "bytes": 10, "expires_at": "2099-01-01T00:00:00Z"});
    }
    r
}

async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {SHARED}"))
        .header("x-llmshim-actor", "privacy@example.test");
    let body = match body {
        Some(b) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn app_with(records: &[Value]) -> axum::Router {
    let mut store = common::store();
    assert_eq!(store.ingest_requests(records).unwrap().rejected, 0);
    router(AppState::new(store, SHARED, None))
}

async fn count(app: &axum::Router) -> u64 {
    let (_, page) = call(app, Method::GET, "/v1/requests?limit=200", None).await;
    page["requests"].as_array().unwrap().len() as u64
}

#[tokio::test]
async fn two_tenants_whose_keys_share_a_name_are_never_paired() {
    // Interleaved in time, same key name, provider and model: only the tenant tells them apart.
    let app = app_with(&[
        record(
            "a1",
            "2026-11-19T10:00:00Z",
            "acme",
            "checkout",
            "s1",
            false,
        ),
        record(
            "b1",
            "2026-11-19T10:00:30Z",
            "globex",
            "checkout",
            "s9",
            false,
        ),
        record(
            "a2",
            "2026-11-19T10:01:00Z",
            "acme",
            "checkout",
            "s1",
            false,
        ),
    ]);
    let (_, a2) = call(&app, Method::GET, "/v1/requests/a2/structure", None).await;
    assert_eq!(a2["previous"]["attempt_id"], "a1", "{a2}");
    let (_, b1) = call(&app, Method::GET, "/v1/requests/b1/structure", None).await;
    assert!(
        b1["previous"].is_null(),
        "another tenant's request was its predecessor: {b1}"
    );
    // The window's aggregate does not pair across them either: one pair (a1, a2), identical.
    let (_, misses) = call(
        &app,
        Method::GET,
        "/v1/requests/misses?from=2026-11-19T00:00:00Z&to=2026-11-20T00:00:00Z",
        None,
    )
    .await;
    assert_eq!(misses["pairs"], 1, "{misses}");
    assert_eq!(misses["identical"], 1, "{misses}");
}

#[tokio::test]
async fn a_key_a_tenant_or_a_key_within_a_tenant_is_erased_with_its_structures_and_the_history_says_who(
) {
    let app = app_with(&[
        record("a1", "2026-11-19T10:00:00Z", "acme", "checkout", "s1", true),
        record(
            "a2",
            "2026-11-19T10:01:00Z",
            "acme",
            "checkout",
            "s1",
            false,
        ),
        record("a3", "2026-11-19T10:02:00Z", "acme", "search", "s1", false),
        record(
            "g1",
            "2026-11-19T10:03:00Z",
            "globex",
            "checkout",
            "s1",
            true,
        ),
    ]);
    assert_eq!(count(&app).await, 4);
    // A key within a tenant: the other tenant's key of the same name is untouched.
    let (status, erased) = call(
        &app,
        Method::POST,
        "/v1/requests/erase",
        Some(json!({"tenant": "acme", "key": "checkout", "confirm": "checkout"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{erased}");
    assert_eq!(
        erased,
        json!({"requests": 2, "structures": 2, "payload_references": 1})
    );
    assert_eq!(count(&app).await, 2);
    let (status, _) = call(&app, Method::GET, "/v1/requests/g1/structure", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the other tenant's structure must survive"
    );
    let (status, _) = call(&app, Method::GET, "/v1/requests/a1/structure", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A whole tenant.
    let (_, erased) = call(
        &app,
        Method::POST,
        "/v1/requests/erase",
        Some(json!({"tenant": "acme", "confirm": "acme"})),
    )
    .await;
    assert_eq!(erased["requests"], 1, "{erased}");
    // A key by name, in every tenant.
    let (_, erased) = call(
        &app,
        Method::POST,
        "/v1/requests/erase",
        Some(json!({"key": "checkout", "confirm": "checkout"})),
    )
    .await;
    assert_eq!(
        erased,
        json!({"requests": 1, "structures": 1, "payload_references": 1})
    );
    assert_eq!(count(&app).await, 0);
    // Erasing nothing is a count of zero, not an error, and is still recorded.
    let (status, erased) = call(
        &app,
        Method::POST,
        "/v1/requests/erase",
        Some(json!({"tenant": "nobody", "confirm": "nobody"})),
    )
    .await;
    assert_eq!(
        (status, erased["requests"].as_u64()),
        (StatusCode::OK, Some(0))
    );
    // The history has a row for each: who, how they were established, the selector and counts, no row content.
    let (_, history) = call(&app, Method::GET, "/v1/history?limit=100", None).await;
    let rows: Vec<&Value> = history["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["action"] == "requests.erased")
        .collect();
    assert_eq!(rows.len(), 4, "{history}");
    assert!(rows.iter().all(|r| r["actor"] == "privacy@example.test"));
    let first = rows.last().unwrap();
    assert_eq!(first["subject"], "acme/checkout");
    assert_eq!(first["detail"]["via"], "shared_token");
    assert_eq!(first["detail"]["requests"], 2);
    let text = history.to_string();
    assert!(
        !text.contains("ref_a1") && !text.contains("req-a1"),
        "{text}"
    );
    // The chain is still intact after deleting what it never held.
    let (_, verified) = call(&app, Method::GET, "/v1/history/verify", None).await;
    assert_eq!(verified["ok"], true, "{verified}");
}

#[tokio::test]
async fn an_erasure_needs_a_selector_and_its_value_repeated() {
    let app = app_with(&[record(
        "a1",
        "2026-11-19T10:00:00Z",
        "acme",
        "checkout",
        "s1",
        false,
    )]);
    for body in [
        json!({"confirm": "x"}),
        json!({"tenant": "acme", "confirm": "acm"}),
        json!({"tenant": "acme", "key": "checkout", "confirm": "acme"}),
        json!({"tenant": "acme", "confirm": "acme", "extra": 1}),
    ] {
        let (status, answer) =
            call(&app, Method::POST, "/v1/requests/erase", Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {answer}");
    }
    assert_eq!(count(&app).await, 1, "a refused erasure deleted nothing");
}

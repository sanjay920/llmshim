//! The content diff (design note 0052, slice 5c): for a request that changed an earlier part of its prompt than the
//! same client's request before it, the segments around the first difference from both captured prompts. It opens two
//! payloads, so it has the payload route's guarantees twice over: admin only, a recorded view of each attempt before
//! anything is fetched, `no-store`, nothing kept; and it opens nothing when there is nothing to show.

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

fn fingerprint(text: &str) -> String {
    // Any stable 64-bit hash will do for a test: FNV-1a.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// A captured prompt: a system prompt, then turns, as the gateway's `segments` view cuts it.
fn prompt(system: &str, turns: &[&str]) -> Value {
    let mut out = vec![json!({"kind": "system", "text": system})];
    out.extend(turns.iter().map(|t| json!({"kind": "user", "text": t})));
    Value::Array(out)
}

#[derive(Clone)]
struct Mock {
    asked: Arc<AtomicUsize>,
    references: Arc<std::collections::BTreeMap<String, Value>>,
}

async fn gateway(references: Vec<(&str, Value)>) -> (String, Arc<AtomicUsize>) {
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
        match mock
            .references
            .get(body["reference"].as_str().unwrap_or(""))
        {
            Some(answer) => (StatusCode::OK, axum::Json(answer.clone())),
            None => (
                StatusCode::NOT_FOUND,
                axum::Json(json!({"error": "not_found"})),
            ),
        }
    }
    let asked = Arc::new(AtomicUsize::new(0));
    let references = Arc::new(
        references
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    );
    let app = axum::Router::new()
        .route("/v1/serve/payload/open", post(open))
        .with_state(Mock {
            asked: asked.clone(),
            references,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (origin, asked)
}

fn answer(segments: Value) -> Value {
    json!({"payload": {"v": 1, "request": {"text": "{}", "bytes": 2, "truncated": false}}, "segments": segments})
}

fn gateways(origin: &str) -> Gateways {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!("llmshim-diff-token-{}", uuid::Uuid::new_v4()));
    std::fs::write(&path, GATEWAY_TOKEN).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    Gateways::from_bytes(json!({"gateways": [{"name": "g", "admin_url": origin, "token_file": path, "allow_insecure_http": true}]}).to_string().as_bytes()).unwrap()
}

/// A usage record with a structure (the segments' fingerprints, one per text) and, optionally, a payload reference.
fn record(id: &str, ts: &str, segments: &Value, reference: Option<&str>, expires: &str) -> Value {
    let structure: Vec<Value> = segments
        .as_array()
        .unwrap()
        .iter()
        .map(|s| json!({"kind": s["kind"], "hash": fingerprint(s["text"].as_str().unwrap()), "bytes": s["text"].as_str().unwrap().len()}))
        .collect();
    let mut r = json!({
        "event": "usage", "ts": ts, "attempt_id": id, "tenant": "t", "key": "k", "request_id": format!("req-{id}"),
        "provider": "openai", "model": "m", "kind": "completion", "outcome": "completed", "duration_ms": 10,
        "input_tokens": 1, "output_tokens": 1, "cache_read_tokens": 0, "cache_write_tokens": 0,
        "usage_reported": true, "admission": "fleet", "structure": structure,
    });
    if let Some(reference) = reference {
        r["payload"] = json!({"reference": reference, "bytes": 10, "expires_at": expires});
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
                .header("x-llmshim-actor", "ops@example.test")
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
    if let Some(gateways) = gateways {
        state.set_gateways(gateways);
    }
    router(state)
}

async fn views(app: &axum::Router) -> Vec<String> {
    let (_, _, history) = get(app, "/v1/history?limit=100").await;
    history["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["action"] == "payload.viewed")
        .map(|r| r["subject"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_changed_prompt_is_shown_side_by_side_around_the_first_difference_and_both_views_are_recorded(
) {
    let before = prompt("You are helpful. Time: 10:00:01", &["hello", "thanks"]);
    let after = prompt(
        "You are helpful. Time: 10:03:44",
        &["hello", "thanks", "one more"],
    );
    let (origin, asked) = gateway(vec![
        ("ref_before", answer(before.clone())),
        ("ref_after", answer(after.clone())),
    ])
    .await;
    let app = app_with(
        &[
            record(
                "p1",
                "2026-11-19T10:00:00Z",
                &before,
                Some("ref_before"),
                "2099-01-01T00:00:00Z",
            ),
            record(
                "p2",
                "2026-11-19T10:03:00Z",
                &after,
                Some("ref_after"),
                "2099-01-01T00:00:00Z",
            ),
        ],
        Some(gateways(&origin)),
    );
    let (status, headers, body) = get(&app, "/v1/requests/p2/payload/diff").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        (
            body["attempt_id"].as_str(),
            body["previous_attempt_id"].as_str(),
            body["relation"].as_str()
        ),
        (Some("p2"), Some("p1"), Some("changed"))
    );
    assert_eq!(body["first_change"]["index"], 0);
    assert_eq!(body["first_change"]["label"], "system prompt");
    // The window is the first difference and the segments after it (up to two each side), both requests' text.
    let window = body["window"].as_array().unwrap();
    assert_eq!(window.len(), 3);
    assert_eq!(window[0]["index"], 0);
    assert_eq!(window[0]["changed"], true);
    assert_eq!(
        window[0]["previous"]["text"],
        "You are helpful. Time: 10:00:01"
    );
    assert_eq!(
        window[0]["current"]["text"],
        "You are helpful. Time: 10:03:44"
    );
    assert_eq!(
        window[1]["changed"], false,
        "an unchanged segment is shown as context, not as a change"
    );
    assert_eq!(window[2]["current"]["text"], "thanks");
    // Two payloads were opened, and both views are in the history with the attempt they read.
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    let mut seen = views(&app).await;
    seen.sort();
    assert_eq!(seen, ["p1", "p2"]);
    let history = get(&app, "/v1/history?limit=100").await.2.to_string();
    assert!(
        !history.contains("Time: 10:0") && !history.contains("ref_after"),
        "no text and no reference in the record"
    );
    assert_eq!(get(&app, "/v1/history/verify").await.2["ok"], true);
}

#[tokio::test]
async fn a_prompt_that_only_grew_or_repeated_opens_nothing_and_records_nothing() {
    let (origin, asked) = gateway(vec![]).await;
    let base = prompt("sys", &["a"]);
    let app = app_with(
        &[
            record(
                "g1",
                "2026-11-19T10:00:00Z",
                &base,
                Some("r1"),
                "2099-01-01T00:00:00Z",
            ),
            record(
                "g2",
                "2026-11-19T10:01:00Z",
                &prompt("sys", &["a", "b"]),
                Some("r2"),
                "2099-01-01T00:00:00Z",
            ),
            record(
                "g3",
                "2026-11-19T10:02:00Z",
                &prompt("sys", &["a", "b"]),
                Some("r3"),
                "2099-01-01T00:00:00Z",
            ),
        ],
        Some(gateways(&origin)),
    );
    let (_, _, first) = get(&app, "/v1/requests/g1/payload/diff").await;
    assert_eq!(first["relation"], "no_previous");
    let (_, _, grew) = get(&app, "/v1/requests/g2/payload/diff").await;
    assert_eq!(grew["relation"], "appended");
    let (_, _, same) = get(&app, "/v1/requests/g3/payload/diff").await;
    assert_eq!(same["relation"], "identical");
    for answer in [&grew, &same] {
        assert!(answer.get("window").is_none() && answer.get("unavailable").is_none());
    }
    assert_eq!(asked.load(Ordering::SeqCst), 0, "nothing was opened");
    assert!(
        views(&app).await.is_empty(),
        "nothing was read, so nothing is recorded"
    );
}

#[tokio::test]
async fn when_a_payload_is_missing_expired_or_cut_it_says_so_and_a_failed_fetch_is_still_recorded()
{
    let before = prompt("old system", &["x"]);
    let after = prompt("new system", &["x"]);
    let (origin, _) = gateway(vec![
        ("cut_before", json!({"payload": {"v": 1}, "segments": null})),
        ("ref_after", answer(after.clone())),
    ])
    .await;
    let app = app_with(
        &[
            // The earlier request was not captured at all.
            record("m1", "2026-11-19T10:00:00Z", &before, None, ""),
            record(
                "m2",
                "2026-11-19T10:01:00Z",
                &after,
                Some("ref_after"),
                "2099-01-01T00:00:00Z",
            ),
            // The earlier payload has expired.
            record(
                "e1",
                "2026-11-19T11:00:00Z",
                &before,
                Some("old"),
                "2020-01-01T00:00:00Z",
            ),
            record(
                "e2",
                "2026-11-19T11:01:00Z",
                &after,
                Some("ref_after"),
                "2099-01-01T00:00:00Z",
            ),
            // The earlier request was cut at the capture limit: no segments.
            record(
                "c1",
                "2026-11-19T12:00:00Z",
                &before,
                Some("cut_before"),
                "2099-01-01T00:00:00Z",
            ),
            record(
                "c2",
                "2026-11-19T12:01:00Z",
                &after,
                Some("ref_after"),
                "2099-01-01T00:00:00Z",
            ),
            // A reference no gateway has.
            record(
                "n1",
                "2026-11-19T13:00:00Z",
                &before,
                Some("nobody_has_it"),
                "2099-01-01T00:00:00Z",
            ),
            record(
                "n2",
                "2026-11-19T13:01:00Z",
                &after,
                Some("ref_after"),
                "2099-01-01T00:00:00Z",
            ),
        ],
        Some(gateways(&origin)),
    );
    for id in ["m2", "e2"] {
        let (status, _, body) = get(&app, &format!("/v1/requests/{id}/payload/diff")).await;
        assert_eq!(
            (status, body["unavailable"].as_str()),
            (StatusCode::OK, Some("payload_not_captured_or_expired")),
            "{id}"
        );
    }
    assert!(views(&app).await.is_empty(), "nothing was opened for those");
    let (status, _, body) = get(&app, "/v1/requests/c2/payload/diff").await;
    assert_eq!(
        (status, body["unavailable"].as_str()),
        (StatusCode::OK, Some("request_cut"))
    );
    assert!(body.get("window").is_none());
    // Opened and cut: both views were recorded. A payload no gateway has: also recorded, and a plain 404.
    let (status, _, body) = get(&app, "/v1/requests/n2/payload/diff").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("payload_gone"))
    );
    let mut seen = views(&app).await;
    seen.sort();
    assert_eq!(seen, ["c1", "c2", "n1", "n2"]);
    // A request with no structure at all.
    assert_eq!(
        get(&app, "/v1/requests/missing/payload/diff").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn without_gateways_the_diff_cannot_open_anything_but_the_attempt_to_read_is_recorded() {
    let before = prompt("old", &["x"]);
    let after = prompt("new", &["x"]);
    let app = app_with(
        &[
            record(
                "w1",
                "2026-11-19T10:00:00Z",
                &before,
                Some("a"),
                "2099-01-01T00:00:00Z",
            ),
            record(
                "w2",
                "2026-11-19T10:01:00Z",
                &after,
                Some("b"),
                "2099-01-01T00:00:00Z",
            ),
        ],
        None,
    );
    let (status, _, body) = get(&app, "/v1/requests/w2/payload/diff").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::NOT_IMPLEMENTED, Some("gateways_not_configured"))
    );
    assert_eq!(views(&app).await.len(), 2);
}

#[tokio::test]
async fn a_segment_longer_than_the_limit_is_cut_and_says_so() {
    let long = "x".repeat(70 * 1024);
    let before = prompt(&format!("{long}A"), &[]);
    let after = prompt(&format!("{long}B"), &[]);
    let (origin, _) = gateway(vec![
        ("rb", answer(before.clone())),
        ("ra", answer(after.clone())),
    ])
    .await;
    let app = app_with(
        &[
            record(
                "l1",
                "2026-11-19T10:00:00Z",
                &before,
                Some("rb"),
                "2099-01-01T00:00:00Z",
            ),
            record(
                "l2",
                "2026-11-19T10:01:00Z",
                &after,
                Some("ra"),
                "2099-01-01T00:00:00Z",
            ),
        ],
        Some(gateways(&origin)),
    );
    let (status, _, body) = get(&app, "/v1/requests/l2/payload/diff").await;
    assert_eq!(status, StatusCode::OK);
    let one = &body["window"][0]["current"];
    assert_eq!(one["cut"], true);
    assert_eq!(one["text"].as_str().unwrap().len(), 64 * 1024);
}

#[tokio::test]
async fn a_difference_past_the_cut_still_counts_as_a_change() {
    let long = "x".repeat(70 * 1024);
    let before = prompt(&format!("{long}A"), &[]);
    let after = prompt(&format!("{long}B"), &[]);
    let (origin, _) = gateway(vec![
        ("rb", answer(before.clone())),
        ("ra", answer(after.clone())),
    ])
    .await;
    let app = app_with(
        &[
            record(
                "q1",
                "2026-11-19T10:00:00Z",
                &before,
                Some("rb"),
                "2099-01-01T00:00:00Z",
            ),
            record(
                "q2",
                "2026-11-19T10:01:00Z",
                &after,
                Some("ra"),
                "2099-01-01T00:00:00Z",
            ),
        ],
        Some(gateways(&origin)),
    );
    let (_, _, body) = get(&app, "/v1/requests/q2/payload/diff").await;
    // The texts sent are cut at 64 KiB and so are equal, but the segments differ and are said to.
    assert_eq!(
        body["window"][0]["current"]["text"],
        body["window"][0]["previous"]["text"]
    );
    assert_eq!(body["window"][0]["changed"], true);
    assert_eq!(body["window"][0]["current"]["cut"], true);
}

#[tokio::test]
async fn an_opened_prompt_that_is_not_the_one_the_fingerprints_named_is_not_shown() {
    let before = prompt("old system", &["a", "b"]);
    let after = prompt("new system", &["a", "b"]);
    // A gateway that segments differently (one segment fewer), and one whose text is not the size the fingerprint recorded.
    let fewer = prompt("new system", &["a"]);
    let resized = prompt("new system!", &["a", "b"]);
    let (origin, _) = gateway(vec![
        ("ok_before", answer(before.clone())),
        ("fewer", answer(fewer)),
        ("resized", answer(resized)),
    ])
    .await;
    let app = app_with(
        &[
            record(
                "s1",
                "2026-11-19T10:00:00Z",
                &before,
                Some("ok_before"),
                "2099-01-01T00:00:00Z",
            ),
            record(
                "s2",
                "2026-11-19T10:01:00Z",
                &after,
                Some("fewer"),
                "2099-01-01T00:00:00Z",
            ),
            record(
                "t1",
                "2026-11-19T11:00:00Z",
                &before,
                Some("ok_before"),
                "2099-01-01T00:00:00Z",
            ),
            record(
                "t2",
                "2026-11-19T11:01:00Z",
                &after,
                Some("resized"),
                "2099-01-01T00:00:00Z",
            ),
        ],
        Some(gateways(&origin)),
    );
    for id in ["s2", "t2"] {
        let (status, _, body) = get(&app, &format!("/v1/requests/{id}/payload/diff")).await;
        assert_eq!(
            (status, body["unavailable"].as_str()),
            (StatusCode::OK, Some("segments_do_not_match")),
            "{id}"
        );
        assert!(body.get("window").is_none());
    }
}

#[tokio::test]
async fn a_difference_in_the_folded_tail_has_no_text_to_show_and_says_so() {
    let before = json!([{"kind": "system", "text": "s"}, {"kind": "other", "text": null}]);
    let after = json!([{"kind": "system", "text": "s"}, {"kind": "other", "text": null}]);
    // Fingerprints that differ at index 1, whose text neither side has.
    let mut r1 = record(
        "f1",
        "2026-11-19T10:00:00Z",
        &json!([{"kind": "system", "text": "s"}, {"kind": "other", "text": "tail one"}]),
        Some("a"),
        "2099-01-01T00:00:00Z",
    );
    let mut r2 = record(
        "f2",
        "2026-11-19T10:01:00Z",
        &json!([{"kind": "system", "text": "s"}, {"kind": "other", "text": "tail two"}]),
        Some("b"),
        "2099-01-01T00:00:00Z",
    );
    // (The folded tail's bytes are not the opened text's: it has none.)
    r1["structure"][1]["bytes"] = json!(8);
    r2["structure"][1]["bytes"] = json!(8);
    let (origin, _) = gateway(vec![("a", answer(before)), ("b", answer(after))]).await;
    let app = app_with(&[r1, r2], Some(gateways(&origin)));
    let (status, _, body) = get(&app, "/v1/requests/f2/payload/diff").await;
    assert_eq!(
        (status, body["unavailable"].as_str()),
        (StatusCode::OK, Some("segment_not_shown")),
        "{body}"
    );
}

#[tokio::test]
async fn every_answer_of_the_comparison_is_no_store_including_the_ones_with_no_text() {
    let before = prompt("a", &["x"]);
    let (origin, _) = gateway(vec![]).await;
    let app = app_with(
        &[record(
            "z1",
            "2026-11-19T10:00:00Z",
            &before,
            Some("r"),
            "2099-01-01T00:00:00Z",
        )],
        Some(gateways(&origin)),
    );
    for uri in [
        "/v1/requests/z1/payload/diff",
        "/v1/requests/missing/payload/diff",
    ] {
        let (_, headers, _) = get(&app, uri).await;
        assert_eq!(headers[header::CACHE_CONTROL], "no-store", "{uri}");
    }
}

#[test]
fn only_an_admin_may_open_the_comparison() {
    let role = ROLES
        .iter()
        .find(|(m, p, _)| *m == "get" && *p == "/v1/requests/{attempt}/payload/diff")
        .map(|(_, _, r)| *r);
    assert_eq!(role, Some(Some(Role::Admin)));
}

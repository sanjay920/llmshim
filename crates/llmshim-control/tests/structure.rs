//! Prompt structure and the cache debugger (design note 0052, slice 5a): lists stored beside the ledger, paired
//! with the same client's request before, compared for where the cached prefix first changed, aggregated for
//! where misses start, and aged out with the ledger. On SQLite, and on Postgres when
//! `LLMSHIM_CONTROL_TEST_POSTGRES_URL` is set.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use llmshim_control::api::{router, AppState};
use serde_json::{json, Value};
use tower::ServiceExt;

const SHARED: &str = "control-token-0123456789abcdef";

/// A fingerprint a test can read: the tag's bytes in hex, padded to 16 digits.
fn fingerprint(tag: &str) -> String {
    let hex: String = tag.bytes().map(|b| format!("{b:02x}")).collect();
    format!("{hex:0<16}")
}

fn seg(kind: &str, tag: &str) -> Value {
    json!({"kind": kind, "hash": fingerprint(tag), "bytes": 100})
}

/// A usage record with a structure, for a client, model and time.
fn record(id: &str, ts: &str, key: &str, model: &str, structure: Value) -> Value {
    json!({
        "event": "usage", "ts": ts, "attempt_id": id, "tenant": "payments", "key": key,
        "request_id": format!("req-{id}"), "provider": "openai", "model": model, "kind": "completion",
        "outcome": "completed", "duration_ms": 10, "input_tokens": 1000, "output_tokens": 5,
        "uncached_input_tokens": 100, "cache_read_tokens": 900, "cache_write_tokens": 0, "cost_usd": 0.01,
        "cost_source": "provider", "usage_reported": true, "admission": "fleet", "structure": structure,
    })
}

fn prompt(system: &str, turns: &[&str]) -> Value {
    let mut out = vec![seg("tool", "t1"), seg("system", system)];
    out.extend(turns.iter().map(|t| seg("user", t)));
    Value::Array(out)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {SHARED}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn app_with(records: &[Value]) -> (axum::Router, std::sync::Arc<AppState>) {
    let mut store = common::store();
    let counts = store.ingest_requests(records).unwrap();
    assert_eq!(counts.rejected, 0);
    let state = AppState::new(store, SHARED, None);
    (router(state.clone()), state)
}

#[tokio::test]
async fn a_request_is_compared_with_the_same_clients_request_before_it() {
    let (app, _) = app_with(&[
        record(
            "a1",
            "2026-11-19T10:00:00Z",
            "checkout",
            "gpt-6-sol",
            prompt("s1", &["u1"]),
        ),
        record(
            "a2",
            "2026-11-19T10:01:00Z",
            "checkout",
            "gpt-6-sol",
            prompt("s1", &["u1", "u2"]),
        ),
        record(
            "a3",
            "2026-11-19T10:02:00Z",
            "checkout",
            "gpt-6-sol",
            prompt("s2", &["u1", "u2", "u3"]),
        ),
        // Another client and another model are never paired with these.
        record(
            "b1",
            "2026-11-19T10:02:30Z",
            "search",
            "gpt-6-sol",
            prompt("s9", &["x"]),
        ),
        record(
            "c1",
            "2026-11-19T10:02:40Z",
            "checkout",
            "gpt-6-luna",
            prompt("s9", &["x"]),
        ),
        // More than an hour after the last one: no predecessor.
        record(
            "a4",
            "2026-11-19T12:00:00Z",
            "checkout",
            "gpt-6-sol",
            prompt("s2", &["u1", "u2", "u3"]),
        ),
    ]);
    // The first of its kind has nothing to compare with.
    let (status, first) = get(&app, "/v1/requests/a1/structure").await;
    assert_eq!(status, StatusCode::OK);
    assert!(first["previous"].is_null() && first["diff"].is_null());
    assert_eq!(first["segments"].as_array().unwrap().len(), 3);
    // It only grew: appended, with the common part and no first change.
    let (_, grew) = get(&app, "/v1/requests/a2/structure").await;
    assert_eq!(grew["previous"]["attempt_id"], "a1");
    assert_eq!(grew["previous"]["gap_secs"], 60);
    assert_eq!(grew["previous"]["within_short_window"], true);
    assert_eq!(grew["diff"]["relation"], "appended");
    assert_eq!(grew["diff"]["common_segments"], 3);
    assert_eq!(grew["diff"]["total_segments"], 4);
    assert!(grew["diff"]["first_change"].is_null());
    // The system prompt changed: a miss from there, named.
    let (_, changed) = get(&app, "/v1/requests/a3/structure").await;
    assert_eq!(changed["previous"]["attempt_id"], "a2");
    assert_eq!(changed["diff"]["relation"], "changed");
    assert_eq!(changed["diff"]["common_segments"], 1);
    assert_eq!(changed["diff"]["first_change"]["kind"], "system");
    assert_eq!(changed["diff"]["first_change"]["label"], "system prompt");
    // Other clients and models are their own sequences.
    for id in ["b1", "c1"] {
        let (_, only) = get(&app, &format!("/v1/requests/{id}/structure")).await;
        assert!(only["previous"].is_null(), "{id}: {only}");
    }
    // Past the hour the earlier request cannot still be in a cache: no pairing.
    let (_, late) = get(&app, "/v1/requests/a4/structure").await;
    assert!(late["previous"].is_null(), "{late}");
    // A predecessor older than five minutes is paired but flagged: it may have expired already.
    let (app2, _) = app_with(&[
        record("p1", "2026-11-19T10:00:00Z", "k", "m", prompt("s", &["a"])),
        record("p2", "2026-11-19T10:20:00Z", "k", "m", prompt("s", &["a"])),
    ]);
    let (_, slow) = get(&app2, "/v1/requests/p2/structure").await;
    assert_eq!(slow["diff"]["relation"], "identical");
    assert_eq!(slow["previous"]["within_short_window"], false);
}

#[tokio::test]
async fn a_request_without_structure_is_a_404_that_says_why_and_the_row_is_never_lost_to_a_bad_field(
) {
    let mut no_key = record("n1", "2026-11-19T10:00:00Z", "k", "m", prompt("s", &["a"]));
    no_key["key"] = Value::Null;
    let (app, _) = app_with(&[
        record("none", "2026-11-19T10:00:00Z", "k", "m", Value::Null),
        record(
            "bad",
            "2026-11-19T10:00:01Z",
            "k",
            "m",
            json!([{"kind": "poem", "hash": "x", "bytes": 1}]),
        ),
        record(
            "huge",
            "2026-11-19T10:00:02Z",
            "k",
            "m",
            Value::Array((0..2049).map(|_| seg("user", "a")).collect()),
        ),
        no_key,
    ]);
    for id in ["none", "bad", "huge", "n1", "missing"] {
        let (status, body) = get(&app, &format!("/v1/requests/{id}/structure")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
        assert_eq!(body["error"]["code"], "no_structure");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not capture"));
    }
    // The rows themselves were all stored: spend is never lost to a field that is only for debugging.
    let (_, page) = get(&app, "/v1/requests?limit=100").await;
    assert_eq!(page["requests"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn a_repeated_attempt_keeps_its_first_structure() {
    let mut store = common::store();
    store
        .ingest_requests(&[record(
            "d1",
            "2026-11-19T10:00:00Z",
            "k",
            "m",
            prompt("first", &["a"]),
        )])
        .unwrap();
    let again = store
        .ingest_requests(&[record(
            "d1",
            "2026-11-19T10:00:00Z",
            "k",
            "m",
            prompt("second", &["a"]),
        )])
        .unwrap();
    assert_eq!((again.accepted, again.duplicate), (0, 1));
    let kept = store.request_structure("d1").unwrap().unwrap();
    assert_eq!(kept.segments[1].hash, fingerprint("first"));
}

#[tokio::test]
async fn misses_count_each_relation_and_where_the_changed_ones_began() {
    let (app, _) = app_with(&[
        // checkout: grew, grew, then the system prompt changed twice in a row.
        record(
            "1",
            "2026-11-19T10:00:00Z",
            "checkout",
            "m",
            prompt("s1", &["a"]),
        ),
        record(
            "2",
            "2026-11-19T10:01:00Z",
            "checkout",
            "m",
            prompt("s1", &["a", "b"]),
        ),
        record(
            "3",
            "2026-11-19T10:02:00Z",
            "checkout",
            "m",
            prompt("s1", &["a", "b", "c"]),
        ),
        record(
            "4",
            "2026-11-19T10:03:00Z",
            "checkout",
            "m",
            prompt("s2", &["a", "b", "c"]),
        ),
        record(
            "5",
            "2026-11-19T10:04:00Z",
            "checkout",
            "m",
            prompt("s3", &["a", "b", "c"]),
        ),
        // search: identical, then a tool changed.
        record(
            "6",
            "2026-11-19T10:00:30Z",
            "search",
            "m",
            prompt("s1", &["a"]),
        ),
        record(
            "7",
            "2026-11-19T10:01:30Z",
            "search",
            "m",
            prompt("s1", &["a"]),
        ),
        {
            let mut changed = record(
                "8",
                "2026-11-19T10:02:30Z",
                "search",
                "m",
                prompt("s1", &["a"]),
            );
            changed["structure"][0] = seg("tool", "t9");
            changed
        },
        // A pair more than an hour apart is not a pair.
        record(
            "9",
            "2026-11-19T13:00:00Z",
            "search",
            "m",
            prompt("s1", &["a"]),
        ),
    ]);
    let (status, m) = get(
        &app,
        "/v1/requests/misses?from=2026-11-19T00:00:00Z&to=2026-11-20T00:00:00Z",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (
            m["pairs"].as_u64(),
            m["identical"].as_u64(),
            m["appended"].as_u64(),
            m["changed"].as_u64()
        ),
        (Some(6), Some(1), Some(2), Some(3))
    );
    assert_eq!(m["changed_by_kind"], json!({"system": 2, "tool": 1}));
    assert_eq!(m["truncated"], false);
    // By client and model, the one with the most changes first.
    assert_eq!(m["groups"][0]["key"], "checkout");
    assert_eq!(m["groups"][0]["changed"], 2);
    assert_eq!(m["groups"][0]["changed_by_kind"], json!({"system": 2}));
    assert_eq!(m["groups"][1]["key"], "search");
    assert_eq!(m["groups"][1]["changed_by_kind"], json!({"tool": 1}));
    // A window with nothing in it is an empty answer, and a bad bound is a 400.
    let (_, none) = get(
        &app,
        "/v1/requests/misses?from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z",
    )
    .await;
    assert_eq!(none["pairs"], 0);
    assert_eq!(
        get(&app, "/v1/requests/misses?from=yesterday").await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn structure_ages_out_with_the_ledger() {
    let (app, state) = app_with(&[
        record("old", "2026-10-01T00:00:00Z", "k", "m", prompt("s", &["a"])),
        record("new", "2026-11-19T00:00:00Z", "k", "m", prompt("s", &["a"])),
    ]);
    assert_eq!(
        get(&app, "/v1/requests/old/structure").await.0,
        StatusCode::OK
    );
    let cutoff = chrono::DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(state.purge_requests(cutoff).unwrap(), 1);
    assert_eq!(
        get(&app, "/v1/requests/old/structure").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&app, "/v1/requests/new/structure").await.0,
        StatusCode::OK
    );
}

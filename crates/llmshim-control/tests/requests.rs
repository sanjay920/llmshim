//! The request ledger (design note 0051): ingest that stores each attempt once, queries that page
//! stably, summaries that never turn an unknown cost into zero, retention, and the ingest token that
//! opens one route and nothing else. On SQLite, and on Postgres when `LLMSHIM_CONTROL_TEST_POSTGRES_URL`
//! is set.

mod common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use llmshim_control::api::{router, AppState};
use llmshim_control::requests::{Group, RequestQuery, MAX_BATCH};
use serde_json::{json, Value};
use tower::ServiceExt;

/// A usage record as the gateway's usage log writes it, with the fields a test varies.
fn record(id: &str, ts: &str, extra: Value) -> Value {
    let mut base = json!({
        "event": "usage", "ts": ts, "attempt_id": id, "tenant": "payments", "key": "checkout",
        "tags": {"env": "prod"}, "request_id": format!("req-{id}"), "deployment": null,
        "provider": "openai", "model": "gpt-6-sol", "kind": "completion", "outcome": "completed",
        "http_status": null, "duration_ms": 1200, "input_tokens": 1000, "output_tokens": 50,
        "reasoning_tokens": null, "uncached_input_tokens": 200, "cache_read_tokens": 800,
        "cache_write_tokens": 0, "cost_usd": 0.01, "cost_source": "provider",
        "usage_reported": true, "admission": "fleet",
    });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    base
}

fn ms(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .unwrap()
        .timestamp_millis()
}

#[test]
fn a_batch_is_stored_once_a_malformed_row_is_counted_and_a_repeat_is_a_duplicate() {
    let mut store = common::store();
    let good = record("a1", "2026-11-19T10:00:00Z", json!({}));
    let counts = store
        .ingest_requests(&[
            good.clone(),
            // Newer gateways add fields: ignored.
            record(
                "a2",
                "2026-11-19T10:00:01Z",
                json!({"a_new_field": {"x": 1}}),
            ),
            // Malformed in distinct ways, none echoed, none stored.
            record("a3", "yesterday", json!({})),
            record("a4", "2026-11-19T10:00:02Z", json!({"outcome": "exploded"})),
            record("a5", "2026-11-19T10:00:03Z", json!({"input_tokens": -1})),
            record("a6", "2026-11-19T10:00:04Z", json!({"cost_usd": 1e30})),
            record("a7", "2026-11-19T10:00:05Z", json!({"model": ""})),
            record("a8", "2026-11-19T10:00:06Z", json!({"tags": {"k": 1}})),
            json!("not an object"),
            record("bad id with spaces", "2026-11-19T10:00:07Z", json!({})),
        ])
        .unwrap();
    assert_eq!(
        (counts.accepted, counts.duplicate, counts.rejected),
        (2, 0, 8)
    );
    // A retried batch: the same two attempts are stored once.
    let again = store
        .ingest_requests(&[good, record("a9", "2026-11-19T10:00:08Z", json!({}))])
        .unwrap();
    assert_eq!((again.accepted, again.duplicate, again.rejected), (1, 1, 0));
    let page = store
        .query_requests(&RequestQuery {
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.requests.len(), 3);
    // Too many at once is refused whole.
    let many: Vec<Value> = (0..=MAX_BATCH)
        .map(|n| record(&format!("m{n}"), "2026-11-19T10:00:00Z", json!({})))
        .collect();
    assert!(store.ingest_requests(&many).is_err());
}

#[test]
fn queries_filter_page_stably_and_report_the_cache_share_of_the_prompt() {
    let mut store = common::store();
    let mut rows = Vec::new();
    for n in 0..7 {
        // Two rows share each timestamp, so a page boundary must not split a tie wrongly.
        rows.push(record(
            &format!("r{n:02}"),
            &format!("2026-11-19T10:0{}:00Z", n / 2),
            json!({"model": if n % 2 == 0 { "gpt-6-sol" } else { "claude-opus-5-5" },
                "tenant": if n < 4 { "payments" } else { "analytics" }}),
        ));
    }
    store.ingest_requests(&rows).unwrap();
    let all = |limit| RequestQuery {
        limit,
        ..Default::default()
    };
    // Paging by 3 walks every row exactly once, newest first, and a row inserted mid-walk (older than
    // the cursor, newer than the oldest) neither repeats one nor loses one.
    let first = store.query_requests(&all(3)).unwrap();
    assert_eq!(first.requests.len(), 3);
    store
        .ingest_requests(&[record("late", "2026-11-19T10:01:30Z", json!({}))])
        .unwrap();
    let mut seen: Vec<String> = first
        .requests
        .iter()
        .map(|r| r.attempt_id.clone())
        .collect();
    let mut cursor = first.next_cursor;
    while let Some(c) = cursor {
        let page = store
            .query_requests(&RequestQuery {
                after: Some(llmshim_control::requests::decode_cursor(&c).unwrap()),
                ..all(3)
            })
            .unwrap();
        seen.extend(page.requests.iter().map(|r| r.attempt_id.clone()));
        cursor = page.next_cursor;
    }
    // Every original row is on the walk exactly once and in order; the row that arrived mid-walk is on
    // it at most once (it sorts after the cursor, so it is there exactly when the walk had not passed it).
    let originals: Vec<String> = seen.iter().filter(|id| *id != "late").cloned().collect();
    assert_eq!(
        originals,
        (0..7).rev().map(|n| format!("r{n:02}")).collect::<Vec<_>>()
    );
    assert!(seen.iter().filter(|id| *id == "late").count() <= 1);
    // Filters narrow, and compose.
    fn count(store: &mut llmshim_control::Store, query: RequestQuery) -> usize {
        store.query_requests(&query).unwrap().requests.len()
    }
    assert_eq!(
        count(
            &mut store,
            RequestQuery {
                tenant: Some("analytics".into()),
                ..all(100)
            }
        ),
        3
    );
    assert_eq!(
        count(
            &mut store,
            RequestQuery {
                tenant: Some("payments".into()),
                model: Some("claude-opus-5-5".into()),
                ..all(100)
            }
        ),
        2
    );
    assert_eq!(
        count(
            &mut store,
            RequestQuery {
                from_ms: Some(ms("2026-11-19T10:01:00Z")),
                to_ms: Some(ms("2026-11-19T10:02:00Z")),
                ..all(100)
            }
        ),
        3,
        "from is inclusive and to is exclusive (r02, r03 and late)"
    );
    assert_eq!(
        count(
            &mut store,
            RequestQuery {
                request_id: Some("req-r05".into()),
                ..all(100)
            }
        ),
        1
    );
    assert_eq!(
        count(
            &mut store,
            RequestQuery {
                min_cost_usd: Some(0.5),
                ..all(100)
            }
        ),
        0
    );
    // The cache share is of the whole prompt: 800 read of 200 uncached + 800 read + 0 written.
    let one = store
        .query_requests(&RequestQuery {
            request_id: Some("req-r05".into()),
            ..all(10)
        })
        .unwrap();
    assert_eq!(one.requests[0].cache_hit_pct, Some(80.0));
    // An attempt that did not report the uncached part has no share, which is not zero.
    store
        .ingest_requests(&[record(
            "unk",
            "2026-11-19T11:00:00Z",
            json!({"uncached_input_tokens": null}),
        )])
        .unwrap();
    let unknown = store
        .query_requests(&RequestQuery {
            request_id: Some("req-unk".into()),
            ..all(10)
        })
        .unwrap();
    assert_eq!(unknown.requests[0].cache_hit_pct, None);
    assert_eq!(unknown.requests[0].tags, Some(json!({"env": "prod"})));
}

#[test]
fn a_summary_never_adds_an_unknown_cost_as_zero_and_marks_the_estimates() {
    let mut store = common::store();
    store
        .ingest_requests(&[
            record("s1", "2026-11-18T10:00:00Z", json!({"cost_usd": 0.10, "cost_source": "provider"})),
            record("s2", "2026-11-18T11:00:00Z", json!({"cost_usd": 0.20, "cost_source": "catalog"})),
            record("s3", "2026-11-19T10:00:00Z", json!({"cost_usd": null, "cost_source": null})),
            record(
                "s4",
                "2026-11-19T12:00:00Z",
                json!({"outcome": "http_failure", "http_status": 503, "cost_usd": 0.05,
                    "cost_source": "provider_floor", "model": "claude-opus-5-5", "duration_ms": 9000,
                    "uncached_input_tokens": null}),
            ),
        ])
        .unwrap();
    let query = RequestQuery {
        limit: 100,
        ..Default::default()
    };
    let summary = store.requests_summary(&query, Some(Group::Day)).unwrap();
    let totals = &summary.totals;
    assert_eq!(totals.attempts, 4);
    assert_eq!((totals.completed, totals.failed), (3, 1));
    assert!((totals.cost_usd - 0.35).abs() < 1e-9, "{}", totals.cost_usd);
    assert_eq!(
        totals.cost_unknown_attempts, 1,
        "the unknown cost is a gap, not zero"
    );
    assert_eq!(
        totals.cost_estimated_attempts, 2,
        "a catalog estimate and a provider floor"
    );
    assert_eq!(totals.input_tokens, 4000);
    assert_eq!(totals.max_duration_ms, 9000);
    // 3 attempts reported their uncached part: 2400 read of (200+800)*3 = 3000, 80%.
    assert_eq!(totals.prompt_tokens_known, 3000);
    assert_eq!(totals.cache_hit_pct, Some(80.0));
    // By day, newest day first.
    let days: Vec<_> = summary
        .groups
        .iter()
        .map(|g| g.group.clone().unwrap())
        .collect();
    assert_eq!(days, ["2026-11-19", "2026-11-18"]);
    assert_eq!(summary.groups[0].totals.attempts, 2);
    // By model, the largest known spend first.
    let by_model = store.requests_summary(&query, Some(Group::Model)).unwrap();
    assert_eq!(by_model.groups[0].group.as_deref(), Some("gpt-6-sol"));
    // An empty window is zeros, not an error.
    let empty = store
        .requests_summary(
            &RequestQuery {
                tenant: Some("nobody".into()),
                ..query
            },
            None,
        )
        .unwrap();
    assert_eq!(
        (
            empty.totals.attempts,
            empty.totals.cost_usd,
            empty.totals.cache_hit_pct
        ),
        (0, 0.0, None)
    );
}

#[test]
fn retention_deletes_only_what_is_older_than_the_cutoff() {
    let mut store = common::store();
    store
        .ingest_requests(&[
            record("old1", "2026-10-01T00:00:00Z", json!({})),
            record("old2", "2026-10-02T00:00:00Z", json!({})),
            record("new1", "2026-11-19T00:00:00Z", json!({})),
        ])
        .unwrap();
    assert_eq!(store.purge_requests(ms("2026-11-01T00:00:00Z")).unwrap(), 2);
    let page = store
        .query_requests(&RequestQuery {
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.requests.len(), 1);
    assert_eq!(page.requests[0].attempt_id, "new1");
    // A second purge finds nothing, and the facets list what is left.
    assert_eq!(store.purge_requests(ms("2026-11-01T00:00:00Z")).unwrap(), 0);
    assert_eq!(store.request_facets(None).unwrap()["models"], ["gpt-6-sol"]);
}

// --- the HTTP side -----------------------------------------------------------------------------

const SHARED: &str = "control-token-0123456789abcdef";
const INGEST: &str = "ingest-token-0123456789abcdefgh";

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri);
    if let Some(bearer) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn app(with_ingest: bool) -> axum::Router {
    let state = AppState::new(common::store(), SHARED, None);
    if with_ingest {
        state.set_ingest_token(INGEST);
    }
    router(state)
}

#[tokio::test]
async fn the_ingest_token_opens_the_push_and_nothing_else_and_no_other_token_opens_the_push() {
    let app = app(true);
    let batch = json!({"records": [record("h1", "2026-11-19T10:00:00Z", json!({}))]});
    let (status, body) = call(
        &app,
        "POST",
        "/v1/usage/batch",
        Some(INGEST),
        Some(batch.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"accepted": 1, "duplicate": 0, "rejected": 0}));
    // The same batch again: stored once.
    let (_, body) = call(
        &app,
        "POST",
        "/v1/usage/batch",
        Some(INGEST),
        Some(batch.clone()),
    )
    .await;
    assert_eq!(body["duplicate"], 1);
    // The administering token cannot push, and no credential at all cannot.
    for bearer in [Some(SHARED), Some("wrong-token-0123456789abcdef"), None] {
        let (status, _) = call(&app, "POST", "/v1/usage/batch", bearer, Some(batch.clone())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer:?}");
    }
    // The ingest token is no credential anywhere else.
    for (method, uri) in [
        ("GET", "/v1/requests"),
        ("GET", "/v1/orgs"),
        ("GET", "/v1/history"),
        ("GET", "/v1/whoami"),
    ] {
        let (status, _) = call(&app, method, uri, Some(INGEST), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    // The administering token reads what was pushed.
    let (status, body) = call(&app, "GET", "/v1/requests", Some(SHARED), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["requests"].as_array().unwrap().len(), 1);
    assert_eq!(body["requests"][0]["cache_hit_pct"], 80.0);
    // Bodies that are not a batch are the API's own 400, and an oversized batch is refused whole.
    for bad in [
        json!({}),
        json!({"records": "x"}),
        json!({"records": [], "extra": 1}),
    ] {
        let (status, _) = call(&app, "POST", "/v1/usage/batch", Some(INGEST), Some(bad)).await;
        assert!(status.is_client_error(), "{status}");
    }
    let many: Vec<Value> = (0..=MAX_BATCH)
        .map(|n| record(&format!("z{n}"), "2026-11-19T10:00:00Z", json!({})))
        .collect();
    let (status, _) = call(
        &app,
        "POST",
        "/v1/usage/batch",
        Some(INGEST),
        Some(json!({"records": many})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn with_no_ingest_token_the_push_is_a_401_for_everyone() {
    let app = app(false);
    let batch = json!({"records": [record("h1", "2026-11-19T10:00:00Z", json!({}))]});
    for bearer in [Some(SHARED), Some(INGEST), None] {
        let (status, _) = call(&app, "POST", "/v1/usage/batch", bearer, Some(batch.clone())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer:?}");
    }
}

#[tokio::test]
async fn the_query_api_validates_its_parameters_and_pages_with_a_cursor() {
    let app = app(true);
    let records: Vec<Value> = (0..5)
        .map(|n| {
            record(
                &format!("p{n}"),
                &format!("2026-11-19T10:00:0{n}Z"),
                json!({}),
            )
        })
        .collect();
    call(
        &app,
        "POST",
        "/v1/usage/batch",
        Some(INGEST),
        Some(json!({"records": records})),
    )
    .await;
    let (_, first) = call(&app, "GET", "/v1/requests?limit=2", Some(SHARED), None).await;
    assert_eq!(first["requests"].as_array().unwrap().len(), 2);
    let cursor = first["next_cursor"].as_str().unwrap();
    let (_, second) = call(
        &app,
        "GET",
        &format!("/v1/requests?limit=2&cursor={cursor}"),
        Some(SHARED),
        None,
    )
    .await;
    let ids: Vec<&str> = first["requests"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["requests"].as_array().unwrap())
        .map(|r| r["attempt_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["p4", "p3", "p2", "p1"]);
    for bad in [
        "/v1/requests?limit=0",
        "/v1/requests?limit=501",
        "/v1/requests?from=yesterday",
        "/v1/requests?cursor=not-a-cursor",
        "/v1/requests?min_cost_usd=-1",
        "/v1/requests?unknown=1",
        "/v1/requests?tenant=",
        "/v1/requests/summary?group_by=planet",
    ] {
        let (status, body) = call(&app, "GET", bad, Some(SHARED), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    let (status, summary) = call(
        &app,
        "GET",
        "/v1/requests/summary?group_by=model",
        Some(SHARED),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(summary["totals"]["attempts"], 5);
    assert_eq!(summary["groups"][0]["group"], "gpt-6-sol");
    let (_, facets) = call(&app, "GET", "/v1/requests/facets", Some(SHARED), None).await;
    assert_eq!(facets["tenants"], json!(["payments"]));
}

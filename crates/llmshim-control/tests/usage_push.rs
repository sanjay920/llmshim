//! The whole path of design note 0051, slice 1: the gateway's real exporter pushes usage records over a
//! real socket to this crate's real router and ingest token, and the ledger answers queries with them.
//! Nothing here is a mock of the other side.

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use llmshim::serve::config::SecretRef;
use llmshim::serve::usage_export::{
    ExportSettings, Segment, UsageExporter, UsageRecord, UsageSink,
};
use llmshim_control::api::{router, AppState};
use serde_json::Value;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const SHARED: &str = "control-token-0123456789abcdef";
const INGEST: &str = "ingest-token-0123456789abcdefgh";

fn usage(id: &str) -> UsageRecord {
    UsageRecord {
        event: "usage",
        ts: "2026-11-19T10:00:00Z".into(),
        attempt_id: id.into(),
        tenant: Some("payments".into()),
        key: Some("checkout".into()),
        tags: None,
        request_id: Some(format!("req-{id}")),
        deployment: None,
        provider: "openai".into(),
        model: "gpt-6-sol".into(),
        kind: "completion",
        outcome: "completed",
        http_status: None,
        duration_ms: 1200,
        input_tokens: 1000,
        output_tokens: 50,
        reasoning_tokens: None,
        uncached_input_tokens: Some(200),
        cache_read_tokens: 800,
        cache_write_tokens: 0,
        cost_usd: Some(0.0185),
        cost_source: Some("provider".into()),
        usage_reported: true,
        admission: "fleet",
        structure: None,
        payload: None,
    }
}

fn segment(kind: &'static str, hash: &str, bytes: u64) -> Segment {
    Segment {
        kind,
        hash: hash.into(),
        bytes,
    }
}

async fn get(app: &axum::Router, uri: &str) -> Value {
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
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn records_pushed_by_a_gateway_are_in_the_ledger_once_each_and_the_token_is_the_only_way_in()
{
    let state = AppState::new(common::store(), SHARED, None);
    state.set_ingest_token(INGEST);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let app = router(state);

    let token_file =
        std::env::temp_dir().join(format!("llmshim-push-token-{}", uuid::Uuid::new_v4()));
    std::fs::write(&token_file, format!("{INGEST}\n")).unwrap();
    let token: SecretRef = serde_json::from_value(serde_json::json!({"file": token_file})).unwrap();
    let url = format!("http://{address}");
    let exporter = UsageExporter::new(
        &ExportSettings {
            url: &url,
            token: &token,
            batch_size: 3,
            flush_interval: Duration::from_millis(200),
            queue_size: 1000,
            ca_pem: None,
        },
        &|_| None,
    )
    .unwrap();
    for n in 0..7 {
        exporter.record(&usage(&format!("e{n}")));
    }
    // The same attempt reported twice (a retry the gateway could not know had arrived): stored once.
    exporter.record(&usage("e0"));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let page = get(&app, "/v1/requests?limit=100").await;
        if page["requests"].as_array().unwrap().len() == 7 && exporter.queued() == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the ledger never got the records: {page}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let summary = get(&app, "/v1/requests/summary?group_by=model").await;
    assert_eq!(summary["totals"]["attempts"], 7);
    assert_eq!(summary["totals"]["cache_hit_pct"], 80.0);
    assert!((summary["totals"]["cost_usd"].as_f64().unwrap() - 7.0 * 0.0185).abs() < 1e-9);
    assert_eq!(summary["totals"]["cost_unknown_attempts"], 0);
    let row = &get(&app, "/v1/requests?request_id=req-e3").await["requests"][0];
    assert_eq!(
        (
            row["tenant"].as_str(),
            row["key"].as_str(),
            row["model"].as_str()
        ),
        (Some("payments"), Some("checkout"), Some("gpt-6-sol"))
    );
    assert_eq!(row["cost_source"], "provider");
    // Closing flushes: one more record, closed at once, is still delivered.
    exporter.record(&usage("last"));
    let closing = std::sync::Arc::new(exporter);
    let c = closing.clone();
    tokio::task::spawn_blocking(move || c.close())
        .await
        .unwrap();
    assert_eq!(
        get(&app, "/v1/requests?request_id=req-last").await["requests"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let _ = std::fs::remove_file(token_file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_ingest_token_stores_nothing_and_keeps_the_records_queued() {
    let state = AppState::new(common::store(), SHARED, None);
    state.set_ingest_token(INGEST);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let app = router(state);
    let wrong: SecretRef = serde_json::from_value(serde_json::json!({"env": "WRONG"})).unwrap();
    let url = format!("http://{address}");
    let exporter = UsageExporter::new(
        &ExportSettings {
            url: &url,
            token: &wrong,
            batch_size: 1,
            flush_interval: Duration::from_millis(200),
            queue_size: 100,
            ca_pem: None,
        },
        &|name| (name == "WRONG").then(|| "not-the-ingest-token-0123456789".to_owned()),
    )
    .unwrap();
    exporter.record(&usage("w1"));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        get(&app, "/v1/requests").await["requests"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // Refused, so kept for when the operator fixes the token.
    assert_eq!(exporter.queued(), 1);
    exporter.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_structure_the_gateway_exports_is_stored_and_compared_by_the_control_plane() {
    let state = AppState::new(common::store(), SHARED, None);
    state.set_ingest_token(INGEST);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let app = router(state);
    let token_file =
        std::env::temp_dir().join(format!("llmshim-push-token-{}", uuid::Uuid::new_v4()));
    std::fs::write(&token_file, format!("{INGEST}\n")).unwrap();
    let token: SecretRef = serde_json::from_value(serde_json::json!({"file": token_file})).unwrap();
    let url = format!("http://{address}");
    let exporter = UsageExporter::new(
        &ExportSettings {
            url: &url,
            token: &token,
            batch_size: 10,
            flush_interval: Duration::from_millis(200),
            queue_size: 100,
            ca_pem: None,
        },
        &|_| None,
    )
    .unwrap();
    // Two requests of one client a minute apart: the second changed the system prompt (segment 1).
    let mut first = usage("s1");
    first.structure = Some(vec![
        segment("tool", "0000000000000001", 100),
        segment("system", "00000000000000a1", 400),
        segment("user", "00000000000000b1", 50),
    ]);
    let mut second = usage("s2");
    second.ts = "2026-11-19T10:01:00Z".into();
    second.structure = Some(vec![
        segment("tool", "0000000000000001", 100),
        segment("system", "00000000000000a2", 400),
        segment("user", "00000000000000b1", 50),
    ]);
    exporter.record(&first);
    exporter.record(&second);
    let deadline = Instant::now() + Duration::from_secs(15);
    let diff = loop {
        let answer = tokio::time::timeout(Duration::from_secs(5), async {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/v1/requests/s2/structure")
                        .header(header::AUTHORIZATION, format!("Bearer {SHARED}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            (
                response.status(),
                response.into_body().collect().await.unwrap().to_bytes(),
            )
        })
        .await
        .unwrap();
        if answer.0 == StatusCode::OK {
            break serde_json::from_slice::<Value>(&answer.1).unwrap();
        }
        assert!(Instant::now() < deadline, "the structure never arrived");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(diff["previous"]["attempt_id"], "s1");
    assert_eq!(diff["diff"]["relation"], "changed");
    assert_eq!(diff["diff"]["first_change"]["label"], "system prompt");
    assert_eq!(diff["diff"]["common_segments"], 1);
    let c = std::sync::Arc::new(exporter);
    tokio::task::spawn_blocking(move || c.close())
        .await
        .unwrap();
    let _ = std::fs::remove_file(token_file);
}

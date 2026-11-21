//! The gateways view's data (note 0051, slice 4): the control plane reads each listed gateway's admin
//! listener, with that listener's token, and reports what it found without ever repeating the gateway's
//! own text, the address's credentials or a token. Mock admin listeners are real loopback servers.

mod common;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use http_body_util::BodyExt;
use llmshim_control::api::{router, AppState};
use llmshim_control::gateways::Gateways;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const SHARED: &str = "control-token-0123456789abcdef";
const ADMIN_TOKEN: &str = "gateway-admin-token-0123456789";

struct Mock {
    origin: String,
    hits: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct Behaviour {
    hits: Arc<AtomicUsize>,
    token: Option<&'static str>,
    ready: StatusCode,
    stats: Arc<dyn Fn() -> Response + Send + Sync>,
}

fn authorized(behaviour: &Behaviour, headers: &HeaderMap) -> bool {
    match behaviour.token {
        None => true,
        Some(token) => headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == format!("Bearer {token}")),
    }
}

async fn serve(behaviour: Behaviour) -> Mock {
    async fn stats(State(b): State<Behaviour>, headers: HeaderMap) -> Response {
        b.hits.fetch_add(1, Ordering::SeqCst);
        if !authorized(&b, &headers) {
            return (StatusCode::UNAUTHORIZED, "token ADMIN-SECRET-TEXT").into_response();
        }
        (b.stats)()
    }
    async fn config(State(b): State<Behaviour>, headers: HeaderMap) -> Response {
        if !authorized(&b, &headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        axum::Json(json!({"generation": 3, "in_sync": true, "config_sha256": "abc"}))
            .into_response()
    }
    async fn ready(State(b): State<Behaviour>) -> Response {
        (b.ready, "x").into_response()
    }
    let hits = behaviour.hits.clone();
    let app = axum::Router::new()
        .route("/v1/gateway/stats", get(stats))
        .route("/v1/serve/config", get(config))
        .route("/ready", get(ready))
        .with_state(behaviour);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mock { origin, hits }
}

fn good_stats() -> Response {
    axum::Json(json!({
        "mode": "in-memory", "providers": ["openai"],
        "provider_circuits": [{"provider": "openai", "circuit": "closed"}],
        "routes": [{"name": "fast", "model": "openai/gpt-6-luna", "settings": {}}],
    }))
    .into_response()
}

fn behaviour(
    token: Option<&'static str>,
    stats: impl Fn() -> Response + Send + Sync + 'static,
) -> Behaviour {
    Behaviour {
        hits: Arc::new(AtomicUsize::new(0)),
        token,
        ready: StatusCode::OK,
        stats: Arc::new(stats),
    }
}

fn token_file(content: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!("llmshim-gw-token-{}", uuid::Uuid::new_v4()));
    std::fs::write(&path, content).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn file(entries: Value) -> Vec<u8> {
    json!({"gateways": entries}).to_string().into_bytes()
}

fn app(gateways: Gateways) -> axum::Router {
    let state = AppState::new(common::store(), SHARED, None);
    state.set_gateways(gateways);
    router(state)
}

async fn ask(app: &axum::Router) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/gateways")
                .header(header::AUTHORIZATION, format!("Bearer {SHARED}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn a_gateway_is_read_with_its_own_token_and_the_answer_is_shared() {
    let mock = serve(behaviour(Some(ADMIN_TOKEN), good_stats)).await;
    let token = token_file(ADMIN_TOKEN);
    let gateways = Gateways::from_bytes(&file(json!([
        {"name": "eu-1", "admin_url": mock.origin, "token_file": token, "allow_insecure_http": true}
    ])))
    .unwrap();
    let app = app(gateways);
    let answer = ask(&app).await;
    assert_eq!(answer["configured"], true);
    let gateway = &answer["gateways"][0];
    assert_eq!(gateway["name"], "eu-1");
    assert_eq!(gateway["reachable"], true);
    assert_eq!(gateway["ready"], true);
    assert_eq!(
        gateway["stats"]["provider_circuits"][0]["circuit"],
        "closed"
    );
    assert_eq!(gateway["stats"]["routes"][0]["name"], "fast");
    assert_eq!(gateway["config"]["generation"], 3);
    assert!(gateway.get("error").is_none());
    // Many viewers, one fetch: a second and third answer within the interval do not reach the gateway.
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    ask(&app).await;
    ask(&app).await;
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    // The token is not in the answer.
    assert!(!answer.to_string().contains(ADMIN_TOKEN));
}

#[tokio::test]
async fn failures_are_codes_and_never_the_gateways_own_text() {
    let secret = "ADMIN-SECRET-TEXT";
    let down = serve(behaviour(None, || {
        (StatusCode::INTERNAL_SERVER_ERROR, "ADMIN-SECRET-TEXT").into_response()
    }))
    .await;
    let unauthorized = serve(behaviour(Some(ADMIN_TOKEN), good_stats)).await;
    let garbage = serve(behaviour(None, || {
        "<html>ADMIN-SECRET-TEXT</html>".into_response()
    }))
    .await;
    let huge = serve(behaviour(None, || {
        let big = "x".repeat(2 << 20);
        axum::Json(json!({"pad": big})).into_response()
    }))
    .await;
    let wrong = token_file("a-different-token-0123456789ab");
    // A port nothing listens on.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let gateways = Gateways::with_cache(
        &file(json!([
            {"name": "broken", "admin_url": down.origin, "allow_insecure_http": true},
            {"name": "refused", "admin_url": unauthorized.origin, "token_file": wrong, "allow_insecure_http": true},
            {"name": "garbage", "admin_url": garbage.origin, "allow_insecure_http": true},
            {"name": "huge", "admin_url": huge.origin, "allow_insecure_http": true},
            {"name": "gone", "admin_url": closed, "allow_insecure_http": true},
        ])),
        Duration::ZERO,
    )
    .unwrap();
    let answer = ask(&app(gateways)).await;
    let by_name = |name: &str| {
        answer["gateways"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["name"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(by_name("broken")["error"], "bad_response");
    assert_eq!(by_name("refused")["error"], "unauthorized");
    // The open /ready probe still answers for a gateway whose token was refused.
    assert_eq!(by_name("refused")["ready"], true);
    assert_eq!(by_name("garbage")["error"], "bad_response");
    assert_eq!(by_name("huge")["error"], "bad_response");
    let gone = by_name("gone");
    assert_eq!(gone["reachable"], false);
    assert_eq!(gone["error"], "unreachable");
    assert_eq!(gone["ready"], Value::Null);
    // Nothing the gateways said, and no token, is in anything returned.
    let text = answer.to_string();
    assert!(
        !text.contains(secret) && !text.contains(ADMIN_TOKEN) && !text.contains("different-token"),
        "{text}"
    );
}

#[tokio::test]
async fn a_redirect_is_not_followed_and_the_token_is_read_at_each_fetch() {
    let target = serve(behaviour(None, good_stats)).await;
    let target_origin = target.origin.clone();
    let redirecting = serve(behaviour(None, move || {
        Redirect::temporary(&format!("{target_origin}/v1/gateway/stats")).into_response()
    }))
    .await;
    let gateways = Gateways::with_cache(
        &file(json!([{"name": "r", "admin_url": redirecting.origin, "allow_insecure_http": true}])),
        Duration::ZERO,
    )
    .unwrap();
    let answer = ask(&app(gateways)).await;
    assert_eq!(answer["gateways"][0]["error"], "bad_response");
    assert_eq!(
        target.hits.load(Ordering::SeqCst),
        0,
        "the redirect was followed"
    );

    // A rotated token file is picked up without a restart; an unreadable one is a code.
    let mock = serve(behaviour(Some(ADMIN_TOKEN), good_stats)).await;
    let path = token_file("an-old-token-0123456789abcdefg");
    let gateways = Gateways::with_cache(
        &file(json!([{"name": "rot", "admin_url": mock.origin, "token_file": path, "allow_insecure_http": true}])),
        Duration::ZERO,
    )
    .unwrap();
    let app = app(gateways);
    assert_eq!(ask(&app).await["gateways"][0]["error"], "unauthorized");
    std::fs::write(&path, ADMIN_TOKEN).unwrap();
    assert!(ask(&app).await["gateways"][0].get("error").is_none());
    std::fs::remove_file(&path).unwrap();
    assert_eq!(ask(&app).await["gateways"][0]["error"], "token_unreadable");
}

#[tokio::test]
async fn without_a_gateways_file_the_answer_says_none_are_configured() {
    let app = router(AppState::new(common::store(), SHARED, None));
    assert_eq!(
        ask(&app).await,
        json!({"configured": false, "gateways": []})
    );
}

#[test]
fn the_file_is_validated_by_entry_and_field_without_echoing_values() {
    let ok = json!({"name": "a", "admin_url": "https://gw.internal:9090"});
    assert!(Gateways::from_bytes(&file(json!([ok]))).is_ok());
    assert!(Gateways::from_bytes(&file(json!([]))).is_ok());
    let secret_url = "https://user:hunter2@gw.internal:9090";
    for (entry, field) in [
        (json!({"name": "", "admin_url": "https://g.test"}), "name"),
        (
            json!({"name": "has space", "admin_url": "https://g.test"}),
            "name",
        ),
        (
            json!({"name": "a", "admin_url": "http://g.test"}),
            "admin_url",
        ),
        (json!({"name": "a", "admin_url": secret_url}), "admin_url"),
        (
            json!({"name": "a", "admin_url": "https://g.test/path"}),
            "admin_url",
        ),
        (
            json!({"name": "a", "admin_url": "https://g.test/?q=1"}),
            "admin_url",
        ),
        (
            json!({"name": "a", "admin_url": "https://g.test/#f"}),
            "admin_url",
        ),
        (json!({"name": "a", "admin_url": "not a url"}), "admin_url"),
        (
            json!({"name": "a", "admin_url": "https://g.test", "token_file": "relative"}),
            "token_file",
        ),
        (
            json!({"name": "a", "admin_url": "https://g.test", "token_file": "/nonexistent/token"}),
            "token_file",
        ),
        (
            json!({"name": "a", "admin_url": "https://g.test", "payload_token_file": "relative"}),
            "payload_token_file",
        ),
        (
            json!({"name": "a", "admin_url": "https://g.test", "ca_file": "/nonexistent/ca.pem"}),
            "ca_file",
        ),
    ] {
        let error = match Gateways::from_bytes(&file(json!([entry]))) {
            Err(error) => error,
            Ok(_) => panic!("{entry} was accepted"),
        };
        assert!(
            error.starts_with(&format!("gateways[0].{field}")),
            "{error}"
        );
        assert!(!error.contains("hunter2"), "{error}");
    }
    // Names are unique, unknown fields are refused, and the list is bounded.
    let error = Gateways::from_bytes(&file(json!([ok, ok]))).err().unwrap();
    assert!(error.starts_with("gateways[1].name"), "{error}");
    assert!(Gateways::from_bytes(&file(
        json!([{"name": "a", "admin_url": "https://g.test", "extra": 1}])
    ))
    .is_err());
    let many: Vec<Value> = (0..65)
        .map(|n| json!({"name": format!("g{n}"), "admin_url": "https://g.test"}))
        .collect();
    assert!(Gateways::from_bytes(&file(json!(many))).is_err());
    // A token file other users can read is refused, as for every other token.
    use std::os::unix::fs::PermissionsExt;
    let open = token_file(ADMIN_TOKEN);
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).unwrap();
    let error = Gateways::from_bytes(&file(
        json!([{"name": "a", "admin_url": "https://g.test", "token_file": open}]),
    ))
    .err()
    .unwrap();
    assert!(error.contains("owner-only"), "{error}");
}

#[tokio::test]
async fn a_status_that_is_not_an_object_is_a_bad_response() {
    let list = serve(behaviour(None, || {
        axum::Json(json!(["not", "an", "object"])).into_response()
    }))
    .await;
    let gateways = Gateways::with_cache(
        &file(json!([{"name": "l", "admin_url": list.origin, "allow_insecure_http": true}])),
        Duration::ZERO,
    )
    .unwrap();
    let answer = ask(&app(gateways)).await;
    assert_eq!(answer["gateways"][0]["error"], "bad_response");
    assert!(answer["gateways"][0].get("stats").is_none());
}

#[test]
fn the_payload_token_must_not_be_the_metrics_token() {
    let path = token_file(ADMIN_TOKEN);
    let error = Gateways::from_bytes(&file(json!([{
        "name": "a", "admin_url": "https://g.test", "token_file": path, "payload_token_file": path
    }])))
    .err()
    .unwrap();
    assert!(
        error.starts_with("gateways[0].payload_token_file"),
        "{error}"
    );
}

#[test]
fn the_payload_token_must_not_hold_the_metrics_secret_under_another_path() {
    let admin = token_file(ADMIN_TOKEN);
    let copy = token_file(ADMIN_TOKEN);
    let error = Gateways::from_bytes(&file(json!([{
        "name": "a", "admin_url": "https://g.test", "token_file": admin, "payload_token_file": copy
    }])))
    .err()
    .unwrap();
    assert!(
        error.starts_with("gateways[0].payload_token_file"),
        "{error}"
    );
}

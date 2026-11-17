mod common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use llmshim_control::api::{router, AppState, ROUTES};
use serde_json::{json, Value};
use std::path::PathBuf;
use tower::ServiceExt;

const TOKEN: &str = "control-token-0123456789abcdef";

fn dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "llmshim-control-api-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

struct Api {
    app: axum::Router,
}

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

impl Api {
    fn new(signing_key_file: Option<PathBuf>) -> Self {
        let store = common::store();
        Self {
            app: router(AppState::new(store, TOKEN, signing_key_file)),
        }
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> Answer {
        self.call_with(method, path, body, token, &[]).await
    }

    async fn call_with(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: Option<&str>,
        extra: &[(&str, &str)],
    ) -> Answer {
        let mut request = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(path);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        Answer {
            status: parts.status,
            headers: parts.headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Answer {
        self.call(method, path, body, Some(TOKEN)).await
    }

    /// An organization, team and project, returning their ids.
    async fn tree(&self) -> (String, String, String) {
        let org = self
            .ok("POST", "/v1/orgs", Some(json!({"name": "Acme"})))
            .await
            .json();
        let org = org["id"].as_str().unwrap().to_string();
        let team = self
            .ok(
                "POST",
                &format!("/v1/orgs/{org}/teams"),
                Some(json!({"name": "Platform"})),
            )
            .await
            .json();
        let team = team["id"].as_str().unwrap().to_string();
        let project = self
            .ok(
                "POST",
                &format!("/v1/teams/{team}/projects"),
                Some(json!({"name": "Checkout"})),
            )
            .await
            .json();
        (org, team, project["id"].as_str().unwrap().to_string())
    }
}

#[tokio::test]
async fn every_route_needs_the_token_and_says_nothing_to_a_caller_without_it() {
    let api = Api::new(None);
    for (method, path) in ROUTES {
        let path = path
            .replace("{org}", "o")
            .replace("{team}", "t")
            .replace("{project}", "p")
            .replace("{key}", "k");
        for token in [None, Some("wrong-token-0123456789abcdef"), Some("")] {
            let answer = api
                .call(&method.to_uppercase(), &path, Some(json!({})), token)
                .await;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{method} {path}");
            assert_eq!(answer.headers[header::WWW_AUTHENTICATE], "Bearer");
            assert!(
                answer.body.is_empty(),
                "no detail for an unauthenticated caller: {}",
                answer.body
            );
        }
    }
    // An unknown path is not a route, but it is not an oracle either.
    assert_eq!(
        api.call("GET", "/v1/nothing", None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let answer = api.ok("GET", "/v1/nothing", None).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.json()["error"]["code"], "no_such_route");
}

#[tokio::test]
async fn the_key_lifecycle_over_http_shows_a_secret_once_and_records_the_actor() {
    let api = Api::new(None);
    let (org, _team, project) = api.tree().await;
    let created = api
        .call_with(
            "POST",
            &format!("/v1/projects/{project}/keys"),
            Some(
                json!({"name": "ci", "rpm": 60, "allowed_cidrs": ["10.0.0.0/8"],
                "allowed_models": ["openai/gpt-6-luna", "openrouter/anthropic/*"]}),
            ),
            Some(TOKEN),
            &[("x-llmshim-actor", "alice")],
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.headers[header::CACHE_CONTROL], "no-store");
    let body = created.json();
    let secret = body["secret"]
        .as_str()
        .expect("the secret is in the creation answer")
        .to_string();
    assert!(secret.len() >= 22);
    assert_eq!(
        body["key"]["allowed_models"],
        json!(["openai/gpt-6-luna", "openrouter/anthropic/*"])
    );
    let key = body["key"]["id"].as_str().unwrap().to_string();
    assert!(!created.body.contains("sha256"));

    // Nothing else ever carries it.
    let mut everything = String::new();
    everything += &api.ok("GET", &format!("/v1/keys/{key}"), None).await.body;
    everything += &api
        .ok("GET", &format!("/v1/orgs/{org}/keys"), None)
        .await
        .body;
    everything += &api.ok("GET", "/v1/history", None).await.body;
    assert!(!everything.contains(&secret), "the secret leaked");
    assert!(!everything.contains("sha256"));

    // Rotation: the successor's secret is shown once; the old key stays until it is revoked.
    let rotated = api
        .ok(
            "POST",
            &format!("/v1/keys/{key}/rotate"),
            Some(json!({"name": "ci-2"})),
        )
        .await;
    assert_eq!(rotated.status, StatusCode::CREATED);
    assert_eq!(rotated.headers[header::CACHE_CONTROL], "no-store");
    assert_ne!(rotated.json()["secret"], json!(secret));
    assert_eq!(
        api.ok("GET", &format!("/v1/keys/{key}"), None).await.json()["state"],
        "active"
    );
    let revoked = api
        .ok("POST", &format!("/v1/keys/{key}/revoke"), None)
        .await;
    assert_eq!(revoked.status, StatusCode::OK);
    assert_eq!(revoked.json()["state"], "revoked");
    assert_eq!(
        api.ok("POST", &format!("/v1/keys/{key}/revoke"), None)
            .await
            .status,
        StatusCode::CONFLICT
    );

    // The limits are a whole update.
    let other = rotated.json()["key"]["id"].as_str().unwrap().to_string();
    // A body that leaves a limit out is refused: it cannot silently clear a budget or an expiry.
    let partial = api
        .ok(
            "PATCH",
            &format!("/v1/keys/{other}/limits"),
            Some(json!({"rpm": 5})),
        )
        .await;
    assert_eq!(partial.status, StatusCode::BAD_REQUEST);
    assert!(partial.body.contains("tpm"), "{}", partial.body);
    let changed = api
        .ok(
            "PATCH",
            &format!("/v1/keys/{other}/limits"),
            Some(json!({"rpm": 5, "tpm": null, "budget_usd": null, "budget_window_secs": null, "expires_at": null})),
        )
        .await;
    assert_eq!(changed.status, StatusCode::OK);
    assert_eq!(changed.json()["rpm"], 5);
    assert_eq!(
        changed.json()["allowed_cidrs"],
        json!(["10.0.0.0/8"]),
        "restrictions are not limits"
    );

    let history = api.ok("GET", "/v1/history?limit=100", None).await.json();
    let rows = history["history"].as_array().unwrap();
    assert!(rows
        .iter()
        .any(|r| r["action"] == "key.created" && r["actor"] == "alice"));
    assert!(
        rows.iter()
            .any(|r| r["action"] == "key.created" && r["actor"] == "api"),
        "the default actor"
    );
}

#[tokio::test]
async fn bad_input_is_the_apis_own_400_and_missing_things_404() {
    let api = Api::new(None);
    let (_org, _team, project) = api.tree().await;
    let path = format!("/v1/projects/{project}/keys");
    for body in [
        json!({"name": "ci", "surprise": 1}),
        json!({}),
        json!({"name": "Bad Name"}),
        json!({"name": "ok", "allowed_cidrs": ["nope"]}),
        json!({"name": "ok", "allowed_models": []}),
        json!({"name": "ok", "allowed_models": ["not-a-routing-id"]}),
        json!({"name": "ok", "expires_at": "tomorrow"}),
    ] {
        let answer = api.ok("POST", &path, Some(body.clone())).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(answer.json()["error"]["code"], "invalid_request");
    }
    let broken = api.call("POST", &path, None, Some(TOKEN)).await;
    assert_eq!(broken.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        api.ok(
            "POST",
            "/v1/projects/proj_missing/keys",
            Some(json!({"name": "x"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.ok("GET", "/v1/keys/key_missing", None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.ok("GET", "/v1/orgs/org_missing/keys", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.ok("GET", "/v1/history?limit=0", None).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        api.call_with(
            "POST",
            "/v1/orgs",
            Some(json!({"name": "x"})),
            Some(TOKEN),
            &[("x-llmshim-actor", "")]
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // A taken name is a conflict.
    api.ok("POST", &path, Some(json!({"name": "ci"}))).await;
    assert_eq!(
        api.ok("POST", &path, Some(json!({"name": "ci"})))
            .await
            .status,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn a_snapshot_is_published_signed_and_served_with_an_etag() {
    let directory = dir("publish");
    let key_file = directory.join("seed");
    let (seed, public) = llmshim::serve::snapshot::keygen().unwrap();
    std::fs::write(&key_file, &seed).unwrap();
    std::fs::set_permissions(
        &key_file,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let api = Api::new(Some(key_file.clone()));
    let (org, _team, project) = api.tree().await;
    api.ok(
        "POST",
        &format!("/v1/projects/{project}/keys"),
        Some(json!({"name": "ci"})),
    )
    .await;
    assert_eq!(
        api.ok("GET", &format!("/v1/orgs/{org}/snapshot"), None)
            .await
            .status,
        StatusCode::NOT_FOUND,
        "nothing published yet"
    );

    let published = api
        .ok("POST", &format!("/v1/orgs/{org}/snapshots"), None)
        .await;
    assert_eq!(published.status, StatusCode::CREATED);
    assert_eq!(published.json()["generation"], 1);
    let latest = api
        .ok("GET", &format!("/v1/orgs/{org}/snapshot"), None)
        .await;
    assert_eq!(latest.status, StatusCode::OK);
    assert_eq!(latest.headers[header::ETAG], "\"1\"");
    assert_eq!(latest.headers[header::CONTENT_TYPE], "application/json");
    // The data plane's own verifier accepts exactly what the API served.
    let line = llmshim::serve::snapshot::describe(latest.body.as_bytes(), &[public]).unwrap();
    assert!(line.contains("generation 1"), "{line}");
    let same = api
        .call_with(
            "GET",
            &format!("/v1/orgs/{org}/snapshot"),
            None,
            Some(TOKEN),
            &[("if-none-match", "\"1\"")],
        )
        .await;
    assert_eq!(same.status, StatusCode::NOT_MODIFIED);
    assert!(same.body.is_empty());
    api.ok("POST", &format!("/v1/orgs/{org}/snapshots"), None)
        .await;
    let again = api
        .call_with(
            "GET",
            &format!("/v1/orgs/{org}/snapshot"),
            None,
            Some(TOKEN),
            &[("if-none-match", "\"1\"")],
        )
        .await;
    assert_eq!(
        again.status,
        StatusCode::OK,
        "a newer generation is not 'not modified'"
    );

    // The signing key is read at each publish: a bad one fails that publish, publishes
    // nothing, and says nothing about the file.
    std::fs::write(&key_file, "not a key").unwrap();
    std::fs::set_permissions(
        &key_file,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let failed = api
        .ok("POST", &format!("/v1/orgs/{org}/snapshots"), None)
        .await;
    assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        !failed.body.contains(key_file.to_str().unwrap()) && !failed.body.contains("not a key"),
        "{}",
        failed.body
    );
    std::fs::remove_file(&key_file).unwrap();
    let missing = api
        .ok("POST", &format!("/v1/orgs/{org}/snapshots"), None)
        .await;
    assert_eq!(missing.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!missing.body.contains(directory.to_str().unwrap()));
    std::fs::write(&key_file, &seed).unwrap();
    std::fs::set_permissions(
        &key_file,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(
        api.ok("POST", &format!("/v1/orgs/{org}/snapshots"), None)
            .await
            .json()["generation"],
        3,
        "the failures took no generation"
    );
}

#[tokio::test]
async fn publishing_without_a_signing_key_file_is_unavailable_not_broken() {
    let api = Api::new(None);
    let (org, _, _) = api.tree().await;
    let answer = api
        .ok("POST", &format!("/v1/orgs/{org}/snapshots"), None)
        .await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.json()["error"]["code"], "signing_key_unconfigured");
}

#[tokio::test]
async fn the_openapi_document_and_the_router_have_the_same_routes() {
    let text =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.yaml")).unwrap();
    let document: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
    let mut documented = std::collections::BTreeSet::new();
    for (path, item) in document["paths"].as_mapping().unwrap() {
        for (method, _) in item.as_mapping().unwrap() {
            let method = method.as_str().unwrap();
            if ["get", "post", "patch", "put", "delete"].contains(&method) {
                documented.insert((method.to_string(), path.as_str().unwrap().to_string()));
            }
        }
    }
    let routed: std::collections::BTreeSet<_> = ROUTES
        .iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect();
    assert_eq!(documented, routed, "the OpenAPI document and ROUTES differ");

    // And ROUTES is what the router serves: no entry is missing, none answers 405.
    let api = Api::new(None);
    for (method, path) in ROUTES {
        let path = path
            .replace("{org}", "o")
            .replace("{team}", "t")
            .replace("{project}", "p")
            .replace("{key}", "k");
        let answer = api.ok(&method.to_uppercase(), &path, Some(json!({}))).await;
        assert_ne!(
            answer.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path}"
        );
        assert_ne!(
            answer.json()["error"]["code"],
            "no_such_route",
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn the_bearer_scheme_is_case_insensitive_and_nothing_else_about_it_is_loosened() {
    let api = Api::new(None);
    let ok = api
        .call_with(
            "GET",
            "/v1/orgs",
            None,
            None,
            &[("authorization", &format!("bearer {TOKEN}"))],
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK);
    for header_value in [
        TOKEN.to_string(),
        format!("Basic {TOKEN}"),
        format!("Bearer{TOKEN}"),
        "Bearer ".to_string(),
        format!("Bearer {TOKEN}x"),
    ] {
        let answer = api
            .call_with(
                "GET",
                "/v1/orgs",
                None,
                None,
                &[("authorization", &header_value)],
            )
            .await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{header_value}");
    }
    // HEAD and OPTIONS are not a way around it either.
    for method in ["HEAD", "OPTIONS"] {
        let answer = api.call(method, "/v1/orgs", None, None).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{method}");
    }
}

#[tokio::test]
async fn a_signing_key_other_users_can_read_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let directory = dir("seedmode");
    let key_file = directory.join("seed");
    let (seed, _) = llmshim::serve::snapshot::keygen().unwrap();
    std::fs::write(&key_file, &seed).unwrap();
    let api = Api::new(Some(key_file.clone()));
    let (org, _, _) = api.tree().await;
    for mode in [0o644, 0o640, 0o604] {
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(mode)).unwrap();
        let answer = api
            .ok("POST", &format!("/v1/orgs/{org}/snapshots"), None)
            .await;
        assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR, "{mode:o}");
        assert!(
            answer.body.contains("readable by others"),
            "{}",
            answer.body
        );
    }
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        api.ok("POST", &format!("/v1/orgs/{org}/snapshots"), None)
            .await
            .status,
        StatusCode::CREATED
    );
}

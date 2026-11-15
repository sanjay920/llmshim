//! The operational endpoints of `llmshim serve` against `api/openapi.yaml`.
//!
//! The document is the contract (0013); this holds a real server to it: every
//! path marked `x-llmshim-operational` answers on the admin listener and nowhere
//! on the main one, behind the token the document says, with a body that
//! satisfies the schema the document gives. A path added to the document that the
//! server does not serve, or a field the server adds that the schema forbids,
//! fails here.
#![cfg(feature = "serve")]

#[path = "support/serve_harness.rs"]
mod harness;

use harness::{config, hangup, rewrite, start, ADMIN_TOKEN, CALLER_KEY};
use serde_json::Value;
use std::time::Duration;

fn document() -> Value {
    let text = include_str!("../api/openapi.yaml");
    serde_json::to_value(serde_yaml::from_str::<serde_yaml::Value>(text).unwrap()).unwrap()
}

/// `(path, operation)` for every operational GET in the document.
fn operational(document: &Value) -> Vec<(String, Value)> {
    document["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(path, item)| {
            let get = &item["get"];
            (get["x-llmshim-operational"] == true).then(|| (path.clone(), get.clone()))
        })
        .collect()
}

/// A validator for the schema a response of `operation` declares for `status`
/// and `media`, with the document's components reachable by `$ref`.
fn validator(
    document: &Value,
    operation: &Value,
    status: &str,
    media: &str,
) -> jsonschema::Validator {
    let schema = operation["responses"][status]["content"][media]["schema"].clone();
    assert!(schema.is_object(), "no {media} schema for {status}");
    let mut root = schema;
    root["components"] = document["components"].clone();
    jsonschema::validator_for(&root).expect("the schema compiles")
}

fn admin_config(upstream: &str) -> String {
    config(
        upstream,
        "[admin]\nport = 0\ntoken = { env = \"SERVE_E2E_ADMIN_TOKEN\" }\n",
    )
}

async fn get(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: Option<&str>,
) -> reqwest::Response {
    let mut request = client.get(format!("{base}{path}"));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.send().await.unwrap()
}

fn errors(validator: &jsonschema::Validator, body: &Value) -> Vec<String> {
    validator
        .iter_errors(body)
        .map(|error| format!("{error} at {}", error.instance_path()))
        .collect()
}

#[tokio::test]
async fn the_server_serves_exactly_the_operational_paths_the_document_describes() {
    let document = document();
    let paths = operational(&document);
    let mut names: Vec<&str> = paths.iter().map(|(path, _)| path.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "/metrics",
            "/ready",
            "/v1/gateway/stats",
            "/v1/serve/config"
        ],
        "a new operational path needs its schema and a line here"
    );

    let server = start(&admin_config("http://127.0.0.1:1"), "openapi-paths", &[]);
    let admin = server.admin.clone().expect("the admin listener");
    let client = reqwest::Client::new();
    for (path, operation) in &paths {
        let secured = operation["security"]
            .as_array()
            .is_some_and(|schemes| !schemes.is_empty());
        // Where it is served: the admin listener, and not the main one.
        let on_main = get(&client, &server.base, path, Some(ADMIN_TOKEN)).await;
        assert_eq!(
            on_main.status(),
            404,
            "{path} must not be on the main listener"
        );
        let without = get(&client, &admin, path, None).await;
        if secured {
            assert_eq!(without.status(), 401, "{path} needs the admin token");
            assert_eq!(without.headers()["www-authenticate"], "Bearer", "{path}");
            let wrong = get(
                &client,
                &admin,
                path,
                Some("not-the-admin-token-0123456789"),
            )
            .await;
            assert_eq!(wrong.status(), 401, "{path} with a wrong token");
        } else {
            assert_ne!(without.status(), 401, "{path} stays open for probes");
        }
        let with = get(&client, &admin, path, Some(ADMIN_TOKEN)).await;
        assert_eq!(with.status(), 200, "{path}");
    }
}

#[tokio::test]
async fn every_operational_response_satisfies_its_schema() {
    let document = document();
    let paths: std::collections::BTreeMap<String, Value> =
        operational(&document).into_iter().collect();
    let server = start(&admin_config("http://127.0.0.1:1"), "openapi-bodies", &[]);
    let admin = server.admin.clone().unwrap();
    let client = reqwest::Client::new();

    // /ready: the document's exact body and type.
    let ready = get(&client, &admin, "/ready", None).await;
    assert_eq!(ready.status(), 200);
    assert!(ready.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/plain"));
    let body = Value::String(ready.text().await.unwrap());
    let ready_schema = validator(&document, &paths["/ready"], "200", "text/plain");
    assert!(errors(&ready_schema, &body).is_empty(), "{body}");

    // /metrics: Prometheus text.
    let metrics = get(&client, &admin, "/metrics", Some(ADMIN_TOKEN)).await;
    assert!(metrics.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/plain"));
    let text = metrics.text().await.unwrap();
    // An idle server has registered no metric yet, so only the form is checked:
    // every non-comment line is `name{labels} value`, and the body ends in a newline.
    assert!(text.is_empty() || text.ends_with('\n'), "{text:?}");
    for line in text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
    {
        assert!(
            line.rsplit_once(' ')
                .is_some_and(|(_, v)| v.parse::<f64>().is_ok()),
            "{line}"
        );
    }

    // /v1/gateway/stats and /v1/serve/config: JSON that satisfies the schemas.
    for (path, key) in [
        ("/v1/gateway/stats", "stats"),
        ("/v1/serve/config", "config"),
    ] {
        let response = get(&client, &admin, path, Some(ADMIN_TOKEN)).await;
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json"));
        let body: Value = response.json().await.unwrap();
        let schema = validator(&document, &paths[path], "200", "application/json");
        assert!(
            errors(&schema, &body).is_empty(),
            "{key}: {:?} in {body}",
            errors(&schema, &body)
        );
    }
}

#[tokio::test]
async fn a_rejected_reload_still_satisfies_the_status_schema() {
    let document = document();
    let paths: std::collections::BTreeMap<String, Value> =
        operational(&document).into_iter().collect();
    let schema = validator(
        &document,
        &paths["/v1/serve/config"],
        "200",
        "application/json",
    );
    let server = start(&admin_config("http://127.0.0.1:1"), "openapi-reload", &[]);
    let admin = server.admin.clone().unwrap();
    let client = reqwest::Client::new();

    // A change outside the keys is refused by path, which fills `last_reload`
    // and turns `in_sync` false: the shapes the quiet path never produces.
    rewrite(
        &server,
        &format!(
            "{}\n[limits]\nrpm = 600\n",
            admin_config("http://127.0.0.1:1")
        ),
    );
    hangup(&server);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        let body: Value = get(&client, &admin, "/v1/serve/config", Some(ADMIN_TOKEN))
            .await
            .json()
            .await
            .unwrap();
        if body["last_reload"]["outcome"] == "rejected" || std::time::Instant::now() > deadline {
            break body;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(status["last_reload"]["outcome"], "rejected", "{status}");
    assert_eq!(status["in_sync"], false);
    assert!(
        errors(&schema, &status).is_empty(),
        "{:?} in {status}",
        errors(&schema, &status)
    );
    let _ = CALLER_KEY;
}

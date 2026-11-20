//! The admin UI's static pages (note 0051, slice 2): served without a credential and nothing else is,
//! with a policy that allows only this origin (and the token endpoint, when sign-in is set up), a
//! page that never turns data into markup, and a page that calls only routes the API has.

mod common;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use llmshim::serve::jwks::Fetch;
use llmshim_control::api::{router, AppState, ROUTES};
use llmshim_control::sso::{Sso, SsoConfig};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const SHARED: &str = "control-token-0123456789abcdef";

struct NoKeys;

#[async_trait::async_trait]
impl Fetch for NoKeys {
    async fn fetch(&self) -> Result<Vec<u8>, &'static str> {
        Err("network")
    }
}

fn plain() -> axum::Router {
    router(AppState::new(common::store(), SHARED, None))
}

fn with_sso(ui: Value, break_glass: bool) -> Result<axum::Router, String> {
    let config: SsoConfig = serde_json::from_value(json!({
        "issuer": "https://idp.example.test", "audience": "control",
        "jwks_url": "https://idp.example.test/jwks.json", "algorithms": ["EdDSA"],
        "break_glass": break_glass, "ui": ui,
    }))
    .unwrap();
    let sso = Sso::with_fetch(&config, Arc::new(NoKeys))?;
    Ok(router(AppState::with_sso(
        common::store(),
        break_glass.then_some(SHARED),
        None,
        sso,
    )))
}

async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
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
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test]
async fn the_pages_load_without_a_credential_and_carry_the_security_headers() {
    let app = plain();
    for (path, kind) in [
        ("/ui", "text/html"),
        ("/ui/", "text/html"),
        ("/ui/index.html", "text/html"),
        ("/ui/app.js", "text/javascript"),
        ("/ui/style.css", "text/css"),
        ("/ui/config.json", "application/json"),
    ] {
        let (status, headers, body) = call(&app, Method::GET, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with(kind),
            "{path}"
        );
        assert!(!body.is_empty(), "{path}");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{path}");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store", "{path}");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY", "{path}");
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer", "{path}");
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(
            csp.contains("default-src 'none'") && csp.contains("script-src 'self'"),
            "{csp}"
        );
        assert!(
            csp.contains("frame-ancestors 'none'") && csp.contains("base-uri 'none'"),
            "{csp}"
        );
        assert!(!csp.contains("unsafe-"), "{csp}");
    }
    // HEAD is allowed, anything that changes state is not.
    assert_eq!(call(&app, Method::HEAD, "/ui/").await.0, StatusCode::OK);
    for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
        assert_eq!(
            call(&app, method, "/ui/app.js").await.0,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
}

#[tokio::test]
async fn only_the_pages_are_public() {
    let app = plain();
    // An unknown file under /ui is a 404 that says nothing.
    assert_eq!(
        call(&app, Method::GET, "/ui/secret").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, Method::GET, "/ui/../v1/orgs").await.0,
        StatusCode::NOT_FOUND
    );
    // Lookalikes and the API are still behind the token.
    for path in [
        "/uix",
        "/ui.js",
        "/v1/orgs",
        "/v1/requests",
        "/v1/whoami",
        "/metrics",
    ] {
        assert_eq!(
            call(&app, Method::GET, path).await.0,
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
}

#[tokio::test]
async fn the_config_tells_the_page_only_what_is_public() {
    let (_, _, body) = call(&plain(), Method::GET, "/ui/config.json").await;
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"oidc": null, "break_glass": true})
    );

    let ui = json!({
        "client_id": "control-ui",
        "authorization_endpoint": "https://idp.example.test/authorize",
        "token_endpoint": "https://idp.example.test:8443/token",
        "scope": "openid roles",
    });
    let app = with_sso(ui.clone(), false).unwrap();
    let (_, headers, body) = call(&app, Method::GET, "/ui/config.json").await;
    let config: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(config["oidc"], ui);
    assert_eq!(config["break_glass"], false);
    // The token endpoint's origin, and only that, may be reached besides this server.
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(
        csp.contains("connect-src 'self' https://idp.example.test:8443;"),
        "{csp}"
    );
    // No secret ever appears: the shared token is not in any page.
    for path in ["/ui/", "/ui/app.js", "/ui/config.json", "/ui/style.css"] {
        assert!(!call(&app, Method::GET, path).await.2.contains(SHARED));
    }
    // The default scope is `openid`.
    let app = with_sso(
        json!({"client_id": "c", "authorization_endpoint": "https://i.test/a",
        "token_endpoint": "https://i.test/t"}),
        true,
    )
    .unwrap();
    let config: Value =
        serde_json::from_str(&call(&app, Method::GET, "/ui/config.json").await.2).unwrap();
    assert_eq!(config["oidc"]["scope"], "openid");
    assert_eq!(config["break_glass"], true);
}

#[test]
fn a_sign_in_that_could_leak_a_code_or_widen_the_policy_is_refused_at_start() {
    let good = json!({"client_id": "c", "authorization_endpoint": "https://i.test/a", "token_endpoint": "https://i.test/t"});
    assert!(with_sso(good.clone(), true).is_ok());
    for (field, value) in [
        ("token_endpoint", "http://idp.example.test/token"),
        ("token_endpoint", "https://idp.example.test; script-src *"),
        ("token_endpoint", "https://user:pw@idp.example.test/token"),
        ("token_endpoint", "https://*/token"),
        ("token_endpoint", "https://*.example.test/token"),
        (
            "token_endpoint",
            "https://idp.example.test\\@evil.test/token",
        ),
        ("token_endpoint", "https://idp.example.test:notaport/token"),
        ("authorization_endpoint", "javascript:alert(1)"),
        ("authorization_endpoint", "https://idp.example.test/a#frag"),
        ("client_id", ""),
        ("client_id", "has space"),
        ("scope", "openid \"x"),
        ("scope", ""),
    ] {
        let mut ui = good.clone();
        ui[field] = json!(value);
        let error = match with_sso(ui, true) {
            Err(error) => error,
            Ok(_) => panic!("{field}={value} was accepted"),
        };
        assert!(error.starts_with(&format!("ui.{field}")), "{error}");
        // The message names the field and never echoes the value.
        assert!(!error.contains(value) || value.is_empty(), "{error}");
    }
    // Plain http is for a provider on this machine only.
    let mut ui = good;
    ui["token_endpoint"] = json!("http://localhost:8080/token");
    assert!(with_sso(ui, true).is_ok());
}

#[test]
fn the_page_never_turns_data_into_markup_and_loads_nothing_from_elsewhere() {
    let index = include_str!("../ui/index.html");
    let js = include_str!("../ui/app.js");
    let css = include_str!("../ui/style.css");
    // No inline script or style, no event-handler attributes, no other origin.
    assert!(
        !index.contains("<script>") && !index.contains("style="),
        "inline code in the page"
    );
    assert!(
        !index.contains("onclick=") && !index.contains("onload="),
        "handler attribute"
    );
    for (name, text) in [("index.html", index), ("app.js", js), ("style.css", css)] {
        for forbidden in ["http://", "https://cdn", "@import", "url(http"] {
            assert!(!text.contains(forbidden), "{name} contains {forbidden}");
        }
    }
    // Names are typed by callers: they are put on the page as text, never parsed as markup.
    for forbidden in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function",
        "setAttribute(\"style\"",
    ] {
        assert!(!js.contains(forbidden), "app.js contains {forbidden}");
    }
    // The token is held for the tab only.
    assert!(!js.contains("localStorage") && !js.contains("document.cookie"));
}

#[test]
fn every_route_the_page_calls_is_one_the_api_has() {
    let js = include_str!("../ui/app.js");
    let mut called = Vec::new();
    for piece in js.split("api(\"").skip(1) {
        let path = &piece[..piece.find('"').unwrap()];
        called.push(path.to_owned());
    }
    assert!(called.len() >= 5, "found {called:?}");
    for path in called {
        assert!(
            ROUTES
                .iter()
                .any(|(method, route)| *method == "get" && *route == path),
            "the page calls GET {path}, which is not an API route"
        );
    }
}

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
        ("/ui/manage.js", "text/javascript"),
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
    let scripts = [
        // In the order the page loads them: manage.js defines the views app.js lists.
        ("manage.js", include_str!("../ui/manage.js")),
        ("app.js", include_str!("../ui/app.js")),
    ];
    let css = include_str!("../ui/style.css");
    // Only script files of this server, no inline code of any form, no style attribute or element.
    let elements: Vec<&str> = index.split("<script").skip(1).collect();
    assert_eq!(elements.len(), scripts.len(), "the script elements");
    for (element, (name, _)) in elements.iter().zip(&scripts) {
        let expected = format!("src=\"/ui/{name}\"");
        assert!(
            element.trim_start().starts_with(&expected),
            "{name}: an inline or foreign script"
        );
    }
    assert!(
        !index.contains("style=") && !index.contains("<style"),
        "inline style in the page"
    );
    assert!(
        !index.contains("onclick=") && !index.contains("onload="),
        "handler attribute"
    );
    let mut all = vec![("index.html", index), ("style.css", css)];
    all.extend(scripts.iter().copied());
    for (name, text) in all {
        for forbidden in ["http://", "https://cdn", "@import", "url(http"] {
            assert!(!text.contains(forbidden), "{name} contains {forbidden}");
        }
    }
    // Names are typed by callers: they are put on the page as text, never parsed as markup.
    for (name, js) in scripts {
        for forbidden in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "eval(",
            "new Function",
            "setAttribute(\"style\"",
            "localStorage",
            "document.cookie",
            "window.open",
            "location.href",
        ] {
            assert!(!js.contains(forbidden), "{name} contains {forbidden}");
        }
    }
}

#[test]
fn every_route_the_page_calls_is_one_the_api_has() {
    // Every "/v1/…" string in the scripts is a route, or the fixed start of one that takes an id.
    let mut literals = Vec::new();
    for js in [
        include_str!("../ui/app.js"),
        include_str!("../ui/manage.js"),
    ] {
        for piece in js.split("\"/v1/").skip(1) {
            literals.push(format!("/v1/{}", &piece[..piece.find('"').unwrap()]));
        }
    }
    assert!(literals.len() >= 12, "found {literals:?}");
    for path in &literals {
        assert!(
            ROUTES.iter().any(|(_, route)| {
                route == &path.as_str()
                    || route
                        .split('{')
                        .next()
                        .is_some_and(|start| start == path && route.contains('{'))
            }),
            "the page calls {path}, which is not an API route"
        );
    }
    // The calls that change something name a method the API has on that path.
    for (verb, path) in [
        ("POST", "/v1/orgs"),
        ("POST", "/v1/orgs/{org}/teams"),
        ("PUT", "/v1/teams/{team}/budget"),
        ("DELETE", "/v1/teams/{team}/budget"),
        ("POST", "/v1/teams/{team}/projects"),
        ("POST", "/v1/projects/{project}/keys"),
        ("POST", "/v1/keys/{key}/revoke"),
        ("POST", "/v1/keys/{key}/rotate"),
        ("PATCH", "/v1/keys/{key}/limits"),
    ] {
        assert!(
            ROUTES
                .iter()
                .any(|(m, r)| m.eq_ignore_ascii_case(verb) && *r == path),
            "{verb} {path} is not an API route"
        );
    }
    let manage = include_str!("../ui/manage.js");
    // The fixed tails the script appends to an id ("/revoke", "/limits"…) are real last segments of routes.
    let tails: Vec<&str> = manage
        .split('"')
        .filter(|p| {
            p.len() > 1 && p.starts_with('/') && p[1..].bytes().all(|b| b.is_ascii_lowercase())
        })
        .collect();
    assert!(tails.len() >= 6, "found {tails:?}");
    for tail in tails {
        assert!(
            ROUTES.iter().any(|(_, route)| route.ends_with(tail)),
            "the script appends {tail}, which ends no route"
        );
    }
    for verb in ["\"POST\"", "\"PUT\"", "\"DELETE\"", "\"PATCH\""] {
        assert!(
            manage.contains(&format!("send({verb}")),
            "the script no longer sends {verb}"
        );
    }
}

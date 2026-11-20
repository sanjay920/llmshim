//! The admin UI's static pages (design note 0051, slice 2).
//!
//! Three files compiled into the binary and served without a credential, because a page must load
//! before a person can sign in; they hold no data. Everything the page shows comes from the JSON API with
//! the person's own bearer token, so the API's roles are what gate it. The page gets only what is
//! public from `/ui/config.json`: whether sign-in through the identity provider is set up (its client id
//! and endpoints) and whether pasting the break-glass token is offered.

use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const STYLE: &str = include_str!("../ui/style.css");

/// The browser's half of single sign-on: a public client doing authorization code with PKCE. Given as
/// the `ui` object of the issuer file. Without it the page offers the token paste only.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UiOidc {
    pub client_id: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// Space-separated; `openid` by default.
    #[serde(default)]
    pub scope: Option<String>,
}

/// The origin (`scheme://host[:port]`) of an `https` URL, or of an `http` one only for loopback (a
/// development provider). `None` for anything else, including a URL with credentials or a fragment.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "https" && scheme != "http" {
        return None;
    }
    if url.contains('#')
        || url
            .chars()
            .any(|c| c.is_control() || c == ' ' || c == ';' || c == ',')
    {
        return None;
    }
    let authority = rest.split(['/', '?']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let host = if authority.starts_with('[') {
        authority
            .split_once(']')
            .map_or(authority, |(h, _)| &authority[..=h.len()])
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    if scheme == "http" && !loopback {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

impl UiOidc {
    /// Every problem by field, never with a value.
    pub fn validate(&self) -> Result<(), String> {
        let visible = |s: &str| {
            !s.is_empty() && s.len() <= 256 && s.bytes().all(|b| (0x21..0x7f).contains(&b))
        };
        if !visible(&self.client_id) {
            return Err("ui.client_id: must be 1 to 256 visible ASCII characters".into());
        }
        for (field, value) in [
            ("authorization_endpoint", &self.authorization_endpoint),
            ("token_endpoint", &self.token_endpoint),
        ] {
            if origin_of(value).is_none() {
                return Err(format!(
                    "ui.{field}: must be an https URL (http only for localhost) without credentials or a fragment"
                ));
            }
        }
        if let Some(scope) = &self.scope {
            if scope.is_empty()
                || scope.len() > 256
                || !scope
                    .bytes()
                    .all(|b| b == b' ' || (0x21..0x7f).contains(&b) && b != b'"' && b != b'\\')
            {
                return Err("ui.scope: must be 1 to 256 visible ASCII characters".into());
            }
        }
        Ok(())
    }
}

/// What the page is served with, built once.
pub struct Ui {
    config_json: String,
    csp: String,
}

impl Ui {
    /// `oidc` must have passed [`UiOidc::validate`].
    pub fn new(oidc: Option<&UiOidc>, break_glass: bool) -> Self {
        let mut connect = String::from("'self'");
        if let Some(oidc) = oidc {
            // The page exchanges the code at the token endpoint, so that origin, and only that one, may
            // be reached besides this server.
            if let Some(origin) = origin_of(&oidc.token_endpoint) {
                connect.push(' ');
                connect.push_str(&origin);
            }
        }
        // Own origin for everything; no inline script or style, no framing, no base tag, no form posts.
        let csp = format!(
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             connect-src {connect}; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
        );
        let config_json = json!({
            "oidc": oidc.map(|o| json!({
                "client_id": o.client_id,
                "authorization_endpoint": o.authorization_endpoint,
                "token_endpoint": o.token_endpoint,
                "scope": o.scope.as_deref().unwrap_or("openid"),
            })),
            "break_glass": break_glass,
        })
        .to_string();
        Self { config_json, csp }
    }

    pub fn csp(&self) -> &str {
        &self.csp
    }

    /// The page, script, style or config for `uri`; `None` for any other path under `/ui`.
    pub fn respond(&self, method: &Method, uri: &Uri) -> Response {
        let (body, kind): (&str, &'static str) = match uri.path() {
            "/ui" | "/ui/" | "/ui/index.html" => (INDEX, "text/html; charset=utf-8"),
            "/ui/app.js" => (APP_JS, "text/javascript; charset=utf-8"),
            "/ui/style.css" => (STYLE, "text/css; charset=utf-8"),
            "/ui/config.json" => (&self.config_json, "application/json"),
            _ => {
                return (StatusCode::NOT_FOUND, "not found").into_response();
            }
        };
        if method != Method::GET && method != Method::HEAD {
            let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return response;
        }
        let mut response = (StatusCode::OK, body.to_owned()).into_response();
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
        // The page is small and changes with the binary: never cached, so an upgrade is seen at once.
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        if let Ok(csp) = HeaderValue::from_str(&self.csp) {
            headers.insert(header::CONTENT_SECURITY_POLICY, csp);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_https_or_loopback_and_carry_no_credentials() {
        assert_eq!(
            origin_of("https://idp.example.com/oauth/token").as_deref(),
            Some("https://idp.example.com")
        );
        assert_eq!(
            origin_of("https://idp.example.com:8443/t?x=1").as_deref(),
            Some("https://idp.example.com:8443")
        );
        assert_eq!(
            origin_of("http://localhost:8080/t").as_deref(),
            Some("http://localhost:8080")
        );
        for bad in [
            "http://idp.example.com/t",
            "https://u:p@idp.example.com/t",
            "https://idp.example.com/t#frag",
            "ftp://idp.example.com",
            "https://",
            "https://idp.example.com; script-src *",
            "https://idp.example.com/a b",
            "idp.example.com",
        ] {
            assert_eq!(origin_of(bad), None, "{bad}");
        }
    }
}

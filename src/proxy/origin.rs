use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::env;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

pub(crate) const TRUSTED_ORIGINS_ENV: &str = "LLMSHIM_TRUSTED_ORIGINS";

/// Browser origins explicitly allowed to use the local HTTP server.
#[derive(Clone, Debug, Default)]
pub(crate) struct OriginPolicy {
    trusted_origins: Vec<HeaderValue>,
}

impl OriginPolicy {
    pub(crate) fn from_env() -> Self {
        match env::var(TRUSTED_ORIGINS_ENV) {
            Ok(configured_origins) => match Self::from_csv(&configured_origins) {
                Ok(policy) => policy,
                Err(error) => {
                    eprintln!(
                        "Ignoring {TRUSTED_ORIGINS_ENV}: {error}. Browser-origin requests will be rejected."
                    );
                    Self::default()
                }
            },
            Err(env::VarError::NotPresent) => Self::default(),
            Err(env::VarError::NotUnicode(_)) => {
                eprintln!(
                    "Ignoring {TRUSTED_ORIGINS_ENV}: value is not valid Unicode. Browser-origin requests will be rejected."
                );
                Self::default()
            }
        }
    }

    pub(crate) fn from_csv(configured_origins: &str) -> Result<Self, String> {
        let mut trusted_origins = Vec::new();
        for (configured_origin_index, configured_origin) in
            configured_origins.split(',').enumerate()
        {
            let origin_entry_number = configured_origin_index + 1;
            let trimmed_origin = configured_origin.trim();
            if trimmed_origin.is_empty() {
                return Err(format!("origin entry {origin_entry_number} is empty"));
            }
            let parsed_origin = reqwest::Url::parse(trimmed_origin)
                .map_err(|_| format!("origin entry {origin_entry_number} is not a valid origin"))?;
            if !matches!(parsed_origin.scheme(), "http" | "https")
                || !parsed_origin.username().is_empty()
                || parsed_origin.password().is_some()
                || parsed_origin.path() != "/"
                || parsed_origin.query().is_some()
                || parsed_origin.fragment().is_some()
            {
                return Err(format!(
                    "origin entry {origin_entry_number} must be an http or https origin without credentials, a path, query, or fragment"
                ));
            }
            let serialized_origin = parsed_origin.origin().ascii_serialization();
            let origin_header = HeaderValue::try_from(serialized_origin.as_str())
                .map_err(|_| format!("origin entry {origin_entry_number} is not a valid origin"))?;
            if !trusted_origins.contains(&origin_header) {
                trusted_origins.push(origin_header);
            }
        }
        if trusted_origins.is_empty() {
            return Err("at least one origin is required".into());
        }
        Ok(Self { trusted_origins })
    }

    fn contains(&self, request_origin: &HeaderValue) -> bool {
        self.trusted_origins.contains(request_origin)
    }

    pub(crate) fn cors_layer(&self) -> CorsLayer {
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(self.trusted_origins.clone()))
            .allow_methods(Any)
            .allow_headers(Any)
            .expose_headers(Any)
            .allow_private_network(true)
    }
}

/// Refuse browser-originated requests before they can reach a route handler.
pub(crate) async fn admit_browser_origin(
    origin_policy: OriginPolicy,
    request: Request,
    next: Next,
) -> Response {
    let mut request_origins = request.headers().get_all(header::ORIGIN).iter();
    match (request_origins.next(), request_origins.next()) {
        (None, _) => next.run(request).await,
        (Some(request_origin), None) if origin_policy.contains(request_origin) => {
            next.run(request).await
        }
        _ => StatusCode::FORBIDDEN.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::OriginPolicy;

    #[test]
    fn accepts_only_serialized_http_origins() {
        let policy =
            OriginPolicy::from_csv("https://app.example, http://localhost:5173, https://[::1]")
                .expect("valid origins");
        assert_eq!(policy.trusted_origins.len(), 3);
        assert_eq!(policy.trusted_origins[0], "https://app.example");
        assert_eq!(policy.trusted_origins[1], "http://localhost:5173");
        assert_eq!(policy.trusted_origins[2], "https://[::1]");
    }

    #[test]
    fn rejects_wildcards_opaque_and_non_origin_values() {
        for configured_origins in [
            "*",
            "null",
            "file:///tmp/app.html",
            "https://app.example/path",
            "https://app.example?query=value",
            "https://user@app.example",
            "https://app.example,",
        ] {
            assert!(
                OriginPolicy::from_csv(configured_origins).is_err(),
                "{configured_origins} should be rejected"
            );
        }
    }

    #[test]
    fn rejected_configuration_diagnostics_do_not_echo_origin_secrets() {
        let synthetic_origin_secret = "origin-password-must-not-appear";
        let configured_origins = format!("https://user:{synthetic_origin_secret}@app.example");
        let diagnostic = OriginPolicy::from_csv(&configured_origins).expect_err("invalid origin");
        assert_eq!(
            diagnostic,
            "origin entry 1 must be an http or https origin without credentials, a path, query, or fragment"
        );
        assert!(!diagnostic.contains(synthetic_origin_secret));
    }
}

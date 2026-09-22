use llmshim_catalog::{Catalog, CatalogHandle, CatalogOptions, RefreshOutcome, Support};
use serde_json::json;
use std::time::Duration;

fn options(dir: &tempfile::TempDir, url: String) -> CatalogOptions {
    CatalogOptions {
        cache_file: Some(dir.path().join("models.dev.json")),
        global_overrides: None,
        project_overrides: None,
        offline: false,
        ttl: Duration::from_secs(86400),
        url,
    }
}
fn fixture() -> String {
    json!({"fixture": {"models":{"fresh-model":{"tool_call":true,"family":"qwen","cost":{"input":1}}}}}).to_string()
}

#[tokio::test]
async fn refresh_etag_ttl_and_not_modified_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = mockito::Server::new_async().await;
    let first = server
        .mock("GET", "/catalog")
        .match_header("if-none-match", mockito::Matcher::Missing)
        .with_header("etag", "\"revision-1\"")
        .with_body(fixture())
        .expect(1)
        .create_async()
        .await;
    let opts = options(&dir, format!("{}/catalog", server.url()));
    let handle = CatalogHandle::load(opts.clone()).unwrap();
    assert!(handle.snapshot().resolve("fixture/fresh-model").is_none());
    assert!(matches!(
        handle.refresh(false).await,
        RefreshOutcome::Updated { models: 1 }
    ));
    assert_eq!(handle.refresh(false).await, RefreshOutcome::Fresh);
    assert_eq!(
        handle
            .snapshot()
            .resolve("fixture/fresh-model")
            .unwrap()
            .capabilities
            .tools,
        Support::Supported
    );
    first.assert_async().await;
    let next = server
        .mock("GET", "/catalog")
        .match_header("if-none-match", "\"revision-1\"")
        .with_status(304)
        .expect(1)
        .create_async()
        .await;
    let fresh_process = CatalogHandle::load(opts.clone()).unwrap();
    assert_eq!(fresh_process.refresh(false).await, RefreshOutcome::Fresh);
    assert_eq!(
        fresh_process.refresh(true).await,
        RefreshOutcome::NotModified
    );
    next.assert_async().await;
    assert_eq!(
        std::fs::read_to_string(opts.cache_file.unwrap()).unwrap(),
        fixture()
    );
}

#[tokio::test]
async fn offline_ignores_cache_and_contacts_neither_catalog_nor_provider() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = mockito::Server::new_async().await;
    let never = server
        .mock("GET", mockito::Matcher::Any)
        .expect(0)
        .create_async()
        .await;
    let mut opts = options(&dir, server.url());
    std::fs::write(opts.cache_file.as_ref().unwrap(), fixture()).unwrap();
    let local = dir.path().join("models.toml");
    std::fs::write(&local, "[models.\"offline/local\"]\nfamily=\"llama\"").unwrap();
    opts.offline = true;
    opts.global_overrides = Some(local);
    let h = CatalogHandle::load(opts).unwrap();
    assert!(h.snapshot().resolve("fixture/fresh-model").is_none());
    assert!(h.snapshot().resolve("offline/local").is_some());
    assert_eq!(h.refresh(true).await, RefreshOutcome::Offline);
    assert!(h.refresh_in_background().is_none());
    assert_eq!(
        h.discover_provider("test", &server.url(), Some("secret-test-token"))
            .await
            .unwrap(),
        RefreshOutcome::Offline
    );
    never.assert_async().await;
}

#[tokio::test]
async fn failures_serve_stale_indefinitely_without_replacing_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = mockito::Server::new_async().await;
    let opts = options(&dir, server.url());
    std::fs::write(opts.cache_file.as_ref().unwrap(), fixture()).unwrap();
    let h = CatalogHandle::load(opts.clone()).unwrap();
    for (status, body) in [
        (503, "unavailable"),
        (200, "{broken"),
        (200, "{}"),
        (304, ""),
    ] {
        // No validated metadata/ETag exists, so a bare 304 is a protocol failure.
        let m = server
            .mock("GET", "/")
            .with_status(status)
            .with_body(body)
            .create_async()
            .await;
        let outcome = h.refresh(true).await;
        assert!(
            matches!(outcome, RefreshOutcome::Stale { .. }),
            "{outcome:?}"
        );
        assert!(h.snapshot().resolve("fixture/fresh-model").is_some());
        assert_eq!(
            std::fs::read_to_string(opts.cache_file.as_ref().unwrap()).unwrap(),
            fixture()
        );
        m.assert_async().await;
        m.remove_async().await;
    }
}

#[tokio::test]
async fn catalog_transport_errors_do_not_expose_query_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = mockito::Server::new_async().await;
    let query_credential = "synthetic-catalog-query-credential";
    let catalog_url = format!("{}/catalog?key={query_credential}", server.url());
    let failed_catalog = server
        .mock("GET", "/catalog")
        .match_query(mockito::Matcher::UrlEncoded(
            "key".into(),
            query_credential.into(),
        ))
        .with_status(503)
        .expect(1)
        .create_async()
        .await;
    let handle = CatalogHandle::load(options(&dir, catalog_url)).unwrap();
    let RefreshOutcome::Stale { reason } = handle.refresh(true).await else {
        panic!("catalog request should have remained stale");
    };
    assert!(!reason.contains(query_credential));
    assert!(!reason.contains("/catalog"));
    failed_catalog.assert_async().await;

    let failed_provider = server
        .mock("GET", "/models")
        .match_query(mockito::Matcher::UrlEncoded(
            "key".into(),
            query_credential.into(),
        ))
        .with_status(503)
        .expect(1)
        .create_async()
        .await;
    let error = handle
        .discover_provider(
            "fixture",
            &format!("{}/models?key={query_credential}", server.url()),
            Some("synthetic-bearer-credential"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains(query_credential));
    assert!(!error.contains("synthetic-bearer-credential"));
    assert!(!error.contains("/models"));
    failed_provider.assert_async().await;
}

#[tokio::test]
async fn provider_discovery_does_not_follow_redirects_or_forward_bearer_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = mockito::Server::new_async().await;
    let mut redirected = mockito::Server::new_async().await;
    let redirected_request = redirected
        .mock("GET", mockito::Matcher::Any)
        .expect(0)
        .create_async()
        .await;
    let redirect = source
        .mock("GET", "/models")
        .match_header("authorization", "Bearer synthetic-bearer-credential")
        .with_status(302)
        .with_header("location", &format!("{}/models", redirected.url()))
        .expect(1)
        .create_async()
        .await;
    let handle = CatalogHandle::load(options(&dir, source.url())).unwrap();
    let error = handle
        .discover_provider(
            "fixture",
            &format!("{}/models", source.url()),
            Some("synthetic-bearer-credential"),
        )
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "invalid catalog: provider catalog server returned an error"
    );
    redirect.assert_async().await;
    redirected_request.assert_async().await;
}

#[test]
fn corrupt_cache_and_partial_metadata_fall_back_to_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let opts = options(&dir, "http://127.0.0.1:1".into());
    std::fs::write(opts.cache_file.as_ref().unwrap(), "{broken").unwrap();
    std::fs::write(dir.path().join("models.dev.metadata.json"), "{}").unwrap();
    let c = Catalog::load(&opts).unwrap();
    assert!(c.resolve("anthropic/claude-opus-5").is_some());
}

#[test]
fn project_override_wins_over_global_and_invalid_local_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(&dir, "http://127.0.0.1:1".into());
    let global = dir.path().join("global.toml");
    let project = dir.path().join("project.toml");
    std::fs::write(
        &global,
        "[models.\"openai/gpt-6-astra\"]\ncontext_window_tokens=11",
    )
    .unwrap();
    std::fs::write(
        &project,
        "[models.\"openai/gpt-6-astra\"]\ncontext_window_tokens=22",
    )
    .unwrap();
    opts.global_overrides = Some(global);
    opts.project_overrides = Some(project.clone());
    assert_eq!(
        Catalog::load(&opts)
            .unwrap()
            .resolve("gpt-6-astra")
            .unwrap()
            .context_window_tokens,
        Some(22)
    );
    std::fs::write(&project, "invalid [").unwrap();
    assert!(Catalog::load(&opts).is_err());
}

#[tokio::test]
async fn provider_discovery_requires_explicit_call_preserves_local_and_rejects_prices() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = mockito::Server::new_async().await;
    let opts = options(&dir, format!("{}/catalog", server.url()));
    let h = CatalogHandle::load(opts).unwrap();
    let provider = server
        .mock("GET", "/v1/models")
        .match_header("authorization", "Bearer test-token")
        .with_body(
            json!({"data":[{"id":"local","tool_call":true,"cost":{"input":999}}]}).to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    assert!(matches!(
        h.discover_provider(
            "vllm",
            &format!("{}/v1/models", server.url()),
            Some("test-token")
        )
        .await
        .unwrap(),
        RefreshOutcome::Updated { models: 1 }
    ));
    let refresh = server
        .mock("GET", "/catalog")
        .with_body(fixture())
        .create_async()
        .await;
    h.refresh(true).await;
    let snap = h.snapshot();
    let m = snap.resolve("vllm/local").unwrap();
    assert!(m.cost.is_none());
    assert_eq!(m.capabilities.tools, Support::Supported);
    provider.assert_async().await;
    refresh.assert_async().await;
}

#[tokio::test]
async fn snapshot_remains_accessible_while_refresh_is_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("GET", "/")
        .with_body(fixture())
        .create_async()
        .await;
    let h = CatalogHandle::load(options(&dir, server.url())).unwrap();
    let old = h.snapshot();
    let task = h.refresh_in_background().unwrap();
    assert!(h.snapshot().resolve("gpt-6-astra").is_some());
    assert!(matches!(
        task.await.unwrap(),
        RefreshOutcome::Updated { .. }
    ));
    assert!(old.resolve("fixture/fresh-model").is_none());
    assert!(h.snapshot().resolve("fixture/fresh-model").is_some());
    m.assert_async().await;
}

use chrono::Utc;
use llmshim_catalog::{
    Catalog, CatalogError, CatalogHandle, CatalogOptions, ModelInfo, RefreshOutcome,
};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

const LIMIT_ERROR: &str = "catalog import exceeds derived identity limits";

fn oversized_feed(provider: &str) -> String {
    let models: Map<String, Value> = (0..256)
        .map(|index| (format!("m{index}"), json!({})))
        .collect();
    json!({provider: {"models": models}}).to_string()
}

fn options(directory: &tempfile::TempDir, url: String) -> CatalogOptions {
    CatalogOptions {
        cache_file: Some(directory.path().join("catalog.json")),
        global_overrides: None,
        project_overrides: None,
        offline: false,
        ttl: Duration::ZERO,
        url,
    }
}

#[test]
fn amplified_import_rejection_preserves_existing_models() {
    let provider = "p".repeat(65_536);
    let feed = oversized_feed(&provider);
    assert!(feed.len() < 70 * 1024);
    let mut catalog = Catalog::empty();
    catalog.merge_model(ModelInfo::new("trusted", "kept"));
    let before = serde_json::to_value(catalog.models().collect::<Vec<_>>()).unwrap();
    assert!(matches!(
        catalog.merge_models_dev(&feed, None),
        Err(CatalogError::Invalid(LIMIT_ERROR))
    ));
    assert_eq!(
        serde_json::to_value(catalog.models().collect::<Vec<_>>()).unwrap(),
        before
    );

    let duplicate_rows = json!({"data": (0..256)
        .map(|_| json!({"id": "same-model"}))
        .collect::<Vec<_>>()});
    assert!(matches!(
        catalog.merge_provider_models(&provider, &duplicate_rows, Utc::now()),
        Err(CatalogError::Invalid(LIMIT_ERROR))
    ));
    assert_eq!(catalog.models().count(), 1);
    assert!(catalog.resolve("trusted/kept").is_some());
}

#[test]
fn accepted_opaque_identities_and_lookup_spellings_remain_exact() {
    let provider = "p".repeat(8_192);
    let model_name = "opaque/model.v1";
    let qualified_id = format!("{provider}/{model_name}");
    let mut catalog = Catalog::empty();
    catalog
        .merge_models_dev(
            &json!({provider.clone(): {"models": {model_name: {}}}}).to_string(),
            None,
        )
        .unwrap();
    let model = catalog.resolve(&qualified_id).unwrap();
    assert_eq!(model.provider, provider);
    assert_eq!(model.name, model_name);
    assert_eq!(model.id, qualified_id);

    catalog
        .merge_models_dev(
            &json!({"anthropic": {"models": {"claude-1.1": {}}},
                "google": {"models": {"global.fixture": {}}}})
            .to_string(),
            None,
        )
        .unwrap();
    assert_eq!(
        catalog.resolve("anthropic/claude-1.1").unwrap().name,
        "claude-1-1"
    );
    assert_eq!(
        catalog.resolve("google/fixture").unwrap().id,
        "gemini/global.fixture"
    );
}

#[tokio::test]
async fn refused_refresh_preserves_snapshot_and_cached_body() {
    let directory = tempfile::tempdir().unwrap();
    let mut server = mockito::Server::new_async().await;
    let catalog_options = options(&directory, format!("{}/catalog", server.url()));
    let cached_body = json!({"fixture": {"models": {"kept": {}}}}).to_string();
    std::fs::write(catalog_options.cache_file.as_ref().unwrap(), &cached_body).unwrap();
    let handle = CatalogHandle::load(catalog_options.clone()).unwrap();
    let snapshot = handle.snapshot();
    let response = server
        .mock("GET", "/catalog")
        .with_body(oversized_feed(&"p".repeat(65_536)))
        .expect(1)
        .create_async()
        .await;
    assert_eq!(
        handle.refresh(true).await,
        RefreshOutcome::Stale {
            reason: "invalid catalog: catalog import exceeds derived identity limits".into()
        }
    );
    assert!(Arc::ptr_eq(&snapshot, &handle.snapshot()));
    assert!(!catalog_options
        .cache_file
        .as_ref()
        .unwrap()
        .with_extension("metadata.json")
        .exists());
    assert_eq!(
        std::fs::read_to_string(catalog_options.cache_file.unwrap()).unwrap(),
        cached_body
    );
    response.assert_async().await;
}

#[tokio::test]
async fn refused_discovery_does_not_poison_the_saved_provider_layer() {
    let directory = tempfile::tempdir().unwrap();
    let mut server = mockito::Server::new_async().await;
    let handle =
        CatalogHandle::load(options(&directory, format!("{}/catalog", server.url()))).unwrap();
    let provider = "p".repeat(65_536);
    let first_response = server
        .mock("GET", "/models-good")
        .with_body(json!({"data": [{"id": "kept"}]}).to_string())
        .expect(1)
        .create_async()
        .await;
    assert_eq!(
        handle
            .discover_provider(&provider, &format!("{}/models-good", server.url()), None)
            .await
            .unwrap(),
        RefreshOutcome::Updated { models: 1 }
    );
    let snapshot = handle.snapshot();
    let duplicate_rows = json!({"data": (0..256)
        .map(|_| json!({"id": "uncommitted"}))
        .collect::<Vec<_>>()});
    let refused_response = server
        .mock("GET", "/models-bad")
        .with_body(duplicate_rows.to_string())
        .expect(1)
        .create_async()
        .await;
    assert!(matches!(
        handle
            .discover_provider(&provider, &format!("{}/models-bad", server.url()), None)
            .await,
        Err(CatalogError::Invalid(LIMIT_ERROR))
    ));
    assert!(Arc::ptr_eq(&snapshot, &handle.snapshot()));

    let refreshed_response = server
        .mock("GET", "/catalog")
        .with_body(json!({"fixture": {"models": {"fresh": {}}}}).to_string())
        .expect(1)
        .create_async()
        .await;
    assert_eq!(
        handle.refresh(true).await,
        RefreshOutcome::Updated { models: 1 }
    );
    assert!(handle
        .snapshot()
        .resolve(&format!("{provider}/kept"))
        .is_some());
    assert!(handle
        .snapshot()
        .resolve(&format!("{provider}/uncommitted"))
        .is_none());
    first_response.assert_async().await;
    refused_response.assert_async().await;
    refreshed_response.assert_async().await;
}

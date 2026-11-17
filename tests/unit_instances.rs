//! Named instances of a hosted provider (note 0032): the kind and the admission identity differ.

use llmshim::breaker::{BreakerConfig, ProviderBreaker};
use llmshim::deployment::{Deployment, Strategy, Target};
use llmshim::providers::openai::OpenAi;
use llmshim::router::Router;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn router(breaker: Arc<ProviderBreaker>) -> Router {
    Router::new()
        .with_breaker(breaker)
        .register(
            "openai",
            Box::new(OpenAi::new("key-a".into()).with_instance("openai")),
        )
        .register(
            "openai-b",
            Box::new(OpenAi::new("key-b".into()).with_instance("openai-b")),
        )
        .deployment(
            "chat",
            Deployment::new(
                Strategy::RoundRobin,
                vec![
                    Target {
                        model: "openai/gpt-6-luna".into(),
                        weight: 1,
                    },
                    Target {
                        model: "openai-b/gpt-6-luna".into(),
                        weight: 1,
                    },
                ],
            ),
        )
}

#[test]
fn an_instance_is_admitted_by_its_own_name_and_is_the_kind_to_everything_else() {
    let router = router(Arc::new(ProviderBreaker::new()));
    let (second, model) = router.resolve("openai-b/gpt-6-luna").unwrap();
    assert_eq!(second.instance(), "openai-b");
    assert_eq!(
        second.name(),
        "openai",
        "pricing, replay and tools read the kind"
    );
    assert_eq!(model, "gpt-6-luna");
    let (first, _) = router.resolve("openai/gpt-6-luna").unwrap();
    assert_eq!((first.instance(), first.name()), ("openai", "openai"));
    // A bare name still goes to the kind's own entry, never to an instance.
    let (bare, _) = router.resolve("gpt-6-luna").unwrap();
    assert_eq!(bare.instance(), "openai");
}

#[test]
fn the_kind_and_its_instance_resolve_a_model_alike() {
    let router = router(Arc::new(ProviderBreaker::new()));
    // Parity, not canonicalization: no built-in spelling differs from its id today, so this cannot
    // show the catalog being asked about the kind; it shows the two never disagree (the resolver
    // keeps the instance as the key and asks the kind's catalog whenever the two could differ).
    for spelling in [
        "gpt-6-luna",
        "GPT-6-LUNA",
        "gpt-6-Luna",
        "gpt-6-luna-2026-10-01",
    ] {
        let first = router.resolve(&format!("openai/{spelling}")).unwrap().1;
        let second = router.resolve(&format!("openai-b/{spelling}")).unwrap().1;
        assert_eq!(first, second, "{spelling}");
    }
}

#[test]
fn the_dispatch_target_carries_the_instance_only_when_it_differs_from_the_kind() {
    let router = router(Arc::new(ProviderBreaker::new()));
    let request = json!({"model": "gpt-6-luna",
        "messages": [{"role": "user", "content": "hi"}]});
    let target = |key: &str| {
        let (provider, model) = router.resolve(&format!("{key}/gpt-6-luna")).unwrap();
        let native = provider.transform_request(&model, &request).unwrap();
        provider.request_replay_target(&model, &native)
    };
    assert_eq!(target("openai").instance, None);
    let second = target("openai-b");
    assert_eq!(second.instance.as_deref(), Some("openai-b"));
    assert_eq!(
        second.provider, "openai",
        "the target stays the kind for replay and pricing"
    );
}

#[test]
fn one_accounts_open_circuit_does_not_skip_the_other() {
    let breaker = Arc::new(ProviderBreaker::with_config(BreakerConfig {
        window: Duration::from_secs(60),
        trip_threshold: 1,
        cooldown: Duration::from_secs(3600),
    }));
    let router = router(breaker.clone());
    let picked = || {
        router
            .expand_route(&json!({"model": "deployment/chat"}))
            .unwrap()["model"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // The second account fails: its circuit (keyed by its instance) opens, and the first is
    // not affected, as it would be if both reported the kind "openai".
    breaker.record_failure("openai-b", true, Instant::now());
    assert!(breaker.should_skip("openai-b"));
    assert!(!breaker.should_skip("openai"));
    assert!((0..6).all(|_| picked() == "openai/gpt-6-luna"));
    // And the other way round.
    let breaker = Arc::new(ProviderBreaker::with_config(BreakerConfig {
        window: Duration::from_secs(60),
        trip_threshold: 1,
        cooldown: Duration::from_secs(3600),
    }));
    let router = self::router(breaker.clone());
    breaker.record_failure("openai", true, Instant::now());
    assert!((0..6).all(|_| {
        router
            .expand_route(&json!({"model": "deployment/chat"}))
            .unwrap()["model"]
            == "openai-b/gpt-6-luna"
    }));
}

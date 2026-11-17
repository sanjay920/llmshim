//! Deployments in the router (note 0031): one pick per request, in `expand_route`, with health.

use llmshim::breaker::{BreakerConfig, ProviderBreaker};
use llmshim::deployment::{Deployment, Strategy, Target};
use llmshim::providers::openai_compat::OpenAiCompatible;
use llmshim::router::Router;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn target(model: &str, weight: u32) -> Target {
    Target {
        model: model.into(),
        weight,
    }
}

fn router(strategy: Strategy, breaker: Arc<ProviderBreaker>) -> Router {
    Router::new()
        .with_breaker(breaker)
        .register(
            "east",
            Box::new(OpenAiCompatible::new("east", "http://127.0.0.1:1/v1", None)),
        )
        .register(
            "west",
            Box::new(OpenAiCompatible::new("west", "http://127.0.0.1:2/v1", None)),
        )
        .deployment(
            "chat",
            Deployment::new(strategy, vec![target("east/m", 1), target("west/m", 1)]),
        )
}

fn request() -> Value {
    json!({"model": "deployment/chat", "messages": [{"role": "user", "content": "hi"}]})
}

fn picked(router: &Router) -> String {
    router.expand_route(&request()).unwrap()["model"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_deployment_resolves_once_to_a_concrete_target_and_the_name_never_resolves_again() {
    let router = router(Strategy::RoundRobin, Arc::new(ProviderBreaker::new()));
    let first = picked(&router);
    assert!(first == "east/m" || first == "west/m");
    // The expanded request names the target: resolving it picks nothing and finds the provider.
    let (provider, model) = router.resolve(&first).unwrap();
    assert_eq!(
        (provider.name(), model.as_str()),
        (first.split('/').next().unwrap(), "m")
    );
    // The name itself is refused wherever resolution (not expansion) sees it: a second, stateful pick
    // must not happen by accident.
    let error = router.resolve("deployment/chat").err().unwrap().to_string();
    assert!(
        error.contains("resolved when the request is prepared"),
        "{error}"
    );
    // An unknown name is a 400 that names what is configured.
    let unknown = router
        .expand_route(&json!({"model": "deployment/nope"}))
        .err()
        .unwrap()
        .to_string();
    assert!(
        unknown.contains("unknown deployment") && unknown.contains("chat"),
        "{unknown}"
    );
    // A request that names no deployment is untouched.
    let plain = json!({"model": "east/m"});
    assert_eq!(router.expand_route(&plain).unwrap().into_owned(), plain);
}

#[test]
fn round_robin_alternates_and_an_open_circuit_is_skipped() {
    let breaker = Arc::new(ProviderBreaker::with_config(BreakerConfig {
        window: Duration::from_secs(60),
        trip_threshold: 2,
        cooldown: Duration::from_secs(3600),
    }));
    let router = router(Strategy::RoundRobin, breaker.clone());
    let both: Vec<_> = (0..4).map(|_| picked(&router)).collect();
    assert_eq!(both, ["east/m", "west/m", "east/m", "west/m"]);
    // East fails twice: its circuit opens, and every pick goes west.
    let now = Instant::now();
    breaker.record_failure("east", true, now);
    breaker.record_failure("east", true, now);
    assert!(breaker.should_skip("east"));
    assert!((0..6).all(|_| picked(&router) == "west/m"));
    // When every target is open health is ignored: a pick, never a refusal.
    breaker.record_failure("west", true, now);
    breaker.record_failure("west", true, now);
    let any = picked(&router);
    assert!(any == "east/m" || any == "west/m");
}

#[test]
fn a_target_past_its_cooldown_is_probed_so_it_can_recover() {
    let breaker = Arc::new(ProviderBreaker::with_config(BreakerConfig {
        window: Duration::from_secs(60),
        trip_threshold: 1,
        cooldown: Duration::from_millis(80),
    }));
    let router = router(Strategy::Ordered, breaker.clone());
    breaker.record_failure("east", true, Instant::now());
    assert!(breaker.should_skip("east"));
    // Within the cooldown east is skipped.
    assert_eq!(picked(&router), "west/m");
    std::thread::sleep(Duration::from_millis(100));
    // Past it, ordered prefers east and the selector reserves its probe for this request, which
    // is what closes the circuit once it succeeds. Without the probe east would stay open for
    // ever, because nothing else ever reaches it.
    assert_eq!(picked(&router), "east/m");
    // The probe slot is taken: until the outcome is reported the next request goes elsewhere.
    assert_eq!(picked(&router), "west/m");
    breaker.record_success("east");
    assert_eq!(picked(&router), "east/m");
}

#[test]
fn a_probe_is_reserved_only_for_the_target_that_was_picked() {
    let breaker = Arc::new(ProviderBreaker::with_config(BreakerConfig {
        window: Duration::from_secs(60),
        trip_threshold: 1,
        cooldown: Duration::from_millis(0),
    }));
    // Ordered: the healthy primary is picked every time, while the standby's circuit is open
    // and past its cooldown. Asking about the standby must not take its probe slot: no request
    // is ever sent to it, so nothing would report, and it would stay half-open for ever.
    let router = router(Strategy::Ordered, breaker.clone());
    breaker.record_failure("west", true, Instant::now());
    for _ in 0..5 {
        assert_eq!(picked(&router), "east/m");
    }
    assert!(
        breaker.try_admit_probe("west", Instant::now()),
        "the standby's probe slot is still free"
    );
}

#[test]
fn a_probe_that_never_reports_is_offered_again_after_a_cooldown() {
    let breaker = ProviderBreaker::with_config(BreakerConfig {
        window: Duration::from_secs(60),
        trip_threshold: 1,
        cooldown: Duration::from_millis(80),
    });
    breaker.record_failure("east", true, Instant::now());
    std::thread::sleep(Duration::from_millis(100));
    assert!(breaker.try_admit_probe("east", Instant::now()));
    // The slot is taken while the probe could still report.
    assert!(!breaker.try_admit_probe("east", Instant::now()));
    assert!(!breaker.probe_due("east", Instant::now()));
    // A request that died before it was dispatched never reports: after a cooldown the slot is
    // offered again rather than the target staying half-open for ever.
    std::thread::sleep(Duration::from_millis(100));
    assert!(breaker.probe_due("east", Instant::now()));
    assert!(breaker.try_admit_probe("east", Instant::now()));
}

#[test]
fn peeking_at_a_deployment_does_not_take_a_turn_or_a_probe() {
    let router = router(Strategy::RoundRobin, Arc::new(ProviderBreaker::new()));
    let peeked = router.peek_deployment(&request()).unwrap();
    assert_eq!(peeked, router.peek_deployment(&request()).unwrap());
    // The real picks still start where the peek said and alternate from there.
    assert_eq!(picked(&router), peeked);
    assert_ne!(picked(&router), peeked);
    // A model that is not a deployment is not peeked.
    assert_eq!(router.peek_deployment(&json!({"model": "east/m"})), None);
}

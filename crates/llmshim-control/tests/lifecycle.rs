use llmshim_control::{Credential, Error, LimitsUpdate, NewKey, Store};
use serde_json::json;
use std::path::PathBuf;

const ACTOR: &str = "operator@example.test";

fn dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "llmshim-control-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

struct Fixture {
    store: Store,
    org: String,
    team: String,
    project: String,
}

fn fixture() -> Fixture {
    let mut store = Store::open_in_memory().unwrap();
    let org = store.create_org("Acme", ACTOR).unwrap();
    let team = store.create_team(&org.id, "Platform", ACTOR).unwrap();
    let project = store.create_project(&team.id, "Checkout", ACTOR).unwrap();
    Fixture {
        store,
        org: org.id,
        team: team.id,
        project: project.id,
    }
}

fn control_plane(
    org: &str,
    public: &str,
    file: &std::path::Path,
) -> llmshim::serve::config::ControlPlane {
    serde_json::from_value(json!({
        "org": org, "verify_keys": [public], "snapshot_file": file,
    }))
    .unwrap()
}

#[test]
fn a_secret_is_shown_once_and_only_its_hash_is_stored() {
    let directory = dir("hash");
    let database = directory.join("control.db");
    let mut store = Store::open(&database).unwrap();
    let org = store.create_org("Acme", ACTOR).unwrap();
    let team = store.create_team(&org.id, "Platform", ACTOR).unwrap();
    let project = store.create_project(&team.id, "Checkout", ACTOR).unwrap();
    let created = store
        .create_key(NewKey::secret(&project.id, "ci"), ACTOR)
        .unwrap();
    let secret = created
        .secret
        .expect("a secret key returns its secret once");
    assert!(secret.len() >= 22, "the data plane refuses a shorter token");
    assert_eq!(created.record.state, "active");
    drop(store);
    // Every byte of the database file (and its write-ahead log) is free of the secret.
    for entry in std::fs::read_dir(&directory).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        assert!(
            !bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "the secret is in the database"
        );
    }
    // Reopening keeps the data (and the schema version is accepted).
    let store = Store::open(&database).unwrap();
    assert_eq!(store.keys(&org.id).unwrap().len(), 1);
}

#[test]
fn names_are_checked_and_unique_within_an_organization() {
    let mut f = fixture();
    f.store
        .create_key(NewKey::secret(&f.project, "ci"), ACTOR)
        .unwrap();
    assert!(matches!(
        f.store.create_key(NewKey::secret(&f.project, "ci"), ACTOR),
        Err(Error::Conflict(_))
    ));
    for bad in ["", "Has Space", "UPPER", "-lead", &"x".repeat(65)] {
        assert!(
            matches!(
                f.store.create_key(NewKey::secret(&f.project, bad), ACTOR),
                Err(Error::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    assert!(matches!(
        f.store
            .create_key(NewKey::secret("proj_missing", "x"), ACTOR),
        Err(Error::NotFound(_))
    ));
    let mut bad = NewKey::secret(&f.project, "later");
    bad.expires_at = Some("tomorrow".into());
    assert!(matches!(
        f.store.create_key(bad, ACTOR),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        f.store.create_key(NewKey::secret(&f.project, "later"), ""),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn revocation_is_terminal_and_every_change_is_in_the_history_without_a_secret() {
    let mut f = fixture();
    let created = f
        .store
        .create_key(NewKey::secret(&f.project, "ci"), ACTOR)
        .unwrap();
    let secret = created.secret.unwrap();
    let revoked = f.store.revoke_key(&created.record.id, ACTOR).unwrap();
    assert_eq!(revoked.state, "revoked");
    assert!(revoked.revoked_at.is_some());
    assert!(matches!(
        f.store.revoke_key(&created.record.id, ACTOR),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        f.store.rotate_key(&created.record.id, "ci2", ACTOR),
        Err(Error::Conflict(_))
    ));
    let history = f.store.history_rows(100).unwrap();
    let actions: Vec<&str> = history
        .iter()
        .rev()
        .map(|row| row.action.as_str())
        .collect();
    assert_eq!(
        actions,
        [
            "org.created",
            "team.created",
            "project.created",
            "key.created",
            "key.revoked"
        ]
    );
    assert!(history.iter().all(|row| row.actor == ACTOR));
    assert!(!serde_json::to_string(&history).unwrap().contains(&secret));
}

#[test]
fn rotation_makes_a_successor_and_leaves_the_old_key_until_it_is_revoked() {
    let mut f = fixture();
    let mut first = NewKey::secret(&f.project, "ci");
    first.rpm = Some(60);
    first.tier = 2;
    first.allowed_cidrs = Some(vec!["10.0.0.0/8".into()]);
    let old = f.store.create_key(first, ACTOR).unwrap();
    let successor = f.store.rotate_key(&old.record.id, "ci-2", ACTOR).unwrap();
    assert_ne!(successor.secret, old.secret);
    assert_eq!(successor.record.rpm, Some(60));
    assert_eq!(successor.record.tier, 2);
    assert_eq!(successor.record.allowed_cidrs, old.record.allowed_cidrs);
    assert_eq!(f.store.key(&old.record.id).unwrap().state, "active");
    f.store.revoke_key(&old.record.id, ACTOR).unwrap();
    assert!(f
        .store
        .history_rows(20)
        .unwrap()
        .iter()
        .any(|row| row.action == "key.rotated"
            && row.detail["successor"] == json!(successor.record.id)));
    // A certificate or token identity is the workload's own: it does not rotate here.
    let mut san = NewKey::secret(&f.project, "workload");
    san.credential = Credential::ClientSan("uri:spiffe://corp/a".into());
    let san = f.store.create_key(san, ACTOR).unwrap();
    assert!(san.secret.is_none());
    assert!(matches!(
        f.store.rotate_key(&san.record.id, "workload-2", ACTOR),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn a_published_snapshot_verifies_and_loads_in_the_data_plane() {
    let directory = dir("snapshot");
    let mut f = fixture();
    let ci = f
        .store
        .create_key(NewKey::secret(&f.project, "ci"), ACTOR)
        .unwrap();
    let old = f
        .store
        .create_key(NewKey::secret(&f.project, "old"), ACTOR)
        .unwrap();
    f.store.revoke_key(&old.record.id, ACTOR).unwrap();
    let mut jwt = NewKey::secret(&f.project, "agent");
    jwt.credential = Credential::Jwt {
        table: "issuer".into(),
        subject: "svc-agent".into(),
    };
    f.store.create_key(jwt, ACTOR).unwrap();
    let (seed, public) = llmshim::serve::snapshot::keygen().unwrap();

    let first = f.store.publish_snapshot(&f.org, &seed, ACTOR).unwrap();
    assert_eq!(first.generation, 1);
    let file = directory.join("snapshot.json");
    std::fs::write(&file, &first.envelope).unwrap();

    // The data plane's own loader accepts it: same format, same rules.
    let control_plane = control_plane(&f.org, &public, &file);
    let loaded = llmshim::serve::snapshot::load(&control_plane, None, chrono::Utc::now()).unwrap();
    assert_eq!(loaded.generation, 1);
    assert_eq!(loaded.inactive, 1, "the revoked key is listed, not loaded");
    let names: Vec<&str> = loaded.keys.iter().map(|k| k.name.as_str()).collect();
    assert_eq!(names, ["ci", "agent"]);
    assert!(loaded.keys.iter().all(|k| k.tenant == f.team));
    assert!(loaded.keys[0].sha256.is_some() && loaded.keys[1].jwt.is_some());
    // Nothing the data plane reads is the secret itself.
    assert!(!first.envelope.contains(&ci.secret.unwrap()));

    // The next generation advances, and a later change shows in it.
    f.store.revoke_key(&ci.record.id, ACTOR).unwrap();
    let second = f.store.publish_snapshot(&f.org, &seed, ACTOR).unwrap();
    assert_eq!(second.generation, 2);
    std::fs::write(&file, &second.envelope).unwrap();
    let floor = llmshim::serve::snapshot::Floor {
        org: f.org.clone(),
        generation: 1,
        payload_sha256: loaded.payload_sha256.clone(),
        applied_at_epoch_secs: 0,
    };
    let loaded =
        llmshim::serve::snapshot::load(&control_plane, Some(&floor), chrono::Utc::now()).unwrap();
    assert_eq!(loaded.generation, 2);
    assert_eq!(loaded.keys.len(), 1);
    assert_eq!(
        f.store.latest_snapshot(&f.org).unwrap().unwrap().generation,
        2
    );
}

#[test]
fn an_expired_key_is_published_as_expired_and_limits_changes_reach_the_next_snapshot() {
    let mut f = fixture();
    let mut late = NewKey::secret(&f.project, "late");
    late.expires_at = Some("2020-01-01T00:00:00Z".into());
    f.store.create_key(late, ACTOR).unwrap();
    let live = f
        .store
        .create_key(NewKey::secret(&f.project, "live"), ACTOR)
        .unwrap();
    f.store
        .update_key_limits(
            &live.record.id,
            LimitsUpdate {
                rpm: Some(10),
                tpm: Some(1000),
                budget_usd: Some(5.0),
                budget_window_secs: Some(3600),
                expires_at: Some("2999-01-01T00:00:00Z".into()),
            },
            ACTOR,
        )
        .unwrap();
    let payload = f.store.build_payload(&f.org).unwrap();
    let keys = payload["keys"].as_array().unwrap();
    let state = |name: &str| keys.iter().find(|k| k["name"] == name).unwrap()["state"].clone();
    assert_eq!(state("late"), "expired");
    assert_eq!(state("live"), "active");
    let live = keys.iter().find(|k| k["name"] == "live").unwrap();
    assert_eq!(live["rpm"], 10);
    assert_eq!(live["budget_usd"], 5.0);
    assert_eq!(live["expires_at"], "2999-01-01T00:00:00Z");
    // A revoked key cannot change.
    let id = live["id"].as_str().unwrap().to_string();
    f.store.revoke_key(&id, ACTOR).unwrap();
    assert!(matches!(
        f.store
            .update_key_limits(&id, LimitsUpdate::default(), ACTOR),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn an_unreadable_signing_key_or_an_unknown_organization_is_an_error_not_a_snapshot() {
    let mut f = fixture();
    assert!(matches!(
        f.store.publish_snapshot(&f.org, "not a key", ACTOR),
        Err(Error::Snapshot(_))
    ));
    let (seed, _) = llmshim::serve::snapshot::keygen().unwrap();
    assert!(matches!(
        f.store.publish_snapshot("org_missing", &seed, ACTOR),
        Err(Error::NotFound(_))
    ));
    // Nothing was published, so the next generation is still the first.
    assert_eq!(f.store.build_payload(&f.org).unwrap()["generation"], 1);
}

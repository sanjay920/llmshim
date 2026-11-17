mod common;

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
    let mut store = common::store();
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

/// Two handles on one database: SQLite files, or two connections to one Postgres schema.
fn two_handles(label: &str) -> (Store, Store, PathBuf, Option<(String, String)>) {
    let directory = dir(label);
    match common::postgres_url() {
        Some(url) => {
            let schema = common::fresh_schema();
            let first = Store::open_postgres(&url, Some(&schema)).unwrap();
            let second = Store::open_postgres(&url, Some(&schema)).unwrap();
            (first, second, directory, Some((url, schema)))
        }
        None => {
            let database = directory.join("control.db");
            let first = Store::open(&database).unwrap();
            let second = Store::open(&database).unwrap();
            (first, second, directory, None)
        }
    }
}

/// Everything the database holds, as text, to look for a secret in.
fn everything_stored(
    directory: &std::path::Path,
    postgres: &Option<(String, String)>,
) -> Vec<Vec<u8>> {
    match postgres {
        None => std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
            .collect(),
        Some((url, schema)) => {
            let mut client = postgres::Client::connect(url, postgres::NoTls)
                .or_else(|_| {
                    postgres::Client::connect(&format!("{url}?sslmode=disable"), postgres::NoTls)
                })
                .expect("a plain connection for the check");
            client
                .batch_execute(&format!("SET search_path TO {schema}"))
                .unwrap();
            ["orgs", "teams", "projects", "keys", "history", "snapshots"]
                .iter()
                .map(|table| {
                    let rows = client
                        .query(&format!("SELECT x::text FROM {table} x"), &[])
                        .unwrap();
                    rows.iter()
                        .map(|r| r.get::<_, String>(0))
                        .collect::<String>()
                        .into_bytes()
                })
                .collect()
        }
    }
}

#[test]
fn a_secret_is_shown_once_and_only_its_hash_is_stored() {
    let (mut store, _second, directory, postgres) = two_handles("hash");
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
    let (seed, _) = llmshim::serve::snapshot::keygen().unwrap();
    store.publish_snapshot(&org.id, &seed, ACTOR).unwrap();
    drop(store);
    // Every byte the database holds (a SQLite file and its log, or every Postgres table) is
    // free of the secret.
    for bytes in everything_stored(&directory, &postgres) {
        assert!(
            !bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "the secret is in the database"
        );
    }
    // A second handle sees the data (and the schema version is accepted).
    let mut again = _second;
    assert_eq!(again.keys(&org.id).unwrap().len(), 1);
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

#[test]
fn a_key_the_data_plane_would_refuse_is_refused_here_and_never_reaches_a_snapshot() {
    let mut f = fixture();
    let attempt = |f: &mut Fixture, edit: &dyn Fn(&mut NewKey)| {
        let mut new = NewKey::secret(&f.project, "k");
        edit(&mut new);
        f.store.create_key(new, ACTOR)
    };
    assert!(matches!(
        attempt(&mut f, &|k| k.allowed_cidrs = Some(vec!["nope".into()])),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        attempt(&mut f, &|k| k.allowed_cidrs = Some(vec![])),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        attempt(&mut f, &|k| k.credential =
            Credential::ClientSan("uri:has space".into())),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        attempt(&mut f, &|k| k.budget_usd = Some(f64::NAN)),
        Err(Error::Invalid(_))
    ));
    assert!(
        matches!(
            attempt(&mut f, &|k| k.budget_window_secs = Some(3600)),
            Err(Error::Invalid(_)),
        ),
        "a window needs a budget"
    );
    assert!(matches!(
        attempt(&mut f, &|k| {
            k.budget_usd = Some(5.0);
            k.budget_window_secs = Some(0);
        }),
        Err(Error::Invalid(_))
    ));
    // Nothing partial was left behind: the organization has no keys, and a valid one works.
    assert!(f.store.keys(&f.org).unwrap().is_empty());
    assert!(attempt(&mut f, &|k| {
        k.allowed_cidrs = Some(vec!["10.0.0.0/8".into(), "2001:db8::/32".into()]);
        k.budget_usd = Some(5.0);
        k.budget_window_secs = Some(3600);
    })
    .is_ok());
    // An update that would leave the stored flags without a budget is refused too.
    let mut with_flag = NewKey::secret(&f.project, "flag");
    with_flag.budget_usd = Some(1.0);
    with_flag.budget_allow_unpriced = true;
    let flagged = f.store.create_key(with_flag, ACTOR).unwrap();
    assert!(matches!(
        f.store
            .update_key_limits(&flagged.record.id, LimitsUpdate::default(), ACTOR),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn debug_output_never_shows_a_secret() {
    let mut f = fixture();
    let created = f
        .store
        .create_key(NewKey::secret(&f.project, "ci"), ACTOR)
        .unwrap();
    let secret = created.secret.clone().unwrap();
    assert!(!format!("{created:?}").contains(&secret));
    assert!(!format!("{:?}", created.record).contains(&secret));
    // Nor does the serialized record carry its hash.
    let record = serde_json::to_string(&created.record).unwrap();
    assert!(!record.contains("sha256"), "{record}");
}

#[test]
fn two_processes_on_one_database_cannot_take_the_same_generation_or_double_revoke() {
    let (mut first, mut second, _directory, _) = two_handles("two-writers");
    let org = first.create_org("Acme", ACTOR).unwrap();
    let team = first.create_team(&org.id, "Platform", ACTOR).unwrap();
    let project = first.create_project(&team.id, "Checkout", ACTOR).unwrap();
    let key = first
        .create_key(NewKey::secret(&project.id, "ci"), ACTOR)
        .unwrap();
    let (seed, _) = llmshim::serve::snapshot::keygen().unwrap();

    // The second reads what the first committed: generation 2, and it lists the revocation.
    assert_eq!(
        first
            .publish_snapshot(&org.id, &seed, ACTOR)
            .unwrap()
            .generation,
        1
    );
    first.revoke_key(&key.record.id, ACTOR).unwrap();
    let published = second.publish_snapshot(&org.id, &seed, ACTOR).unwrap();
    assert_eq!(published.generation, 2);
    let payload = second.build_payload(&org.id).unwrap();
    assert_eq!(payload["generation"], 3);
    let listed = &second.latest_snapshot(&org.id).unwrap().unwrap().envelope;
    assert!(listed.len() > 10);
    // The second's own view of the key is the committed one: revoking again conflicts, and
    // a limits change on it is refused (the state is checked where it is written).
    assert!(matches!(
        second.revoke_key(&key.record.id, ACTOR),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        second.update_key_limits(&key.record.id, LimitsUpdate::default(), ACTOR),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn a_rotation_is_one_unit_it_leaves_no_successor_without_its_record() {
    let mut f = fixture();
    let old = f
        .store
        .create_key(NewKey::secret(&f.project, "ci"), ACTOR)
        .unwrap();
    // A rotation that fails (a name already taken) leaves no successor and no record.
    f.store
        .create_key(NewKey::secret(&f.project, "taken"), ACTOR)
        .unwrap();
    let before = f.store.history_rows(100).unwrap().len();
    assert!(matches!(
        f.store.rotate_key(&old.record.id, "taken", ACTOR),
        Err(Error::Conflict(_))
    ));
    assert_eq!(f.store.history_rows(100).unwrap().len(), before);
    assert_eq!(f.store.keys(&f.org).unwrap().len(), 2);
}

#[test]
fn a_second_active_key_for_the_same_identity_is_refused_so_a_snapshot_is_never_refused_whole() {
    let mut f = fixture();
    let san = |f: &mut Fixture, name: &str, text: &str| {
        let mut new = NewKey::secret(&f.project, name);
        new.credential = Credential::ClientSan(text.into());
        f.store.create_key(new, ACTOR)
    };
    let jwt = |f: &mut Fixture, name: &str, table: &str, subject: &str| {
        let mut new = NewKey::secret(&f.project, name);
        new.credential = Credential::Jwt {
            table: table.into(),
            subject: subject.into(),
        };
        f.store.create_key(new, ACTOR)
    };
    let first = san(&mut f, "a", "dns:Svc.Example.Test").unwrap();
    assert_eq!(
        first.record.client_san.as_deref(),
        Some("dns:svc.example.test")
    );
    // The data plane compares DNS names case-insensitively: the same name again conflicts.
    assert!(matches!(
        san(&mut f, "b", "dns:svc.example.test"),
        Err(Error::Conflict(_))
    ));
    assert!(san(&mut f, "c", "uri:spiffe://corp/Svc").is_ok());
    assert!(matches!(
        san(&mut f, "d", "uri:spiffe://corp/Svc"),
        Err(Error::Conflict(_))
    ));
    jwt(&mut f, "e", "idp", "svc:a").unwrap();
    assert!(matches!(
        jwt(&mut f, "g", "idp", "svc:a"),
        Err(Error::Conflict(_))
    ));
    assert!(jwt(&mut f, "h", "idp", "svc:b").is_ok());
    for bad in ["", "has space", &"x".repeat(513)] {
        assert!(
            matches!(jwt(&mut f, "i", "idp", bad), Err(Error::Invalid(_))),
            "{bad:?}"
        );
    }
    // Revoking the holder frees the identity (the data plane skips a revoked key).
    f.store.revoke_key(&first.record.id, ACTOR).unwrap();
    assert!(san(&mut f, "b", "dns:svc.example.test").is_ok());
    // And what was published loads: no duplicate reached the signed payload.
    let directory = dir("dups");
    let (seed, public) = llmshim::serve::snapshot::keygen().unwrap();
    let published = f.store.publish_snapshot(&f.org, &seed, ACTOR).unwrap();
    let file = directory.join("snapshot.json");
    std::fs::write(&file, published.envelope).unwrap();
    let cp = control_plane(&f.org, &public, &file);
    assert!(llmshim::serve::snapshot::load(&cp, None, chrono::Utc::now()).is_ok());
}

#[test]
fn too_many_networks_are_refused() {
    let mut f = fixture();
    let mut new = NewKey::secret(&f.project, "wide");
    new.allowed_cidrs = Some(
        (0..257)
            .map(|n| format!("10.{}.{}.0/24", n / 256, n % 256))
            .collect(),
    );
    assert!(matches!(
        f.store.create_key(new, ACTOR),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn concurrent_publishers_each_get_their_own_generation() {
    let (mut first, mut second, _directory, _) = two_handles("concurrent");
    let org = first.create_org("Acme", ACTOR).unwrap();
    let team = first.create_team(&org.id, "Platform", ACTOR).unwrap();
    let project = first.create_project(&team.id, "Checkout", ACTOR).unwrap();
    first
        .create_key(NewKey::secret(&project.id, "ci"), ACTOR)
        .unwrap();
    let (seed, _) = llmshim::serve::snapshot::keygen().unwrap();
    let publish = |store: &mut Store, org: &str, seed: &str| {
        (0..8)
            .map(|_| store.publish_snapshot(org, seed, ACTOR).unwrap().generation)
            .collect::<Vec<_>>()
    };
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| publish(&mut first, &org.id, &seed));
        let b = scope.spawn(|| publish(&mut second, &org.id, &seed));
        (a.join().unwrap(), b.join().unwrap())
    });
    let mut all: Vec<u64> = a.into_iter().chain(b).collect();
    all.sort_unstable();
    assert_eq!(
        all,
        (1..=16).collect::<Vec<_>>(),
        "no generation was taken twice or skipped"
    );
}

#[test]
fn names_and_actors_refuse_control_characters_on_every_backend() {
    let mut f = fixture();
    for bad in ["a\u{0}b", "line\nbreak", "tab\there"] {
        assert!(
            matches!(f.store.create_org(bad, ACTOR), Err(Error::Invalid(_))),
            "{bad:?}"
        );
        assert!(
            matches!(
                f.store.create_team(&f.org, bad, ACTOR),
                Err(Error::Invalid(_))
            ),
            "{bad:?}"
        );
        assert!(
            matches!(f.store.create_org("ok", bad), Err(Error::Invalid(_))),
            "actor {bad:?}"
        );
    }
}

#[test]
fn postgres_requires_tls_names_a_valid_schema_and_survives_a_killed_connection() {
    let Some(url) = common::postgres_url() else {
        return;
    };
    // No sslmode (so the library default of "prefer") against a server without TLS: refused,
    // never quietly plaintext. The password is in no message.
    let bare = url.split('?').next().unwrap().to_string();
    let error = Store::open_postgres(&bare, Some(&common::fresh_schema()))
        .err()
        .expect("no TLS, no connection");
    assert!(
        !error.to_string().contains(":pw@") && !error.to_string().contains("pw@"),
        "{error}"
    );
    // A schema name is a plain identifier.
    for bad in ["1abc", "Has", "a-b", "pg_catalog", "information_schema", ""] {
        assert!(
            matches!(
                Store::open_postgres(&url, Some(bad)),
                Err(Error::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    // A killed session is replaced: the next call works without a restart.
    let application = format!("reconnect_{}", uuid::Uuid::new_v4().simple());
    let named = format!("{url}&application_name={application}");
    let schema = common::fresh_schema();
    let mut store = Store::open_postgres(&named, Some(&schema)).unwrap();
    let org = store.create_org("Acme", ACTOR).unwrap();
    let mut other = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let killed = other
        .query(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name = $1",
            &[&application],
        )
        .unwrap();
    assert!(!killed.is_empty(), "the session to kill was found");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(
        store.teams(&org.id).unwrap().len(),
        0,
        "the next call reconnected"
    );
    assert!(store.create_team(&org.id, "Platform", ACTOR).is_ok());
}

#[test]
fn a_model_list_is_checked_stored_copied_on_rotation_and_signed_only_when_set() {
    let mut f = fixture();
    // Refused like the data plane would refuse it.
    for bad in [
        vec![],
        vec!["gpt-6-luna".to_string()],
        vec!["openai/*/x".to_string()],
        vec!["*".to_string()],
        (0..257).map(|n| format!("openai/m{n}")).collect(),
    ] {
        let mut new = NewKey::secret(&f.project, "bad");
        new.allowed_models = Some(bad.clone());
        assert!(
            matches!(f.store.create_key(new, ACTOR), Err(Error::Invalid(_))),
            "{bad:?}"
        );
    }
    assert!(f.store.keys(&f.org).unwrap().is_empty());
    // Exactly 256 entries is allowed.
    let mut full = NewKey::secret(&f.project, "full");
    full.allowed_models = Some((0..256).map(|n| format!("openai/m{n}")).collect());
    f.store.create_key(full, ACTOR).unwrap();
    let mut restricted = NewKey::secret(&f.project, "batch");
    restricted.allowed_models = Some(vec!["openai/gpt-6-luna".into(), "route/cheap".into()]);
    let batch = f.store.create_key(restricted, ACTOR).unwrap();
    let open = f
        .store
        .create_key(NewKey::secret(&f.project, "open"), ACTOR)
        .unwrap();
    assert_eq!(
        f.store.key(&batch.record.id).unwrap().allowed_models,
        Some(vec![
            "openai/gpt-6-luna".to_string(),
            "route/cheap".to_string()
        ])
    );
    assert_eq!(f.store.key(&open.record.id).unwrap().allowed_models, None);
    // A rotation keeps the restriction.
    let next = f
        .store
        .rotate_key(&batch.record.id, "batch-2", ACTOR)
        .unwrap();
    assert_eq!(next.record.allowed_models, batch.record.allowed_models);
    // The payload names the list on the keys that have one and omits it elsewhere, so a
    // snapshot with no restricted key is the one an older data plane still reads.
    let payload = f.store.build_payload(&f.org).unwrap();
    let keys = payload["keys"].as_array().unwrap();
    let find = |name: &str| keys.iter().find(|k| k["name"] == name).unwrap();
    assert_eq!(
        find("batch")["allowed_models"],
        json!(["openai/gpt-6-luna", "route/cheap"])
    );
    assert!(find("open").get("allowed_models").is_none());
    // And the data plane reads it back onto the key it serves.
    let directory = dir("models");
    let (seed, public) = llmshim::serve::snapshot::keygen().unwrap();
    let published = f.store.publish_snapshot(&f.org, &seed, ACTOR).unwrap();
    let file = directory.join("snapshot.json");
    std::fs::write(&file, published.envelope).unwrap();
    let cp = control_plane(&f.org, &public, &file);
    let loaded = llmshim::serve::snapshot::load(&cp, None, chrono::Utc::now()).unwrap();
    let served = |name: &str| loaded.keys.iter().find(|k| k.name == name).unwrap();
    assert_eq!(
        served("batch").allowed_models,
        Some(vec![
            "openai/gpt-6-luna".to_string(),
            "route/cheap".to_string()
        ])
    );
    assert_eq!(served("open").allowed_models, None);
}

/// A database made by an earlier schema version opens, is migrated in place, and keeps its
/// keys: version 1 had neither model lists nor tags, version 2 had model lists only.
#[test]
fn an_older_database_is_migrated_in_place() {
    for from in [1_i64, 2] {
        migrate_from(from);
    }
}

fn migrate_from(from: i64) {
    let drop_columns = if from == 1 {
        "ALTER TABLE keys DROP COLUMN allowed_models; ALTER TABLE keys DROP COLUMN tags;"
    } else {
        "ALTER TABLE keys DROP COLUMN tags;"
    };
    let use_new_columns = |store: &mut Store, project: &str| {
        let mut new = NewKey::secret(project, "new");
        new.allowed_models = Some(vec!["openai/*".into()]);
        new.tags = [("env".to_string(), "prod".to_string())].into();
        let made = store.create_key(new, ACTOR).unwrap();
        assert_eq!(made.record.tags["env"], "prod");
    };
    if let Some(url) = common::postgres_url() {
        let schema = common::fresh_schema();
        let org = {
            let mut store = Store::open_postgres(&url, Some(&schema)).unwrap();
            let org = store.create_org("Acme", ACTOR).unwrap();
            let team = store.create_team(&org.id, "Platform", ACTOR).unwrap();
            let project = store.create_project(&team.id, "Checkout", ACTOR).unwrap();
            store
                .create_key(NewKey::secret(&project.id, "old"), ACTOR)
                .unwrap();
            org.id
        };
        let mut raw = postgres::Client::connect(&url, postgres::NoTls)
            .expect("the test database accepts a plain connection (sslmode=disable)");
        raw.batch_execute(&format!(
            "SET search_path TO {schema}; {drop_columns} UPDATE schema_meta SET version = {from}"
        ))
        .unwrap();
        drop(raw);
        let mut store = Store::open_postgres(&url, Some(&schema)).unwrap();
        let keys = store.keys(&org).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].allowed_models, None);
        assert!(keys[0].tags.is_empty());
        use_new_columns(&mut store, &keys[0].project_id);
        // The version was bumped: opening again does not migrate again.
        drop(store);
        let mut again = Store::open_postgres(&url, Some(&schema)).unwrap();
        assert_eq!(again.keys(&org).unwrap().len(), 2);
        return;
    }
    let directory = dir("migrate");
    let file = directory.join("control.db");
    let (store, org) = {
        let mut store = Store::open(&file).unwrap();
        let org = store.create_org("Acme", ACTOR).unwrap();
        let team = store.create_team(&org.id, "Platform", ACTOR).unwrap();
        let project = store.create_project(&team.id, "Checkout", ACTOR).unwrap();
        store
            .create_key(NewKey::secret(&project.id, "old"), ACTOR)
            .unwrap();
        (store, org.id)
    };
    drop(store);
    {
        let connection = rusqlite::Connection::open(&file).unwrap();
        connection
            .execute_batch(&format!("{drop_columns} PRAGMA user_version = {from};"))
            .unwrap();
    }
    let mut store = Store::open(&file).unwrap();
    let keys = store.keys(&org).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].allowed_models, None);
    assert!(keys[0].tags.is_empty());
    use_new_columns(&mut store, &keys[0].project_id);
    // The version was bumped: a second start does not migrate again (and so does not fail on
    // the columns that now exist).
    drop(store);
    let mut again = Store::open(&file).unwrap();
    assert_eq!(again.keys(&org).unwrap().len(), 2);
}

#[test]
fn tags_are_checked_stored_copied_on_rotation_and_signed_only_when_present() {
    let mut f = fixture();
    let too_many: std::collections::BTreeMap<String, String> = (0..17)
        .map(|n| (format!("t{n}"), "v".to_string()))
        .collect();
    let bad: Vec<std::collections::BTreeMap<String, String>> = vec![
        too_many,
        [("Env".to_string(), "x".to_string())].into(),
        [("env".to_string(), String::new())].into(),
        [("env".to_string(), " lead".to_string())].into(),
        [("env".to_string(), "caf\u{e9}".to_string())].into(),
        [("a-b".to_string(), "x".to_string())].into(),
    ];
    for tags in bad {
        let mut new = NewKey::secret(&f.project, "bad");
        new.tags = tags.clone();
        assert!(
            matches!(f.store.create_key(new, ACTOR), Err(Error::Invalid(_))),
            "{tags:?}"
        );
    }
    assert!(f.store.keys(&f.org).unwrap().is_empty());
    let mut tagged = NewKey::secret(&f.project, "tagged");
    tagged.tags = [
        ("product".to_string(), "checkout".to_string()),
        ("cost_center".to_string(), "cc-4417".to_string()),
    ]
    .into();
    let made = f.store.create_key(tagged, ACTOR).unwrap();
    f.store
        .create_key(NewKey::secret(&f.project, "plain"), ACTOR)
        .unwrap();
    assert_eq!(
        f.store.key(&made.record.id).unwrap().tags["cost_center"],
        "cc-4417"
    );
    let next = f
        .store
        .rotate_key(&made.record.id, "tagged-2", ACTOR)
        .unwrap();
    assert_eq!(next.record.tags, made.record.tags);
    let payload = f.store.build_payload(&f.org).unwrap();
    let keys = payload["keys"].as_array().unwrap();
    let find = |name: &str| keys.iter().find(|k| k["name"] == name).unwrap();
    assert_eq!(
        find("tagged")["tags"],
        json!({"cost_center": "cc-4417", "product": "checkout"})
    );
    assert!(find("plain").get("tags").is_none());
    // The data plane reads them back onto the key it serves.
    let directory = dir("tags");
    let (seed, public) = llmshim::serve::snapshot::keygen().unwrap();
    let published = f.store.publish_snapshot(&f.org, &seed, ACTOR).unwrap();
    let file = directory.join("snapshot.json");
    std::fs::write(&file, published.envelope).unwrap();
    let cp = control_plane(&f.org, &public, &file);
    let loaded = llmshim::serve::snapshot::load(&cp, None, chrono::Utc::now()).unwrap();
    let served = |name: &str| loaded.keys.iter().find(|k| k.name == name).unwrap();
    assert_eq!(served("tagged").tags["product"], "checkout");
    assert!(served("plain").tags.is_empty());
}

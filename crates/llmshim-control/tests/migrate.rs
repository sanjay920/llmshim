//! The migration policy (note 0054): only `migrate` (and `serve --migrate-on-start`) changes the schema; `serve` and
//! `audit-verify` refuse a database that is not at this build's version and leave it exactly as it was.

mod common;

use llmshim_control::Store;
use std::path::{Path, PathBuf};
use std::process::Command;

fn dir(label: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("llmshim-migrate-{label}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_llmshim-control"))
        .args(args)
        .env_clear()
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

/// A database as an older build left it (version 4: no chain, no ledger), with an organization in it.
fn old_database(path: &Path) {
    let mut store = Store::open(path).unwrap();
    store.create_org("Acme", "tester").unwrap();
    drop(store);
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(
            "ALTER TABLE history DROP COLUMN prev; ALTER TABLE history DROP COLUMN hash; \
             DROP TABLE request_structure; DROP TABLE requests; PRAGMA user_version = 4;",
        )
        .unwrap();
}

#[test]
fn serve_and_audit_verify_refuse_an_old_empty_or_missing_database_and_change_nothing() {
    let d = dir("refuse");
    let old = d.join("old.db");
    old_database(&old);
    let empty = d.join("empty.db");
    std::fs::write(&empty, b"").unwrap();
    let token = d.join("token");
    std::fs::write(&token, "a-token-of-sufficient-length-0123").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    for db in [&old, &empty] {
        let file = db.to_str().unwrap();
        let before = std::fs::read(db).unwrap();
        let (code, _, err) = run(&[
            "serve",
            "--db",
            file,
            "--listen",
            "127.0.0.1:0",
            "--token-file",
            token.to_str().unwrap(),
        ]);
        assert_eq!(code, Some(1), "{err}");
        assert!(err.contains("migrate"), "{err}");
        let (code, _, err) = run(&["audit-verify", "--db", file]);
        assert_eq!(code, Some(1), "{err}");
        assert_eq!(
            std::fs::read(db).unwrap(),
            before,
            "a refused open must not touch the file"
        );
    }
    assert_eq!(version(&old), 4);
    assert_eq!(version(&empty), 0);
}

#[test]
fn migrate_copies_an_old_database_first_migrates_it_and_the_copy_restores() {
    let d = dir("migrate");
    let db = d.join("c.db");
    old_database(&db);
    let file = db.to_str().unwrap();
    let (code, out, err) = run(&["migrate", "--db", file]);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.contains("from version 4 to 8") && out.contains("pre-v4.bak"),
        "{out}"
    );
    let backup = d.join("c.db.pre-v4.bak");
    assert_eq!(version(&backup), 4, "the copy is the database as it was");
    assert_eq!(version(&db), 8);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&backup).unwrap().permissions().mode() & 0o077,
        0
    );
    // The migrated database opens and keeps its data; audit-verify now accepts it.
    assert_eq!(Store::open_existing(&db).unwrap().orgs().unwrap().len(), 1);
    assert_eq!(run(&["audit-verify", "--db", file]).0, Some(0));
    // Again: nothing to do, and no second copy.
    let (code, out, _) = run(&["migrate", "--db", file]);
    assert_eq!(
        (code, out.contains("already at version 8")),
        (Some(0), true)
    );
    // A second old database at the same path: the first copy is never overwritten, the new one is numbered.
    let first_copy = std::fs::read(&backup).unwrap();
    std::fs::copy(&backup, &db).unwrap();
    let (code, out, err) = run(&["migrate", "--db", file]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("pre-v4.1.bak"), "{out}");
    assert_eq!(std::fs::read(&backup).unwrap(), first_copy);
    assert_eq!(version(&d.join("c.db.pre-v4.1.bak")), 4);
    assert_eq!(version(&db), 8);
    // The copy holds the data as it was, and putting it back is a rollback: the old schema, the organization intact.
    std::fs::copy(&backup, &db).unwrap();
    assert_eq!(version(&db), 4);
    let orgs: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("SELECT count(*) FROM orgs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(orgs, 1);
}

#[test]
fn a_symlink_at_the_backup_name_is_never_written_through() {
    let d = dir("symlink");
    let db = d.join("c.db");
    old_database(&db);
    let victim = d.join("victim");
    std::fs::write(&victim, b"keep me").unwrap();
    std::os::unix::fs::symlink(&victim, d.join("c.db.pre-v4.bak")).unwrap();
    let (code, out, err) = run(&["migrate", "--db", db.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("pre-v4.1.bak"), "{out}");
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
}

#[test]
fn a_current_database_is_served_and_checked_byte_for_byte_unchanged_and_a_missing_one_is_not_created(
) {
    let d = dir("current");
    let db = d.join("c.db");
    let file = db.to_str().unwrap();
    assert_eq!(run(&["migrate", "--db", file]).0, Some(0));
    // Not in WAL mode (a restored copy is not): looking must not switch it.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("PRAGMA journal_mode = DELETE")
        .unwrap();
    let before = std::fs::read(&db).unwrap();
    assert_eq!(run(&["audit-verify", "--db", file]).0, Some(0));
    assert_eq!(std::fs::read(&db).unwrap(), before);
    assert!(!d.join("c.db-wal").exists() && !d.join("c.db-shm").exists());
    // A path that is not there is refused and not made (so never with the umask's mode).
    let missing = d.join("missing.db");
    let token = d.join("token");
    std::fs::write(&token, "a-token-of-sufficient-length-0123").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (code, _, err) = run(&[
        "serve",
        "--db",
        missing.to_str().unwrap(),
        "--listen",
        "127.0.0.1:0",
        "--token-file",
        token.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("migrate"), "{err}");
    assert!(!missing.exists());
    // And one from a newer build is refused by serve as well.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("PRAGMA user_version = 99")
        .unwrap();
    let (code, _, err) = run(&[
        "serve",
        "--db",
        file,
        "--listen",
        "127.0.0.1:0",
        "--token-file",
        token.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("99"), "{err}");
}

#[test]
fn migrate_creates_a_new_database_and_refuses_one_from_a_newer_build() {
    let d = dir("new");
    let db = d.join("fresh.db");
    let file = db.to_str().unwrap();
    let (code, out, err) = run(&["migrate", "--db", file]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("from version 0 to 8"), "{out}");
    assert!(
        !d.join("fresh.db.pre-v0.bak").exists(),
        "a new database has nothing to copy"
    );
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("PRAGMA user_version = 99")
        .unwrap();
    let (code, _, err) = run(&["migrate", "--db", file]);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains("99") && err.contains("restore a backup"),
        "{err}"
    );
    assert_eq!(version(&db), 99);
    // migrate takes only the database options.
    assert_eq!(
        run(&["migrate", "--db", file, "--listen", "127.0.0.1:0"]).0,
        Some(2)
    );
}

#[test]
fn postgres_is_read_only_until_it_is_migrated() {
    let Some(url) = common::postgres_url() else {
        return;
    };
    let schema = common::fresh_schema();
    // Nothing is created by looking: an unmigrated schema is refused, twice, the same way.
    for _ in 0..2 {
        let error = Store::open_postgres_existing(&url, Some(&schema), None)
            .err()
            .expect("refused");
        assert!(error.to_string().contains("migrate"), "{error}");
    }
    // Looking did not create the schema either (a typo must not leave an empty one behind).
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let exists = |client: &mut postgres::Client| -> bool {
        client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = $1)",
                &[&schema],
            )
            .unwrap()
            .get(0)
    };
    assert!(!exists(&mut client));
    let report = llmshim_control::migrate_postgres(&url, Some(&schema), None).unwrap();
    assert!(exists(&mut client));
    assert_eq!((report.from, report.backup), (0, None));
    Store::open_postgres_existing(&url, Some(&schema), None).unwrap();
    let again = llmshim_control::migrate_postgres(&url, Some(&schema), None).unwrap();
    assert_eq!(again.from, again.to);
}

//! The change history's hash chain (note 0047): every row names the one before it, so an edit, a
//! removal in the middle, or a reordering shows at the row where it happened. SQLite files, because
//! the tests reach under the store with a second connection; the chain code is the same on Postgres.

use llmshim_control::{Store, HISTORY_GENESIS};
use std::path::{Path, PathBuf};
use std::process::Command;

const ACTOR: &str = "operator@example.test";

fn database(label: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "llmshim-control-chain-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    directory.join("control.db")
}

fn populated(path: &Path) {
    let mut store = Store::open(path).unwrap();
    let org = store.create_org("Acme", ACTOR).unwrap();
    let team = store.create_team(&org.id, "Platform", ACTOR).unwrap();
    store.create_project(&team.id, "Checkout", ACTOR).unwrap();
}

fn raw(path: &Path, sql: &str) {
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

fn verify(path: &Path) -> Result<llmshim_control::HistoryVerified, llmshim_control::HistoryBroken> {
    Store::open(path).unwrap().verify_history().unwrap()
}

#[test]
fn every_change_is_chained_to_the_one_before_and_the_chain_verifies() {
    let path = database("intact");
    populated(&path);
    let done = verify(&path).unwrap();
    assert_eq!(done.records, 3);
    assert_eq!(done.unchained, 0);
    assert_ne!(done.head, HISTORY_GENESIS);
    // A reopened store continues the same chain.
    let mut store = Store::open(&path).unwrap();
    store.create_org("Beta", ACTOR).unwrap();
    let more = store.verify_history().unwrap().unwrap();
    assert_eq!(more.records, 4);
    assert_ne!(more.head, done.head);
}

#[test]
fn an_edit_and_a_removal_each_break_the_chain_where_they_happened() {
    let edit = database("edit");
    populated(&edit);
    raw(
        &edit,
        "UPDATE history SET actor = 'someone.else' WHERE id = 2",
    );
    let broken = verify(&edit).unwrap_err();
    assert_eq!(broken.id, 2);
    assert!(broken.reason.contains("altered"), "{}", broken.reason);

    let removed = database("removed");
    populated(&removed);
    raw(&removed, "DELETE FROM history WHERE id = 2");
    assert_eq!(verify(&removed).unwrap_err().id, 3);

    // Every field is covered, not only the ones a reader looks at first.
    for column in ["at", "action", "subject", "detail"] {
        let path = database(column);
        populated(&path);
        raw(
            &path,
            &format!("UPDATE history SET {column} = {column} || 'x' WHERE id = 1"),
        );
        assert_eq!(verify(&path).unwrap_err().id, 1, "{column}");
    }

    // A forged hash does not match the row it sits on.
    let rehashed = database("rehashed");
    populated(&rehashed);
    raw(&rehashed, "UPDATE history SET hash = 'forged' WHERE id = 1");
    assert_eq!(verify(&rehashed).unwrap_err().id, 1);
}

#[test]
fn rows_from_before_the_chain_are_counted_and_the_chain_starts_after_them() {
    let path = database("legacy");
    // A version-4 database: the history table without the chain columns.
    {
        let mut store = Store::open(&path).unwrap();
        store.create_org("Old", ACTOR).unwrap();
    }
    raw(
        &path,
        "ALTER TABLE history DROP COLUMN prev; ALTER TABLE history DROP COLUMN hash; \
         PRAGMA user_version = 4;",
    );
    let mut store = Store::open(&path).unwrap();
    store.create_org("New", ACTOR).unwrap();
    let done = store.verify_history().unwrap().unwrap();
    assert_eq!((done.unchained, done.records), (1, 1));
    // An unchained row after chained ones is a removed hash, not history.
    raw(
        &path,
        "INSERT INTO history (at, actor, action, subject, detail) VALUES ('t','a','b','c','{}')",
    );
    let broken = verify(&path).unwrap_err();
    assert!(
        broken.reason.contains("without a hash"),
        "{}",
        broken.reason
    );
}

fn audit_verify(args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_llmshim-control"))
        .arg("audit-verify")
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

#[test]
fn the_audit_verify_command_reports_the_head_names_a_broken_row_and_never_creates_a_database() {
    let path = database("command");
    populated(&path);
    let head = verify(&path).unwrap().head;
    let file = path.to_str().unwrap();
    let (code, out, _) = audit_verify(&["--db", file]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("3 chained row(s)") && out.contains(&head),
        "{out}"
    );

    raw(&path, "UPDATE history SET subject = 'x' WHERE id = 3");
    let (code, out, _) = audit_verify(&["--db", file]);
    assert_eq!(code, Some(1));
    assert!(out.contains("broken at row 3"), "{out}");

    let missing = path.with_file_name("absent.db");
    let (code, _, err) = audit_verify(&["--db", missing.to_str().unwrap()]);
    assert_eq!(code, Some(1));
    assert!(err.contains("does not exist"), "{err}");
    assert!(!missing.exists());

    // Serving options do not belong to it.
    let (code, _, _) = audit_verify(&["--db", file, "--listen", "127.0.0.1:0"]);
    assert_eq!(code, Some(2));
}

mod common;

/// Two handles writing at once (two replicas of the control plane on one Postgres, or two
/// processes on one SQLite file) still make one chain: the write transaction is the only writer.
#[test]
fn two_writers_at_once_still_make_one_chain() {
    let (mut first, mut second, _guard) = match common::postgres_url() {
        Some(url) => {
            let schema = common::fresh_schema();
            (
                Store::open_postgres(&url, Some(&schema)).unwrap(),
                Store::open_postgres(&url, Some(&schema)).unwrap(),
                None,
            )
        }
        None => {
            let path = database("two");
            (
                Store::open(&path).unwrap(),
                Store::open(&path).unwrap(),
                Some(path),
            )
        }
    };
    std::thread::scope(|scope| {
        for (store, name) in [(&mut first, "a"), (&mut second, "b")] {
            scope.spawn(move || {
                for n in 0..15 {
                    store.create_org(&format!("{name}{n}"), ACTOR).unwrap();
                }
            });
        }
    });
    let done = first.verify_history().unwrap().unwrap();
    assert_eq!(done.records, 30);
}

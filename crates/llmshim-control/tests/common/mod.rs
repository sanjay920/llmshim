//! A store on whichever backend the run is for: SQLite in memory, or, with
//! `LLMSHIM_CONTROL_TEST_POSTGRES_URL` set, a fresh schema in that Postgres database. CI runs
//! every suite on both.
#![allow(dead_code)]

use llmshim_control::Store;

pub fn postgres_url() -> Option<String> {
    let url = std::env::var("LLMSHIM_CONTROL_TEST_POSTGRES_URL").ok();
    // CI sets this so that a typo in the URL variable cannot turn the Postgres run into a
    // second, silent SQLite run.
    assert!(
        url.is_some() || std::env::var("LLMSHIM_CONTROL_REQUIRE_POSTGRES").is_err(),
        "LLMSHIM_CONTROL_REQUIRE_POSTGRES is set but LLMSHIM_CONTROL_TEST_POSTGRES_URL is not"
    );
    url
}

pub fn fresh_schema() -> String {
    format!("t_{}", uuid::Uuid::new_v4().simple())
}

/// A new, empty store.
pub fn store() -> Store {
    match postgres_url() {
        Some(url) => Store::open_postgres(&url, Some(&fresh_schema())).unwrap(),
        None => Store::open_in_memory().unwrap(),
    }
}

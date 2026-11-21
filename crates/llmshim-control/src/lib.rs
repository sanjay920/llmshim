//! The llmshim control plane (design note 0024): organizations, teams, projects, the
//! lifecycle of caller keys, and the signed policy snapshots a data plane (`llmshim
//! serve`, `[control_plane]`) takes its keys from.
//!
//! This is the store and its operations as a library; the HTTP API is the next slice. The
//! database holds **hashes** of caller keys, never the keys: a secret is generated here,
//! returned once, and only its SHA-256 is kept. Every mutation appends one row to the change
//! history (who, what, when, ids, never a secret). Signing keys are never in the database.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use db::{i, of, oi, ot, t, Backend, Exec, Row};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub mod api;
mod db;
pub mod gateways;
pub mod requests;
pub mod sso;
pub mod structure;
pub mod ui;

const SCHEMA_VERSION: i64 = 9;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("database error: {0}")]
    Database(String),
    #[error("snapshot error: {0}")]
    Snapshot(String),
    /// The statement budget ran out and the work was interrupted: nothing was written.
    #[error("the database statement ran past its time budget")]
    Timeout,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Org {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Team {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub created_at: String,
    /// The team's budget over every key of the team (the data plane's tenant level, note 0030).
    pub budget: Option<TeamBudget>,
}

/// A team's budget: the data plane's `[tenants.<name>]` (design note 0030), carried in the
/// signed snapshot's tenant record when set.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TeamBudget {
    pub budget_usd: f64,
    pub budget_window_secs: Option<u64>,
    pub budget_allow_unpriced: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub team_id: String,
    pub name: String,
    pub created_at: String,
}

/// How a key proves who it is. Exactly one per key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// A bearer secret, generated here and shown once.
    Secret,
    /// A client certificate identity, `dns:<name>` or `uri:<uri>` (0011).
    ClientSan(String),
    /// A workload token from a configured issuer table, by exact `sub` (0017).
    Jwt { table: String, subject: String },
}

#[derive(Debug, Clone)]
pub struct NewKey {
    pub project_id: String,
    /// The label the logs print: 1 to 64 of `a-z`, `0-9`, `-`, `_`, starting with a letter or
    /// digit. Unique within the organization.
    pub name: String,
    pub credential: Credential,
    pub tier: u8,
    pub rpm: Option<u32>,
    pub tpm: Option<u32>,
    pub budget_usd: Option<f64>,
    pub budget_window_secs: Option<u64>,
    pub budget_allow_unpriced: bool,
    pub allowed_cidrs: Option<Vec<String>>,
    /// The models the key may call (data-plane note 0028): `provider/model`, `prefix/*` or
    /// `route/<name>`. `None` is no restriction; an empty list is an error.
    pub allowed_models: Option<Vec<String>>,
    /// Labels copied onto the key's usage records (data-plane note 0029). Empty is none.
    pub tags: std::collections::BTreeMap<String, String>,
    /// RFC 3339.
    pub expires_at: Option<String>,
}

impl NewKey {
    pub fn secret(project_id: &str, name: &str) -> Self {
        Self {
            project_id: project_id.into(),
            name: name.into(),
            credential: Credential::Secret,
            tier: 0,
            rpm: None,
            tpm: None,
            budget_usd: None,
            budget_window_secs: None,
            budget_allow_unpriced: false,
            allowed_cidrs: None,
            allowed_models: None,
            tags: Default::default(),
            expires_at: None,
        }
    }
}

/// What `update_key_limits` sets (each replaces the stored value; `None` clears it).
#[derive(Debug, Clone, Default)]
pub struct LimitsUpdate {
    pub rpm: Option<u32>,
    pub tpm: Option<u32>,
    pub budget_usd: Option<f64>,
    pub budget_window_secs: Option<u64>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct KeyRecord {
    pub id: String,
    pub org_id: String,
    pub project_id: String,
    pub team_id: String,
    pub name: String,
    /// `secret`, `client_san` or `jwt`.
    pub kind: String,
    /// `active` or `revoked`. A key past its `expires_at` is still `active` here; the snapshot
    /// says `expired` and the data plane refuses it at request time.
    pub state: String,
    /// The SHA-256 of a secret key's secret. Never serialized.
    #[serde(skip)]
    pub sha256: Option<String>,
    pub client_san: Option<String>,
    pub jwt_table: Option<String>,
    pub jwt_subject: Option<String>,
    pub expires_at: Option<String>,
    pub tier: u8,
    pub rpm: Option<u32>,
    pub tpm: Option<u32>,
    pub budget_usd: Option<f64>,
    pub budget_window_secs: Option<u64>,
    pub budget_allow_unpriced: bool,
    pub allowed_cidrs: Option<Vec<String>>,
    pub allowed_models: Option<Vec<String>>,
    pub tags: std::collections::BTreeMap<String, String>,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

/// A created key and, for a `Secret`, the secret: shown once, never stored. `Debug` hides
/// it, so a `{:?}` in a log line or a failed assertion cannot leak it.
pub struct CreatedKey {
    pub record: KeyRecord,
    pub secret: Option<String>,
}

impl std::fmt::Debug for CreatedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedKey")
            .field("record", &self.record)
            .field("secret", &self.secret.as_ref().map(|_| "<shown once>"))
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HistoryRow {
    pub id: i64,
    pub at: String,
    pub actor: String,
    pub action: String,
    pub subject: String,
    pub detail: Value,
}

/// A history chain that checks out (`verify_history`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryVerified {
    pub records: u64,
    /// Rows from before the chain existed, which it cannot vouch for.
    pub unchained: u64,
    pub head: String,
}

/// The first row at which a history chain fails, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryBroken {
    pub id: i64,
    pub reason: String,
}

#[derive(Debug)]
pub struct Published {
    pub generation: u64,
    /// The signed envelope, to write where a data plane's `snapshot_file` points.
    pub envelope: String,
}

pub struct Store {
    backend: Backend,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

fn valid_label(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}

fn display_name(name: &str, what: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(Error::Invalid(format!(
            "{what} must be 1 to 128 characters, with no control characters"
        )));
    }
    Ok(())
}

fn actor_check(actor: &str) -> Result<()> {
    if actor.trim().is_empty() || actor.len() > 128 || actor.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "actor must be 1 to 128 characters, with no control characters".into(),
        ));
    }
    Ok(())
}

/// A bearer secret: 32 random bytes, URL-safe, 43 characters (the data plane requires at
/// least 22).
fn generate_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .map_err(|_| Error::Invalid("the system random source failed".into()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn digest(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

const KEY_COLUMNS: &str = "id, org_id, project_id, team_id, name, kind, state, sha256, \
    client_san, jwt_table, jwt_subject, expires_at, tier, rpm, tpm, budget_usd, \
    budget_window_secs, budget_allow_unpriced, allowed_cidrs, created_at, revoked_at, \
    allowed_models, tags";

fn key_from_row(row: &Row) -> Result<KeyRecord> {
    let range = |what: &str| Error::Invalid(format!("a stored {what} is out of range"));
    // Values outside their range, or a corrupt list, are errors: never a wrapped number, and
    // never "no restriction".
    let allowed_cidrs = match row.opt_text(18)? {
        Some(text) => Some(serde_json::from_str::<Vec<String>>(&text).map_err(|_| {
            Error::Invalid("a stored allowed_cidrs is not a list of strings".into())
        })?),
        None => None,
    };
    let allowed_models = match row.opt_text(21)? {
        Some(text) => Some(serde_json::from_str::<Vec<String>>(&text).map_err(|_| {
            Error::Invalid("a stored allowed_models is not a list of strings".into())
        })?),
        None => None,
    };
    let tags = match row.opt_text(22)? {
        Some(text) => serde_json::from_str::<std::collections::BTreeMap<String, String>>(&text)
            .map_err(|_| Error::Invalid("a stored tags value is not a map of strings".into()))?,
        None => Default::default(),
    };
    Ok(KeyRecord {
        id: row.text(0)?,
        org_id: row.text(1)?,
        project_id: row.text(2)?,
        team_id: row.text(3)?,
        name: row.text(4)?,
        kind: row.text(5)?,
        state: row.text(6)?,
        sha256: row.opt_text(7)?,
        client_san: row.opt_text(8)?,
        jwt_table: row.opt_text(9)?,
        jwt_subject: row.opt_text(10)?,
        expires_at: row.opt_text(11)?,
        tier: u8::try_from(row.int(12)?).map_err(|_| range("tier"))?,
        rpm: row
            .opt_int(13)?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| range("rpm"))?,
        tpm: row
            .opt_int(14)?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| range("tpm"))?,
        budget_usd: row.opt_real(15)?,
        budget_window_secs: row
            .opt_int(16)?
            .map(u64::try_from)
            .transpose()
            .map_err(|_| range("budget_window_secs"))?,
        budget_allow_unpriced: row.int(17)? != 0,
        allowed_cidrs,
        allowed_models,
        tags,
        created_at: row.text(19)?,
        revoked_at: row.opt_text(20)?,
    })
}

fn key_in(tx: &mut dyn Exec, id: &str) -> Result<KeyRecord> {
    let rows = tx.query(
        &format!("SELECT {KEY_COLUMNS} FROM keys WHERE id = ?1"),
        &[t(id)],
    )?;
    rows.first()
        .map(key_from_row)
        .transpose()?
        .ok_or_else(|| Error::NotFound(format!("keys {id}")))
}

fn keys_of(tx: &mut dyn Exec, org_id: &str) -> Result<Vec<KeyRecord>> {
    tx.query(
        &format!("SELECT {KEY_COLUMNS} FROM keys WHERE org_id = ?1 ORDER BY seq"),
        &[t(org_id)],
    )?
    .iter()
    .map(key_from_row)
    .collect()
}

fn teams_of(tx: &mut dyn Exec, org_id: &str) -> Result<Vec<Team>> {
    tx.query(
        "SELECT id, org_id, name, created_at, budget_usd, budget_window_secs, \
         budget_allow_unpriced FROM teams WHERE org_id = ?1 ORDER BY seq",
        &[t(org_id)],
    )?
    .iter()
    .map(team_from_row)
    .collect()
}

fn team_from_row(row: &Row) -> Result<Team> {
    let budget =
        match row.opt_real(4)? {
            // A stored amount that is not a finite number in range is an error, never "no cap":
            // JSON would write an infinity as null, and the data plane would read no budget.
            Some(budget_usd)
                if !budget_usd.is_finite()
                    || !(0.0..=llmshim::serve::config::MAX_BUDGET_USD).contains(&budget_usd) =>
            {
                return Err(Error::Invalid(
                    "a stored team budget is not a finite amount in range".into(),
                ))
            }
            Some(budget_usd) => Some(TeamBudget {
                budget_usd,
                budget_window_secs: row.opt_int(5)?.map(u64::try_from).transpose().map_err(
                    |_| Error::Invalid("a stored team budget window is out of range".into()),
                )?,
                budget_allow_unpriced: row.int(6)? != 0,
            }),
            None => None,
        };
    Ok(Team {
        id: row.text(0)?,
        org_id: row.text(1)?,
        name: row.text(2)?,
        created_at: row.text(3)?,
        budget,
    })
}

/// The rules a data plane applies to a key that need no knowledge of its configuration, so a
/// snapshot never carries a key `serve` would refuse (0024). (What depends on the data plane's
/// file, an issuer table for a token binding or `trusted_proxies` for networks, is judged there
/// and refuses the whole snapshot by path, the previous one serving.)
fn check_fields(new: &NewKey) -> Result<()> {
    let invalid = |message: &str| Err(Error::Invalid(message.into()));
    if !valid_label(&new.name) {
        return invalid(
            "a key name is 1 to 64 of a-z, 0-9, '-' and '_', starting with a letter or digit",
        );
    }
    if let Some(expires) = &new.expires_at {
        if chrono::DateTime::parse_from_rfc3339(expires).is_err() {
            return invalid("expires_at must be an RFC 3339 time");
        }
    }
    if let Some(cidrs) = &new.allowed_cidrs {
        if cidrs.is_empty() {
            return invalid("allowed_cidrs is empty, which would refuse every request; omit it");
        }
        if cidrs.len() > 256 {
            return invalid("allowed_cidrs holds more than 256 entries");
        }
        for cidr in cidrs {
            if llmshim::serve::allowlist::Cidr::parse(cidr).is_err() {
                return invalid("allowed_cidrs holds an entry that is not an address or CIDR");
            }
        }
    }
    if let Some(models) = &new.allowed_models {
        if models.is_empty() {
            return invalid("allowed_models is empty, which would refuse every request; omit it");
        }
        if models.len() > 256 {
            return invalid("allowed_models holds more than 256 entries");
        }
        for entry in models {
            if llmshim::gateway::auth::model_entry_problem(entry).is_some() {
                return invalid(
                    "allowed_models holds an entry that is not provider/model, provider/* or route/<name>",
                );
            }
        }
    }
    if new.tags.len() > 16 {
        return invalid("tags holds more than 16 tags");
    }
    for (name, value) in &new.tags {
        if llmshim::serve::config::tag_name_problem(name).is_some()
            || llmshim::serve::config::tag_value_problem(value).is_some()
        {
            return invalid(
                "a tag name is 1 to 32 of a-z, 0-9 and '_' starting with a letter, and a value is 1 to 64 printable ASCII characters without leading or trailing space",
            );
        }
    }
    check_budget(
        new.budget_usd,
        new.budget_window_secs,
        new.budget_allow_unpriced,
    )
}

fn check_budget(usd: Option<f64>, window: Option<u64>, allow_unpriced: bool) -> Result<()> {
    let invalid = |message: &str| Err(Error::Invalid(message.into()));
    if let Some(budget) = usd {
        if !budget.is_finite() || !(0.0..=llmshim::serve::config::MAX_BUDGET_USD).contains(&budget)
        {
            return invalid("budget_usd must be between 0 and 9007199.25");
        }
    }
    if window
        .is_some_and(|secs| !(1..=llmshim::serve::config::MAX_BUDGET_WINDOW_SECS).contains(&secs))
    {
        return invalid("budget_window_secs must be between 1 and 366 days");
    }
    if usd.is_none() && (window.is_some() || allow_unpriced) {
        return invalid("budget_window_secs and budget_allow_unpriced need budget_usd");
    }
    Ok(())
}

/// The next position in a table's creation order (inside a write transaction, so no two rows
/// share one): the store lists in this order, never by a random id.
fn next_seq(table: &str) -> String {
    // `table` is one of four constants at the call sites below.
    format!("(SELECT COALESCE(MAX(seq), 0) + 1 FROM {table})")
}

/// Where a history chain starts: the `prev` of the first chained row.
pub const HISTORY_GENESIS: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// The hash of one history row: SHA-256 over a JSON array of its fields in a fixed order (an
/// array, so no key order or duplicate key can spell the same row two ways), `detail` as the exact
/// text that is stored.
fn history_hash(
    prev: &str,
    at: &str,
    actor: &str,
    action: &str,
    subject: &str,
    detail: &str,
) -> String {
    let body = json!([prev, at, actor, action, subject, detail]).to_string();
    format!("{:x}", Sha256::digest(body.as_bytes()))
}

/// Append one row to the change history, chained to the row before it (note 0047). Every caller
/// runs inside a write transaction (one writer at a time, on SQLite and Postgres), so reading the
/// last hash and inserting the next row cannot interleave with another row.
fn history(
    tx: &mut dyn Exec,
    actor: &str,
    action: &str,
    subject: &str,
    detail: Value,
) -> Result<()> {
    let last = tx.query("SELECT hash FROM history ORDER BY id DESC LIMIT 1", &[])?;
    let prev = match last.first() {
        Some(row) => row
            .opt_text(0)?
            .unwrap_or_else(|| HISTORY_GENESIS.to_string()),
        None => HISTORY_GENESIS.to_string(),
    };
    let (at, detail) = (now(), detail.to_string());
    let hash = history_hash(&prev, &at, actor, action, subject, &detail);
    tx.exec(
        "INSERT INTO history (at, actor, action, subject, detail, prev, hash) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        &[
            t(&at),
            t(actor),
            t(action),
            t(subject),
            t(&detail),
            t(&prev),
            t(&hash),
        ],
    )?;
    Ok(())
}

fn require_in(tx: &mut dyn Exec, table: &str, id: &str) -> Result<()> {
    // `table` is one of two constants below, never caller text.
    debug_assert!(matches!(table, "orgs" | "teams"));
    let rows = tx.query(&format!("SELECT 1 FROM {table} WHERE id = ?1"), &[t(id)])?;
    if rows.is_empty() {
        return Err(Error::NotFound(format!("{table} {id}")));
    }
    Ok(())
}

impl Store {
    /// Open (creating if needed) the SQLite database at `path`.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let connection =
            rusqlite::Connection::open(path).map_err(|e| Error::Database(e.to_string()))?;
        Self::initialize(Backend::Sqlite(connection), true)
    }

    /// Open a database that must already be at this build's schema version: it never creates, alters or migrates
    /// anything, and says to run `llmshim-control migrate` otherwise. For a server and for read-only tools.
    pub fn open_existing(path: &std::path::Path) -> Result<Self> {
        // No CREATE flag: a path that is not there is refused, not made (with whatever mode the umask gives).
        let connection = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::CannotOpen => {
                Error::Invalid(format!(
                    "{} does not exist; run `llmshim-control migrate` to create it",
                    path.display()
                ))
            }
            other => Error::Database(other.to_string()),
        })?;
        Self::initialize(Backend::Sqlite(connection), false)
    }

    pub fn open_in_memory() -> Result<Self> {
        let connection =
            rusqlite::Connection::open_in_memory().map_err(|e| Error::Database(e.to_string()))?;
        Self::initialize(Backend::Sqlite(connection), true)
    }

    /// Connect to a Postgres database (a `postgres://` URL). TLS with the web PKI roots unless
    /// the URL says `sslmode=disable`. The URL, which holds the password, is never part of an
    /// error. `schema`, when given, is the Postgres schema to use (created if absent): several
    /// control planes, or a test, can share one database.
    pub fn open_postgres(url: &str, schema: Option<&str>) -> Result<Self> {
        Self::open_postgres_with(url, schema, None)
    }

    /// [`Self::open_postgres`] trusting the PEM certificates in `ca_file` instead of the web PKI
    /// roots (a private or provider CA).
    pub fn open_postgres_with(
        url: &str,
        schema: Option<&str>,
        ca_file: Option<&std::path::Path>,
    ) -> Result<Self> {
        Self::open_postgres_mode(url, schema, ca_file, true)
    }

    /// [`Self::open_existing`] for Postgres: the schema must already be at this build's version.
    pub fn open_postgres_existing(
        url: &str,
        schema: Option<&str>,
        ca_file: Option<&std::path::Path>,
    ) -> Result<Self> {
        Self::open_postgres_mode(url, schema, ca_file, false)
    }

    fn open_postgres_mode(
        url: &str,
        schema: Option<&str>,
        ca_file: Option<&std::path::Path>,
        migrate: bool,
    ) -> Result<Self> {
        // The synchronous Postgres client runs its own runtime, which a thread already inside
        // an async runtime may not start: connect and initialize on a thread of their own.
        std::thread::scope(|scope| {
            scope
                .spawn(|| Self::open_postgres_here(url, schema, ca_file, migrate))
                .join()
                .unwrap_or_else(|_| Err(Error::Database("the database thread failed".into())))
        })
    }

    fn open_postgres_here(
        url: &str,
        schema: Option<&str>,
        ca_file: Option<&std::path::Path>,
        migrate: bool,
    ) -> Result<Self> {
        if let Some(problem) = schema.and_then(db::schema_problem) {
            return Err(Error::Invalid(problem.into()));
        }
        let target = db::PgTarget {
            url: url.to_string(),
            ca_file: ca_file.map(std::path::Path::to_path_buf),
            schema: schema.map(str::to_string),
        };
        let client = db::connect_postgres(&target, migrate)?;
        Self::initialize(
            Backend::Postgres(db::PgClient::new(client, target)),
            migrate,
        )
    }

    /// The message for a database that is not at this build's version, when the caller may not migrate it.
    fn needs_migration(found: Option<i64>) -> Error {
        Error::Invalid(match found {
            None | Some(0) => format!(
                "the database has no schema yet; run `llmshim-control migrate` first (this build needs version {SCHEMA_VERSION})"
            ),
            Some(v) if v < SCHEMA_VERSION => format!(
                "the database has schema version {v}; this build needs {SCHEMA_VERSION}: back it up and run `llmshim-control migrate`"
            ),
            Some(v) => format!(
                "the database has schema version {v}; this build reads {SCHEMA_VERSION}: run the build that wrote it, or restore a backup"
            ),
        })
    }

    fn initialize(mut backend: Backend, migrate: bool) -> Result<Self> {
        match &mut backend {
            Backend::Sqlite(connection) => {
                connection
                    .execute_batch("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;")
                    .map_err(|e| Error::Database(e.to_string()))?;
                let version: i64 = connection
                    .query_row("PRAGMA user_version", [], |row| row.get(0))
                    .map_err(|e| Error::Database(e.to_string()))?;
                if !migrate && version != SCHEMA_VERSION {
                    return Err(Self::needs_migration(Some(version)));
                }
                // Looking must not write: the journal mode is set by a migration (and stays), never by a read.
                if migrate {
                    connection
                        .execute_batch("PRAGMA journal_mode = WAL;")
                        .map_err(|e| Error::Database(e.to_string()))?;
                }
                match version {
                    0 => {
                        connection
                            .execute_batch(&schema(false))
                            .map_err(|e| Error::Database(e.to_string()))?;
                        connection
                            .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
                            .map_err(|e| Error::Database(e.to_string()))?;
                    }
                    1..=8 => {
                        // Version 2 added keys.allowed_models, version 3 keys.tags. Re-read the
                        // version under the write lock: another process starting at the same
                        // time may have migrated while this one waited.
                        let db_err = |e: rusqlite::Error| Error::Database(e.to_string());
                        connection.execute_batch("BEGIN IMMEDIATE").map_err(db_err)?;
                        let mut now: i64 = connection
                            .query_row("PRAGMA user_version", [], |row| row.get(0))
                            .map_err(db_err)?;
                        if now == 1 {
                            connection
                                .execute_batch(
                                    "ALTER TABLE keys ADD COLUMN allowed_models TEXT; \
                                     PRAGMA user_version = 2;",
                                )
                                .map_err(db_err)?;
                            now = 2;
                        }
                        if now == 2 {
                            connection
                                .execute_batch(
                                    "ALTER TABLE keys ADD COLUMN tags TEXT; \
                                     PRAGMA user_version = 3;",
                                )
                                .map_err(db_err)?;
                            now = 3;
                        }
                        if now == 3 {
                            // Version 4: a team's budget (note 0030).
                            connection
                                .execute_batch(
                                    "ALTER TABLE teams ADD COLUMN budget_usd REAL; \
                                     ALTER TABLE teams ADD COLUMN budget_window_secs INTEGER; \
                                     ALTER TABLE teams ADD COLUMN budget_allow_unpriced \
                                         INTEGER NOT NULL DEFAULT 0; \
                                     PRAGMA user_version = 4;",
                                )
                                .map_err(db_err)?;
                            now = 4;
                        }
                        if now == 4 {
                            // Version 5: the history's hash chain (note 0047). Rows written before
                            // it keep NULL and are the unchained prefix.
                            connection
                                .execute_batch(
                                    "ALTER TABLE history ADD COLUMN prev TEXT; \
                                     ALTER TABLE history ADD COLUMN hash TEXT; \
                                     PRAGMA user_version = 5;",
                                )
                                .map_err(db_err)?;
                            now = 5;
                        }
                        if now == 5 {
                            // Version 6: the request ledger (note 0051).
                            connection
                                .execute_batch(&format!(
                                    "{} PRAGMA user_version = 6;",
                                    requests::schema_v6(false)
                                ))
                                .map_err(db_err)?;
                            now = 6;
                        }
                        if now == 6 {
                            // Version 7: prompt structure for the cache debugger (note 0052).
                            connection
                                .execute_batch(&format!(
                                    "{} PRAGMA user_version = 7;",
                                    structure::schema_v7(false)
                                ))
                                .map_err(db_err)?;
                            now = 7;
                        }
                        if now == 7 {
                            // Version 8: where a captured payload is (note 0052, slice 5b).
                            connection
                                .execute_batch(&format!(
                                    "{} PRAGMA user_version = 8;",
                                    requests::payload_columns(false)
                                ))
                                .map_err(db_err)?;
                            now = 8;
                        }
                        if now == 8 {
                            // Version 9: whose request a stored structure is (note 0056).
                            connection
                                .execute_batch(&format!(
                                    "{} PRAGMA user_version = 9;",
                                    structure::tenant_column(false)
                                ))
                                .map_err(db_err)?;
                        }
                        connection.execute_batch("COMMIT").map_err(db_err)?;
                    }
                    SCHEMA_VERSION => {}
                    other => {
                        return Err(Error::Invalid(format!(
                            "the database has schema version {other}; this build reads {SCHEMA_VERSION}"
                        )))
                    }
                }
            }
            Backend::Postgres(pg) => {
                if !migrate {
                    // Read only: nothing is created, not even the metadata table.
                    let fail = |_| Error::Database("cannot read the schema version".into());
                    let found = pg
                        .client()
                        .query_opt(
                            "SELECT to_regclass('schema_meta') IS NOT NULL AS present",
                            &[],
                        )
                        .map_err(fail)?
                        .map(|row| row.get::<_, bool>(0))
                        .unwrap_or(false);
                    let version = if found {
                        pg.client()
                            .query_opt("SELECT version FROM schema_meta", &[])
                            .map_err(fail)?
                            .map(|row| row.get::<_, i64>(0))
                    } else {
                        None
                    };
                    if version != Some(SCHEMA_VERSION) {
                        return Err(Self::needs_migration(version));
                    }
                    // Current: no DDL, no lock, so a read-only role or a standby can serve.
                    return Ok(Self { backend });
                }
                // One initializer at a time (two replicas starting together).
                let mut tx = pg
                    .client()
                    .transaction()
                    .map_err(|_| Error::Database("cannot start a transaction".into()))?;
                let fail = |_| Error::Database("cannot initialize the schema".into());
                tx.batch_execute("SELECT pg_advisory_xact_lock(7240002)")
                    .map_err(fail)?;
                tx.batch_execute(
                    "CREATE TABLE IF NOT EXISTS schema_meta (version BIGINT NOT NULL)",
                )
                .map_err(fail)?;
                let rows = tx
                    .query("SELECT version FROM schema_meta", &[])
                    .map_err(fail)?;
                match rows.first().map(|row| row.get::<_, i64>(0)) {
                    None => {
                        tx.batch_execute(&schema(true)).map_err(fail)?;
                        tx.execute("INSERT INTO schema_meta (version) VALUES ($1)", &[&SCHEMA_VERSION])
                            .map_err(fail)?;
                    }
                    Some(version @ (1..=8)) => {
                        // Version 2 added keys.allowed_models, 3 keys.tags, 4 a team's budget.
                        if version == 1 {
                            tx.batch_execute("ALTER TABLE keys ADD COLUMN allowed_models TEXT")
                                .map_err(fail)?;
                        }
                        if version <= 2 {
                            tx.batch_execute("ALTER TABLE keys ADD COLUMN tags TEXT")
                                .map_err(fail)?;
                        }
                        if version <= 3 {
                            tx.batch_execute(
                                "ALTER TABLE teams ADD COLUMN budget_usd DOUBLE PRECISION; \
                                 ALTER TABLE teams ADD COLUMN budget_window_secs BIGINT; \
                                 ALTER TABLE teams ADD COLUMN budget_allow_unpriced \
                                     BIGINT NOT NULL DEFAULT 0",
                            )
                            .map_err(fail)?;
                        }
                        // Version 5: the history's hash chain (note 0047).
                        if version <= 4 {
                            tx.batch_execute(
                                "ALTER TABLE history ADD COLUMN prev TEXT; \
                                 ALTER TABLE history ADD COLUMN hash TEXT",
                            )
                            .map_err(fail)?;
                        }
                        // Version 6: the request ledger (note 0051).
                        tx.batch_execute(&requests::schema(true)).map_err(fail)?;
                        // Version 7: prompt structure for the cache debugger (note 0052).
                        tx.batch_execute(&structure::schema(true)).map_err(fail)?;
                        // Version 8: where a captured payload is (note 0052, slice 5b). A database that was
                        // older than 6 just had the table made with them already.
                        tx.batch_execute(&requests::payload_columns(true)).map_err(fail)?;
                        // Version 9: whose request a stored structure is (note 0056); idempotent for a table that
                        // was just made with it.
                        tx.batch_execute(&structure::tenant_column(true)).map_err(fail)?;
                        tx.execute("UPDATE schema_meta SET version = $1", &[&SCHEMA_VERSION])
                            .map_err(fail)?;
                    }
                    Some(SCHEMA_VERSION) => {}
                    Some(other) => {
                        return Err(Error::Invalid(format!(
                            "the database has schema version {other}; this build reads {SCHEMA_VERSION}"
                        )))
                    }
                }
                tx.commit().map_err(fail)?;
            }
        }
        Ok(Self { backend })
    }

    pub fn is_postgres(&self) -> bool {
        self.backend.is_postgres()
    }

    pub fn create_org(&mut self, name: &str, actor: &str) -> Result<Org> {
        actor_check(actor)?;
        display_name(name, "the organization name")?;
        let org = Org {
            id: new_id("org"),
            name: name.into(),
            created_at: now(),
        };
        self.backend.transaction(true, |tx| {
            tx.exec(
                &format!(
                    "INSERT INTO orgs (id, name, created_at, seq) VALUES (?1, ?2, ?3, {})",
                    next_seq("orgs")
                ),
                &[t(&org.id), t(&org.name), t(&org.created_at)],
            )?;
            history(tx, actor, "org.created", &org.id, json!({"name": org.name}))
        })?;
        Ok(org)
    }

    pub fn create_team(&mut self, org_id: &str, name: &str, actor: &str) -> Result<Team> {
        actor_check(actor)?;
        display_name(name, "the team name")?;
        let team = Team {
            id: new_id("team"),
            org_id: org_id.into(),
            name: name.into(),
            created_at: now(),
            budget: None,
        };
        self.backend.transaction(true, |tx| {
            require_in(tx, "orgs", org_id)?;
            tx.exec(
                &format!(
                    "INSERT INTO teams (id, org_id, name, created_at, seq) VALUES (?1, ?2, ?3, ?4, {})",
                    next_seq("teams")
                ),
                &[t(&team.id), t(&team.org_id), t(&team.name), t(&team.created_at)],
            )?;
            history(
                tx,
                actor,
                "team.created",
                &team.id,
                json!({"org": org_id, "name": team.name}),
            )
        })?;
        Ok(team)
    }

    pub fn create_project(&mut self, team_id: &str, name: &str, actor: &str) -> Result<Project> {
        actor_check(actor)?;
        display_name(name, "the project name")?;
        let project = Project {
            id: new_id("proj"),
            team_id: team_id.into(),
            name: name.into(),
            created_at: now(),
        };
        self.backend.transaction(true, |tx| {
            require_in(tx, "teams", team_id)?;
            tx.exec(
                &format!(
                    "INSERT INTO projects (id, team_id, name, created_at, seq) VALUES (?1, ?2, ?3, ?4, {})",
                    next_seq("projects")
                ),
                &[t(&project.id), t(&project.team_id), t(&project.name), t(&project.created_at)],
            )?;
            history(
                tx,
                actor,
                "project.created",
                &project.id,
                json!({"team": team_id, "name": project.name}),
            )
        })?;
        Ok(project)
    }

    pub fn orgs(&mut self) -> Result<Vec<Org>> {
        self.backend.transaction(false, |tx| {
            tx.query("SELECT id, name, created_at FROM orgs ORDER BY seq", &[])?
                .iter()
                .map(|row| {
                    Ok(Org {
                        id: row.text(0)?,
                        name: row.text(1)?,
                        created_at: row.text(2)?,
                    })
                })
                .collect()
        })
    }

    pub fn projects(&mut self, team_id: &str) -> Result<Vec<Project>> {
        self.backend.transaction(false, |tx| {
            require_in(tx, "teams", team_id)?;
            tx.query(
                "SELECT id, team_id, name, created_at FROM projects WHERE team_id = ?1 ORDER BY seq",
                &[t(team_id)],
            )?
            .iter()
            .map(|row| {
                Ok(Project {
                    id: row.text(0)?,
                    team_id: row.text(1)?,
                    name: row.text(2)?,
                    created_at: row.text(3)?,
                })
            })
            .collect()
        })
    }

    /// Set (or, with `None`, clear) a team's budget: the cap over every key of the team that the
    /// data plane applies as a tenant budget (note 0030), with the data plane's own bounds.
    pub fn set_team_budget(
        &mut self,
        team_id: &str,
        budget: Option<TeamBudget>,
        actor: &str,
    ) -> Result<Team> {
        actor_check(actor)?;
        if let Some(b) = &budget {
            if !b.budget_usd.is_finite()
                || !(0.0..=llmshim::serve::config::MAX_BUDGET_USD).contains(&b.budget_usd)
            {
                return Err(Error::Invalid(format!(
                    "budget_usd must be between 0 and {}",
                    llmshim::serve::config::MAX_BUDGET_USD
                )));
            }
            if b.budget_window_secs.is_some_and(|secs| {
                !(1..=llmshim::serve::config::MAX_BUDGET_WINDOW_SECS).contains(&secs)
            }) {
                return Err(Error::Invalid(format!(
                    "budget_window_secs must be between 1 and {}",
                    llmshim::serve::config::MAX_BUDGET_WINDOW_SECS
                )));
            }
        }
        self.backend.transaction(true, |tx| {
            require_in(tx, "teams", team_id)?;
            tx.exec(
                "UPDATE teams SET budget_usd = ?2, budget_window_secs = ?3, \
                 budget_allow_unpriced = ?4 WHERE id = ?1",
                &[
                    t(team_id),
                    of(budget.as_ref().map(|b| b.budget_usd)),
                    oi(budget
                        .as_ref()
                        .and_then(|b| b.budget_window_secs)
                        .map(|v| v as i64)),
                    i(i64::from(
                        budget.as_ref().is_some_and(|b| b.budget_allow_unpriced),
                    )),
                ],
            )?;
            history(
                tx,
                actor,
                if budget.is_some() {
                    "team.budget_set"
                } else {
                    "team.budget_cleared"
                },
                team_id,
                json!({"budget_usd": budget.as_ref().map(|b| b.budget_usd),
                    "budget_window_secs": budget.as_ref().and_then(|b| b.budget_window_secs),
                    "budget_allow_unpriced":
                        budget.as_ref().is_some_and(|b| b.budget_allow_unpriced)}),
            )?;
            let rows = tx.query(
                "SELECT id, org_id, name, created_at, budget_usd, budget_window_secs, \
                 budget_allow_unpriced FROM teams WHERE id = ?1",
                &[t(team_id)],
            )?;
            team_from_row(
                rows.first()
                    .ok_or_else(|| Error::NotFound(format!("teams {team_id}")))?,
            )
        })
    }

    pub fn teams_checked(&mut self, org_id: &str) -> Result<Vec<Team>> {
        self.backend.transaction(false, |tx| {
            require_in(tx, "orgs", org_id)?;
            teams_of(tx, org_id)
        })
    }

    pub fn keys_checked(&mut self, org_id: &str) -> Result<Vec<KeyRecord>> {
        self.backend.transaction(false, |tx| {
            require_in(tx, "orgs", org_id)?;
            keys_of(tx, org_id)
        })
    }

    pub fn teams(&mut self, org_id: &str) -> Result<Vec<Team>> {
        self.backend.transaction(false, |tx| teams_of(tx, org_id))
    }

    /// Create a key in a project. A `Secret` key's secret is returned once; only its SHA-256
    /// is stored.
    pub fn create_key(&mut self, new: NewKey, actor: &str) -> Result<CreatedKey> {
        actor_check(actor)?;
        self.backend.transaction(true, |tx| {
            let (id, secret) = create_key_in(tx, &new, actor)?;
            Ok(CreatedKey {
                record: key_in(tx, &id)?,
                secret,
            })
        })
    }

    pub fn key(&mut self, id: &str) -> Result<KeyRecord> {
        self.backend.transaction(false, |tx| key_in(tx, id))
    }

    pub fn keys(&mut self, org_id: &str) -> Result<Vec<KeyRecord>> {
        self.backend.transaction(false, |tx| keys_of(tx, org_id))
    }

    /// Revoke a key. Terminal: its hash stays, so a snapshot can say it is revoked.
    pub fn revoke_key(&mut self, id: &str, actor: &str) -> Result<KeyRecord> {
        actor_check(actor)?;
        self.backend.transaction(true, |tx| {
            let record = key_in(tx, id)?;
            let changed = tx.exec(
                "UPDATE keys SET state = 'revoked', revoked_at = ?2 WHERE id = ?1 AND state = 'active'",
                &[t(id), t(&now())],
            )?;
            if changed != 1 {
                return Err(Error::Conflict(format!("key {id} is already revoked")));
            }
            history(tx, actor, "key.revoked", id, json!({"name": record.name}))?;
            key_in(tx, id)
        })
    }

    /// Rotate: create a successor with the same project, limits and restrictions under a new
    /// name, and leave the old key active until the operator revokes it after an overlap. There
    /// is no replace in place, so an incident can name exactly which credential did what. One
    /// transaction: a crash leaves no successor without its record.
    pub fn rotate_key(&mut self, id: &str, new_name: &str, actor: &str) -> Result<CreatedKey> {
        actor_check(actor)?;
        self.backend.transaction(true, |tx| {
            let old = key_in(tx, id)?;
            if old.state == "revoked" {
                return Err(Error::Conflict("a revoked key cannot be rotated".into()));
            }
            if old.kind != "secret" {
                return Err(Error::Invalid(
                    "only a secret key rotates: a certificate or token identity is the workload's own"
                        .into(),
                ));
            }
            let successor = NewKey {
                project_id: old.project_id.clone(),
                name: new_name.into(),
                credential: Credential::Secret,
                tier: old.tier,
                rpm: old.rpm,
                tpm: old.tpm,
                budget_usd: old.budget_usd,
                budget_window_secs: old.budget_window_secs,
                budget_allow_unpriced: old.budget_allow_unpriced,
                allowed_cidrs: old.allowed_cidrs.clone(),
                allowed_models: old.allowed_models.clone(),
                tags: old.tags.clone(),
                expires_at: old.expires_at.clone(),
            };
            let (successor_id, secret) = create_key_in(tx, &successor, actor)?;
            history(tx, actor, "key.rotated", id, json!({"successor": successor_id}))?;
            Ok(CreatedKey {
                record: key_in(tx, &successor_id)?,
                secret,
            })
        })
    }

    /// Change a key's limits, budget and expiry. Credentials never change in place.
    pub fn update_key_limits(
        &mut self,
        id: &str,
        update: LimitsUpdate,
        actor: &str,
    ) -> Result<KeyRecord> {
        actor_check(actor)?;
        if let Some(expires) = &update.expires_at {
            if chrono::DateTime::parse_from_rfc3339(expires).is_err() {
                return Err(Error::Invalid("expires_at must be an RFC 3339 time".into()));
            }
        }
        check_budget(update.budget_usd, update.budget_window_secs, false)?;
        self.backend.transaction(true, |tx| {
            let record = key_in(tx, id)?;
            // The unpriced flag is not part of this update: the stored one must still be valid.
            check_budget(
                update.budget_usd,
                update.budget_window_secs,
                record.budget_allow_unpriced,
            )?;
            let changed = tx.exec(
                "UPDATE keys SET rpm = ?2, tpm = ?3, budget_usd = ?4, budget_window_secs = ?5, expires_at = ?6 WHERE id = ?1 AND state = 'active'",
                &[
                    t(id),
                    oi(update.rpm.map(i64::from)),
                    oi(update.tpm.map(i64::from)),
                    of(update.budget_usd),
                    oi(update.budget_window_secs.map(|v| v as i64)),
                    ot(update.expires_at.as_deref()),
                ],
            )?;
            if changed != 1 {
                return Err(Error::Conflict("a revoked key cannot change".into()));
            }
            history(
                tx,
                actor,
                "key.limits_changed",
                id,
                json!({"rpm": update.rpm, "tpm": update.tpm, "budget_usd": update.budget_usd,
                    "budget_window_secs": update.budget_window_secs,
                    "expires_at": update.expires_at}),
            )?;
            key_in(tx, id)
        })
    }

    pub fn history_rows(&mut self, limit: u32) -> Result<Vec<HistoryRow>> {
        self.backend.transaction(false, |tx| {
            tx.query(
                "SELECT id, at, actor, action, subject, detail FROM history ORDER BY id DESC LIMIT ?1",
                &[i(i64::from(limit))],
            )?
            .iter()
            .map(|row| {
                Ok(HistoryRow {
                    id: row.int(0)?,
                    at: row.text(1)?,
                    actor: row.text(2)?,
                    action: row.text(3)?,
                    subject: row.text(4)?,
                    detail: serde_json::from_str(&row.text(5)?).unwrap_or(Value::Null),
                })
            })
            .collect()
        })
    }

    /// Check the history's hash chain from its first row (note 0047). Rows from before the chain
    /// existed (no hash) are counted as `unchained` and must come first; from the first hashed row
    /// each must carry the hash of the one before it and match its own. The file cannot show a
    /// removed last row: keep `head` somewhere the database's writer cannot reach.
    pub fn verify_history(
        &mut self,
    ) -> Result<std::result::Result<HistoryVerified, HistoryBroken>> {
        self.backend.transaction(false, |tx| {
            let rows = tx.query(
                "SELECT id, at, actor, action, subject, detail, prev, hash FROM history ORDER BY id",
                &[],
            )?;
            let (mut unchained, mut records) = (0u64, 0u64);
            let mut head = HISTORY_GENESIS.to_string();
            for row in &rows {
                let id = row.int(0)?;
                let broken = |reason: &str| {
                    Ok(Err(HistoryBroken {
                        id,
                        reason: reason.to_string(),
                    }))
                };
                let (Some(prev), Some(hash)) = (row.opt_text(6)?, row.opt_text(7)?) else {
                    if records > 0 {
                        return broken("a row without a hash after chained rows");
                    }
                    unchained += 1;
                    continue;
                };
                if prev != head {
                    return broken("does not follow the row before it (a row was removed or moved)");
                }
                let expected = history_hash(
                    &prev,
                    &row.text(1)?,
                    &row.text(2)?,
                    &row.text(3)?,
                    &row.text(4)?,
                    &row.text(5)?,
                );
                if hash != expected {
                    return broken("does not match its own hash (it was altered)");
                }
                head = hash;
                records += 1;
            }
            Ok(Ok(HistoryVerified {
                records,
                unchained,
                head,
            }))
        })
    }

    /// The payload of the next snapshot of an organization (0024): every key it has, with
    /// its state (`expired` for an active key past its time), and the next generation.
    pub fn build_payload(&mut self, org_id: &str) -> Result<Value> {
        // Under the write lock, like a publish: one consistent view on Postgres, whose reads
        // are not one snapshot.
        self.backend.transaction(true, |tx| payload_in(tx, org_id))
    }

    /// Build, sign and store the next generation of an organization's snapshot. `seed_b64` is
    /// the signing key's seed (the text `llmshim snapshot keygen` writes); it is read from a
    /// file by the caller and never stored here. The keys it lists are read in the same write
    /// transaction that stores it, so a revocation made before this returns is in it and one made
    /// after is in the next; two publishers cannot take the same generation.
    pub fn publish_snapshot(
        &mut self,
        org_id: &str,
        seed_b64: &str,
        actor: &str,
    ) -> Result<Published> {
        actor_check(actor)?;
        self.backend.transaction(true, |tx| {
            let payload = payload_in(tx, org_id)?;
            let generation = payload["generation"].as_u64().unwrap_or(0);
            let envelope =
                llmshim::serve::snapshot::sign(&payload, seed_b64).map_err(Error::Snapshot)?;
            tx.exec(
                "INSERT INTO snapshots (org_id, generation, payload, envelope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                &[
                    t(org_id),
                    i(generation as i64),
                    t(&payload.to_string()),
                    t(&envelope),
                    t(&now()),
                ],
            )
            .map_err(|error| match error {
                Error::Conflict(_) => {
                    Error::Conflict("another snapshot took this generation; publish again".into())
                }
                other => other,
            })?;
            history(tx, actor, "snapshot.published", org_id, json!({"generation": generation}))?;
            Ok(Published {
                generation,
                envelope,
            })
        })
    }

    /// The most recent published envelope of an organization.
    pub fn latest_snapshot(&mut self, org_id: &str) -> Result<Option<Published>> {
        self.backend.transaction(false, |tx| {
            let rows = tx.query(
                "SELECT generation, envelope FROM snapshots WHERE org_id = ?1 ORDER BY generation DESC LIMIT 1",
                &[t(org_id)],
            )?;
            rows.first()
                .map(|row| {
                    Ok(Published {
                        generation: row.int(0)? as u64,
                        envelope: row.text(1)?,
                    })
                })
                .transpose()
        })
    }
}

fn create_key_in(tx: &mut dyn Exec, new: &NewKey, actor: &str) -> Result<(String, Option<String>)> {
    check_fields(new)?;
    let rows = tx.query(
        "SELECT t.org_id, t.id FROM projects p JOIN teams t ON t.id = p.team_id WHERE p.id = ?1",
        &[t(&new.project_id)],
    )?;
    let (org_id, team_id) = match rows.first() {
        Some(row) => (row.text(0)?, row.text(1)?),
        None => return Err(Error::NotFound(format!("projects {}", new.project_id))),
    };
    let (kind, secret, sha256, san, table, subject) = match &new.credential {
        Credential::Secret => {
            let secret = generate_secret()?;
            let hash = digest(&secret);
            ("secret", Some(secret), Some(hash), None, None, None)
        }
        Credential::ClientSan(san) => {
            if !llmshim::serve::config::valid_client_san(san) {
                return Err(Error::Invalid(
                    "a client_san is dns:<name> or uri:<uri>, ASCII, with no whitespace".into(),
                ));
            }
            // DNS names compare case-insensitively on the data plane, so they are stored (and
            // made unique) in that form.
            let canonical = match san.strip_prefix("dns:") {
                Some(name) => format!("dns:{}", name.to_ascii_lowercase()),
                None => san.clone(),
            };
            ("client_san", None, None, Some(canonical), None, None)
        }
        Credential::Jwt { table, subject } => {
            if table.is_empty() || table.len() > 128 || !table.bytes().all(|b| b.is_ascii_graphic())
            {
                return Err(Error::Invalid(
                    "a jwt table is 1 to 128 visible ASCII characters".into(),
                ));
            }
            if subject.is_empty()
                || subject.len() > llmshim::serve::jwt::MAX_SUBJECT_BYTES
                || !subject.bytes().all(|b| b.is_ascii_graphic())
            {
                return Err(Error::Invalid(
                    "a jwt subject is 1 to 512 visible ASCII characters".into(),
                ));
            }
            (
                "jwt",
                None,
                None,
                None,
                Some(table.clone()),
                Some(subject.clone()),
            )
        }
    };
    let id = new_id("key");
    let created_at = now();
    let cidrs = new
        .allowed_cidrs
        .as_ref()
        .map(|c| serde_json::to_string(c).unwrap_or_default());
    let models = new
        .allowed_models
        .as_ref()
        .map(|m| serde_json::to_string(m).unwrap_or_default());
    let tags_json =
        (!new.tags.is_empty()).then(|| serde_json::to_string(&new.tags).unwrap_or_default());
    tx.exec(
        &format!(
            "INSERT INTO keys (id, org_id, project_id, team_id, name, kind, state, sha256, \
             client_san, jwt_table, jwt_subject, expires_at, tier, rpm, tpm, budget_usd, \
             budget_window_secs, budget_allow_unpriced, allowed_cidrs, created_at, revoked_at, \
             allowed_models, tags, seq) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
             ?16, ?17, ?18, ?19, NULL, ?20, ?21, {})",
            next_seq("keys")
        ),
        &[
            t(&id),
            t(&org_id),
            t(&new.project_id),
            t(&team_id),
            t(&new.name),
            t(kind),
            ot(sha256.as_deref()),
            ot(san.as_deref()),
            ot(table.as_deref()),
            ot(subject.as_deref()),
            ot(new.expires_at.as_deref()),
            i(i64::from(new.tier)),
            oi(new.rpm.map(i64::from)),
            oi(new.tpm.map(i64::from)),
            of(new.budget_usd),
            oi(new.budget_window_secs.map(|v| v as i64)),
            i(i64::from(new.budget_allow_unpriced)),
            ot(cidrs.as_deref()),
            t(&created_at),
            ot(models.as_deref()),
            ot(tags_json.as_deref()),
        ],
    )
    .map_err(|error| match error {
        Error::Conflict(_) => Error::Conflict(
            "a key with this name (or credential) already exists in the organization".into(),
        ),
        other => other,
    })?;
    history(
        tx,
        actor,
        "key.created",
        &id,
        json!({"project": new.project_id, "name": new.name, "kind": kind,
               "allowed_cidrs": new.allowed_cidrs, "allowed_models": new.allowed_models,
               "tags": new.tags,
            "expires_at": new.expires_at}),
    )?;
    Ok((id, secret))
}

fn payload_in(tx: &mut dyn Exec, org_id: &str) -> Result<Value> {
    require_in(tx, "orgs", org_id)?;
    let generation = tx
        .query(
            "SELECT COALESCE(MAX(generation), 0) + 1 FROM snapshots WHERE org_id = ?1",
            &[t(org_id)],
        )?
        .first()
        .ok_or_else(|| Error::Database("no row".into()))?
        .int(0)?;
    let teams: Vec<Value> = teams_of(tx, org_id)?
        .into_iter()
        .map(|team| {
            let mut entry = json!({"id": team.id, "name": team.name});
            // Emitted only when set: a snapshot without a budget is the one older data planes read.
            if let (Some(budget), Some(object)) = (&team.budget, entry.as_object_mut()) {
                object.insert("budget_usd".into(), json!(budget.budget_usd));
                if let Some(window) = budget.budget_window_secs {
                    object.insert("budget_window_secs".into(), json!(window));
                }
                if budget.budget_allow_unpriced {
                    object.insert("budget_allow_unpriced".into(), json!(true));
                }
            }
            entry
        })
        .collect();
    let at = chrono::Utc::now();
    let mut keys = Vec::new();
    for key in keys_of(tx, org_id)? {
        let expired = key
            .expires_at
            .as_deref()
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .is_some_and(|expires| expires <= at);
        let state = match (key.state.as_str(), expired) {
            ("revoked", _) => "revoked",
            (_, true) => "expired",
            _ => "active",
        };
        let mut entry = json!({
            "id": key.id, "name": key.name, "tenant": key.team_id, "state": state,
            "tier": key.tier, "budget_allow_unpriced": key.budget_allow_unpriced,
        });
        let object = entry.as_object_mut().expect("an object");
        let mut set = |name: &str, value: Value| {
            if !value.is_null() {
                object.insert(name.into(), value);
            }
        };
        set("sha256", json!(key.sha256));
        set("client_san", json!(key.client_san));
        if let (Some(table), Some(subject)) = (&key.jwt_table, &key.jwt_subject) {
            set("jwt", json!({"table": table, "subject": subject}));
        }
        set("expires_at", json!(key.expires_at));
        set("rpm", json!(key.rpm));
        set("tpm", json!(key.tpm));
        set("budget_usd", json!(key.budget_usd));
        set("budget_window_secs", json!(key.budget_window_secs));
        set("allowed_cidrs", json!(key.allowed_cidrs));
        // Emitted only when set: a snapshot without it is the one older data planes read.
        if !key.tags.is_empty() {
            set("tags", json!(key.tags));
        }
        if key.allowed_models.is_some() {
            set("allowed_models", json!(key.allowed_models));
        }
        keys.push(entry);
    }
    Ok(json!({
        "version": 1,
        "org": org_id,
        "generation": generation,
        "issued_at": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "tenants": teams,
        "keys": keys,
    }))
}

/// The schema, for the SQLite (`pg = false`) or Postgres dialect. `seq` is the creation order,
/// assigned inside the write transaction.
fn schema(pg: bool) -> String {
    let (int, real, autoid) = if pg {
        ("BIGINT", "DOUBLE PRECISION", "BIGSERIAL PRIMARY KEY")
    } else {
        ("INTEGER", "REAL", "INTEGER PRIMARY KEY AUTOINCREMENT")
    };
    format!(
        "
CREATE TABLE orgs (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL, seq {int} NOT NULL
);
CREATE TABLE teams (
    id TEXT PRIMARY KEY, org_id TEXT NOT NULL REFERENCES orgs(id),
    name TEXT NOT NULL, created_at TEXT NOT NULL, seq {int} NOT NULL,
    budget_usd {real}, budget_window_secs {int},
    budget_allow_unpriced {int} NOT NULL DEFAULT 0
);
CREATE TABLE projects (
    id TEXT PRIMARY KEY, team_id TEXT NOT NULL REFERENCES teams(id),
    name TEXT NOT NULL, created_at TEXT NOT NULL, seq {int} NOT NULL
);
CREATE TABLE keys (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES orgs(id),
    project_id TEXT NOT NULL REFERENCES projects(id),
    team_id TEXT NOT NULL REFERENCES teams(id),
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('secret', 'client_san', 'jwt')),
    state TEXT NOT NULL CHECK (state IN ('active', 'revoked')),
    sha256 TEXT UNIQUE,
    client_san TEXT, jwt_table TEXT, jwt_subject TEXT,
    expires_at TEXT,
    tier {int} NOT NULL DEFAULT 0,
    rpm {int}, tpm {int},
    budget_usd {real}, budget_window_secs {int},
    budget_allow_unpriced {int} NOT NULL DEFAULT 0,
    allowed_cidrs TEXT,
    allowed_models TEXT,
    tags TEXT,
    created_at TEXT NOT NULL,
    revoked_at TEXT,
    seq {int} NOT NULL,
    UNIQUE (org_id, name)
);
-- What the data plane refuses as a whole snapshot, refused here (an active identity is one key).
CREATE UNIQUE INDEX keys_client_san ON keys (org_id, client_san)
    WHERE client_san IS NOT NULL AND state = 'active';
CREATE UNIQUE INDEX keys_jwt ON keys (org_id, jwt_table, jwt_subject)
    WHERE jwt_table IS NOT NULL AND state = 'active';
CREATE TABLE history (
    id {autoid},
    at TEXT NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL,
    subject TEXT NOT NULL, detail TEXT NOT NULL,
    prev TEXT, hash TEXT
);
CREATE TABLE snapshots (
    org_id TEXT NOT NULL REFERENCES orgs(id),
    generation {int} NOT NULL,
    payload TEXT NOT NULL, envelope TEXT NOT NULL, created_at TEXT NOT NULL,
    PRIMARY KEY (org_id, generation)
);
{requests}
{structure}
",
        requests = requests::schema(pg),
        structure = structure::schema(pg)
    )
}

/// What [`migrate_sqlite`] and [`migrate_postgres`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// The version found (`0`: a new database).
    pub from: i64,
    pub to: i64,
    /// For SQLite, a consistent copy taken before any change (`<db>.pre-v<from>.bak`, owner-only); none for a new
    /// database or one already current. Postgres has none: take a `pg_dump` first.
    pub backup: Option<std::path::PathBuf>,
}

/// Bring a SQLite database to this build's schema. An older database is first copied (`VACUUM INTO`, a consistent
/// snapshot, owner-only) so a bad deploy can be undone by putting the copy back and running the previous build.
pub fn migrate_sqlite(path: &std::path::Path) -> Result<MigrationReport> {
    let err = |e: rusqlite::Error| Error::Database(e.to_string());
    let from: i64 = {
        let probe = rusqlite::Connection::open(path).map_err(err)?;
        probe
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(err)?
    };
    if from > SCHEMA_VERSION {
        return Err(Store::needs_migration(Some(from)));
    }
    let mut backup = None;
    if (1..SCHEMA_VERSION).contains(&from) {
        // `<db>.pre-v<N>.bak`, or `.pre-v<N>.<k>.bak` when that is taken: a copy is never overwritten and a retry
        // after a failed migration is never blocked by the copy the first try left. Created with `create_new` and mode
        // 0600 before SQLite writes into it (an existing file or symlink is refused, and there is no window in which
        // the copy is readable by others).
        let mut target = None;
        for k in 0..100 {
            let mut name = path.as_os_str().to_owned();
            name.push(if k == 0 {
                format!(".pre-v{from}.bak")
            } else {
                format!(".pre-v{from}.{k}.bak")
            });
            let candidate = std::path::PathBuf::from(name);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&candidate) {
                Ok(_) => {
                    target = Some(candidate);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(Error::Database(format!(
                        "cannot create the backup {}: {}",
                        candidate.display(),
                        e.kind()
                    )))
                }
            }
        }
        let target = target.ok_or_else(|| {
            Error::Invalid("a hundred backups of this version exist already: move some away".into())
        })?;
        let copied = (|| -> Result<()> {
            let text = target.to_str().ok_or_else(|| {
                Error::Invalid("the database path must be UTF-8 to be copied".into())
            })?;
            let probe = rusqlite::Connection::open(path).map_err(err)?;
            probe.execute("VACUUM INTO ?1", [text]).map_err(err)?;
            Ok(())
        })();
        if let Err(error) = copied {
            // A copy that did not complete is not a copy: it is removed, and the database was not touched.
            let _ = std::fs::remove_file(&target);
            return Err(error);
        }
        backup = Some(target);
    }
    Store::open(path)?;
    Ok(MigrationReport {
        from,
        to: SCHEMA_VERSION,
        backup,
    })
}

/// Bring a Postgres schema to this build's version, in one transaction under an advisory lock (so replicas
/// migrating together are safe). No copy is made: take a `pg_dump` first.
pub fn migrate_postgres(
    url: &str,
    schema: Option<&str>,
    ca_file: Option<&std::path::Path>,
) -> Result<MigrationReport> {
    if let Some(problem) = schema.and_then(db::schema_problem) {
        return Err(Error::Invalid(problem.into()));
    }
    let target = db::PgTarget {
        url: url.to_string(),
        ca_file: ca_file.map(std::path::Path::to_path_buf),
        schema: schema.map(str::to_string),
    };
    // Off the async runtime, like every other Postgres call here.
    let found: Result<Option<i64>> = std::thread::scope(|scope| {
        scope
            .spawn(|| -> Result<Option<i64>> {
                // Reading: a schema that is not there is version none, and is not created by asking.
                let mut client = match db::connect_postgres(&target, false) {
                    Ok(client) => client,
                    Err(Error::Invalid(_)) => return Ok(None),
                    Err(error) => return Err(error),
                };
                let fail = |_| Error::Database("cannot read the schema version".into());
                let present: bool = client
                    .query_one("SELECT to_regclass('schema_meta') IS NOT NULL", &[])
                    .map_err(fail)?
                    .get(0);
                if !present {
                    return Ok(None);
                }
                Ok(client
                    .query_opt("SELECT version FROM schema_meta", &[])
                    .map_err(fail)?
                    .map(|row| row.get::<_, i64>(0)))
            })
            .join()
            .unwrap_or_else(|_| Err(Error::Database("the database thread failed".into())))
    });
    let from = found?.unwrap_or(0);
    if from > SCHEMA_VERSION {
        return Err(Store::needs_migration(Some(from)));
    }
    Store::open_postgres_with(url, schema, ca_file)?;
    Ok(MigrationReport {
        from,
        to: SCHEMA_VERSION,
        backup: None,
    })
}

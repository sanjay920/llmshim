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
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("snapshot error: {0}")]
    Snapshot(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Org {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Team {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub created_at: String,
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

#[derive(Debug)]
pub struct Published {
    pub generation: u64,
    /// The signed envelope, to write where a data plane's `snapshot_file` points.
    pub envelope: String,
}

pub struct Store {
    conn: Connection,
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
    if name.trim().is_empty() || name.len() > 128 {
        return Err(Error::Invalid(format!(
            "{what} must be 1 to 128 characters"
        )));
    }
    Ok(())
}

fn actor_check(actor: &str) -> Result<()> {
    if actor.trim().is_empty() || actor.len() > 128 {
        return Err(Error::Invalid("actor must be 1 to 128 characters".into()));
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
    budget_window_secs, budget_allow_unpriced, allowed_cidrs, created_at, revoked_at";

fn bad(column: usize, what: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Integer,
        what.to_string().into(),
    )
}

fn key_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<KeyRecord> {
    let cidrs: Option<String> = row.get(18)?;
    // Values outside their range, or a corrupt list, are errors: never a wrapped number, and
    // never "no restriction".
    let allowed_cidrs = match cidrs {
        Some(text) => Some(
            serde_json::from_str::<Vec<String>>(&text)
                .map_err(|_| bad(18, "allowed_cidrs is not a list of strings"))?,
        ),
        None => None,
    };
    Ok(KeyRecord {
        id: row.get(0)?,
        org_id: row.get(1)?,
        project_id: row.get(2)?,
        team_id: row.get(3)?,
        name: row.get(4)?,
        kind: row.get(5)?,
        state: row.get(6)?,
        sha256: row.get(7)?,
        client_san: row.get(8)?,
        jwt_table: row.get(9)?,
        jwt_subject: row.get(10)?,
        expires_at: row.get(11)?,
        tier: u8::try_from(row.get::<_, i64>(12)?).map_err(|_| bad(12, "tier out of range"))?,
        rpm: row
            .get::<_, Option<i64>>(13)?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| bad(13, "rpm out of range"))?,
        tpm: row
            .get::<_, Option<i64>>(14)?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| bad(14, "tpm out of range"))?,
        budget_usd: row.get(15)?,
        budget_window_secs: row
            .get::<_, Option<i64>>(16)?
            .map(u64::try_from)
            .transpose()
            .map_err(|_| bad(16, "budget_window_secs out of range"))?,
        budget_allow_unpriced: row.get::<_, i64>(17)? != 0,
        allowed_cidrs,
        created_at: row.get(19)?,
        revoked_at: row.get(20)?,
    })
}

fn key_in(conn: &Connection, id: &str) -> Result<KeyRecord> {
    conn.query_row(
        &format!("SELECT {KEY_COLUMNS} FROM keys WHERE id = ?1"),
        [id],
        key_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("keys {id}")))
}

fn keys_of(conn: &Connection, org_id: &str) -> Result<Vec<KeyRecord>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {KEY_COLUMNS} FROM keys WHERE org_id = ?1 ORDER BY rowid"
    ))?;
    let rows = statement.query_map([org_id], key_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn teams_of(conn: &Connection, org_id: &str) -> Result<Vec<Team>> {
    let mut statement = conn.prepare(
        "SELECT id, org_id, name, created_at FROM teams WHERE org_id = ?1 ORDER BY rowid",
    )?;
    let rows = statement.query_map([org_id], |row| {
        Ok(Team {
            id: row.get(0)?,
            org_id: row.get(1)?,
            name: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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
        for cidr in cidrs {
            if llmshim::serve::allowlist::Cidr::parse(cidr).is_err() {
                return invalid("allowed_cidrs holds an entry that is not an address or CIDR");
            }
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

impl Store {
    /// Open (creating if needed) the SQLite database at `path`.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => {
                conn.execute_batch(SCHEMA)?;
                conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
            }
            SCHEMA_VERSION => {}
            other => {
                return Err(Error::Invalid(format!(
                    "the database has schema version {other}; this build reads {SCHEMA_VERSION}"
                )))
            }
        }
        Ok(Self { conn })
    }

    fn history(
        tx: &rusqlite::Transaction<'_>,
        actor: &str,
        action: &str,
        subject: &str,
        detail: Value,
    ) -> Result<()> {
        tx.execute(
            "INSERT INTO history (at, actor, action, subject, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now(), actor, action, subject, detail.to_string()],
        )?;
        Ok(())
    }

    pub fn create_org(&mut self, name: &str, actor: &str) -> Result<Org> {
        actor_check(actor)?;
        display_name(name, "the organization name")?;
        let org = Org {
            id: new_id("org"),
            name: name.into(),
            created_at: now(),
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO orgs (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![org.id, org.name, org.created_at],
        )?;
        Self::history(
            &tx,
            actor,
            "org.created",
            &org.id,
            json!({"name": org.name}),
        )?;
        tx.commit()?;
        Ok(org)
    }

    pub fn create_team(&mut self, org_id: &str, name: &str, actor: &str) -> Result<Team> {
        actor_check(actor)?;
        display_name(name, "the team name")?;
        self.require("orgs", org_id)?;
        let team = Team {
            id: new_id("team"),
            org_id: org_id.into(),
            name: name.into(),
            created_at: now(),
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO teams (id, org_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![team.id, team.org_id, team.name, team.created_at],
        )?;
        Self::history(
            &tx,
            actor,
            "team.created",
            &team.id,
            json!({"org": org_id, "name": team.name}),
        )?;
        tx.commit()?;
        Ok(team)
    }

    pub fn create_project(&mut self, team_id: &str, name: &str, actor: &str) -> Result<Project> {
        actor_check(actor)?;
        display_name(name, "the project name")?;
        self.require("teams", team_id)?;
        let project = Project {
            id: new_id("proj"),
            team_id: team_id.into(),
            name: name.into(),
            created_at: now(),
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO projects (id, team_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                project.id,
                project.team_id,
                project.name,
                project.created_at
            ],
        )?;
        Self::history(
            &tx,
            actor,
            "project.created",
            &project.id,
            json!({"team": team_id, "name": project.name}),
        )?;
        tx.commit()?;
        Ok(project)
    }

    fn require(&self, table: &str, id: &str) -> Result<()> {
        let found: Option<i64> = self
            .conn
            .query_row(&format!("SELECT 1 FROM {table} WHERE id = ?1"), [id], |r| {
                r.get(0)
            })
            .optional()?;
        found
            .map(|_| ())
            .ok_or_else(|| Error::NotFound(format!("{table} {id}")))
    }

    pub fn teams(&self, org_id: &str) -> Result<Vec<Team>> {
        teams_of(&self.conn, org_id)
    }

    /// A write transaction that takes the database's write lock at once, so what a method
    /// reads to decide is still true when it writes (two processes cannot both pass the same
    /// check).
    fn write(&mut self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?)
    }

    /// Create a key in a project. A `Secret` key's secret is returned once; only its SHA-256
    /// is stored.
    pub fn create_key(&mut self, new: NewKey, actor: &str) -> Result<CreatedKey> {
        actor_check(actor)?;
        let tx = self.write()?;
        let (id, secret) = create_key_in(&tx, &new, actor)?;
        let record = key_in(&tx, &id)?;
        tx.commit()?;
        Ok(CreatedKey { record, secret })
    }

    pub fn key(&self, id: &str) -> Result<KeyRecord> {
        key_in(&self.conn, id)
    }

    pub fn keys(&self, org_id: &str) -> Result<Vec<KeyRecord>> {
        keys_of(&self.conn, org_id)
    }

    /// Revoke a key. Terminal: its hash stays, so a snapshot can say it is revoked.
    pub fn revoke_key(&mut self, id: &str, actor: &str) -> Result<KeyRecord> {
        actor_check(actor)?;
        let tx = self.write()?;
        let record = key_in(&tx, id)?;
        let changed = tx.execute(
            "UPDATE keys SET state = 'revoked', revoked_at = ?2 WHERE id = ?1 AND state = 'active'",
            params![id, now()],
        )?;
        if changed != 1 {
            return Err(Error::Conflict(format!("key {id} is already revoked")));
        }
        Self::history(&tx, actor, "key.revoked", id, json!({"name": record.name}))?;
        let revoked = key_in(&tx, id)?;
        tx.commit()?;
        Ok(revoked)
    }

    /// Rotate: create a successor with the same project, limits and restrictions under a new
    /// name, and leave the old key active until the operator revokes it after an overlap. There
    /// is no replace in place, so an incident can name exactly which credential did what. One
    /// transaction: a crash leaves no successor without its record.
    pub fn rotate_key(&mut self, id: &str, new_name: &str, actor: &str) -> Result<CreatedKey> {
        actor_check(actor)?;
        let tx = self.write()?;
        let old = key_in(&tx, id)?;
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
            expires_at: old.expires_at.clone(),
        };
        let (successor_id, secret) = create_key_in(&tx, &successor, actor)?;
        Self::history(
            &tx,
            actor,
            "key.rotated",
            id,
            json!({"successor": successor_id}),
        )?;
        let record = key_in(&tx, &successor_id)?;
        tx.commit()?;
        Ok(CreatedKey { record, secret })
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
        let tx = self.write()?;
        let record = key_in(&tx, id)?;
        // The unpriced flag is not part of this update: the stored one must still be valid.
        check_budget(
            update.budget_usd,
            update.budget_window_secs,
            record.budget_allow_unpriced,
        )?;
        let changed = tx.execute(
            "UPDATE keys SET rpm = ?2, tpm = ?3, budget_usd = ?4, budget_window_secs = ?5, expires_at = ?6 WHERE id = ?1 AND state = 'active'",
            params![
                id,
                update.rpm.map(i64::from),
                update.tpm.map(i64::from),
                update.budget_usd,
                update.budget_window_secs.map(|v| v as i64),
                update.expires_at
            ],
        )?;
        if changed != 1 {
            return Err(Error::Conflict("a revoked key cannot change".into()));
        }
        Self::history(
            &tx,
            actor,
            "key.limits_changed",
            id,
            json!({"rpm": update.rpm, "tpm": update.tpm, "budget_usd": update.budget_usd,
                "budget_window_secs": update.budget_window_secs,
                "expires_at": update.expires_at}),
        )?;
        let updated = key_in(&tx, id)?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn history_rows(&self, limit: u32) -> Result<Vec<HistoryRow>> {
        let mut statement = self.conn.prepare(
            "SELECT id, at, actor, action, subject, detail FROM history ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            let detail: String = row.get(5)?;
            Ok(HistoryRow {
                id: row.get(0)?,
                at: row.get(1)?,
                actor: row.get(2)?,
                action: row.get(3)?,
                subject: row.get(4)?,
                detail: serde_json::from_str(&detail).unwrap_or(Value::Null),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The payload of the next snapshot of an organization (0024): every key it has, with
    /// its state (`expired` for an active key past its time), and the next generation.
    pub fn build_payload(&self, org_id: &str) -> Result<Value> {
        payload_in(&self.conn, org_id)
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
        let tx = self.write()?;
        let payload = payload_in(&tx, org_id)?;
        let generation = payload["generation"].as_u64().unwrap_or(0);
        let envelope =
            llmshim::serve::snapshot::sign(&payload, seed_b64).map_err(Error::Snapshot)?;
        match tx.execute(
            "INSERT INTO snapshots (org_id, generation, payload, envelope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![org_id, generation as i64, payload.to_string(), envelope, now()],
        ) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(Error::Conflict(
                    "another snapshot took this generation; publish again".into(),
                ))
            }
            Err(other) => return Err(other.into()),
        }
        Self::history(
            &tx,
            actor,
            "snapshot.published",
            org_id,
            json!({"generation": generation}),
        )?;
        tx.commit()?;
        Ok(Published {
            generation,
            envelope,
        })
    }

    /// The most recent published envelope of an organization.
    pub fn latest_snapshot(&self, org_id: &str) -> Result<Option<Published>> {
        Ok(self
            .conn
            .query_row(
                "SELECT generation, envelope FROM snapshots WHERE org_id = ?1 ORDER BY generation DESC LIMIT 1",
                [org_id],
                |row| {
                    Ok(Published {
                        generation: row.get::<_, i64>(0)? as u64,
                        envelope: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }
}

fn require_in(conn: &Connection, table: &str, id: &str) -> Result<()> {
    // `table` is one of two constants below, never caller text.
    debug_assert!(matches!(table, "orgs" | "teams"));
    let found: Option<i64> = conn
        .query_row(&format!("SELECT 1 FROM {table} WHERE id = ?1"), [id], |r| {
            r.get(0)
        })
        .optional()?;
    found
        .map(|_| ())
        .ok_or_else(|| Error::NotFound(format!("{table} {id}")))
}

fn create_key_in(
    tx: &rusqlite::Transaction<'_>,
    new: &NewKey,
    actor: &str,
) -> Result<(String, Option<String>)> {
    check_fields(new)?;
    let (org_id, team_id): (String, String) = tx
        .query_row(
            "SELECT t.org_id, t.id FROM projects p JOIN teams t ON t.id = p.team_id WHERE p.id = ?1",
            [&new.project_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("projects {}", new.project_id)))?;
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
            ("client_san", None, None, Some(san.clone()), None, None)
        }
        Credential::Jwt { table, subject } => {
            if table.is_empty() || subject.is_empty() {
                return Err(Error::Invalid(
                    "a jwt binding needs a table and a subject".into(),
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
    let inserted = tx.execute(
        &format!("INSERT INTO keys ({KEY_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, NULL)"),
        params![
            id,
            org_id,
            new.project_id,
            team_id,
            new.name,
            kind,
            sha256,
            san,
            table,
            subject,
            new.expires_at,
            new.tier as i64,
            new.rpm.map(i64::from),
            new.tpm.map(i64::from),
            new.budget_usd,
            new.budget_window_secs.map(|v| v as i64),
            new.budget_allow_unpriced as i64,
            new.allowed_cidrs
                .as_ref()
                .map(|c| serde_json::to_string(c).unwrap_or_default()),
            created_at,
        ],
    );
    match inserted {
        Ok(_) => {}
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            return Err(Error::Conflict(
                "a key with this name (or credential) already exists in the organization".into(),
            ))
        }
        Err(other) => return Err(other.into()),
    }
    Store::history(
        tx,
        actor,
        "key.created",
        &id,
        json!({"project": new.project_id, "name": new.name, "kind": kind,
            "expires_at": new.expires_at}),
    )?;
    Ok((id, secret))
}

fn payload_in(conn: &Connection, org_id: &str) -> Result<Value> {
    require_in(conn, "orgs", org_id)?;
    let generation: i64 = conn.query_row(
        "SELECT COALESCE(MAX(generation), 0) + 1 FROM snapshots WHERE org_id = ?1",
        [org_id],
        |row| row.get(0),
    )?;
    let teams: Vec<Value> = teams_of(conn, org_id)?
        .into_iter()
        .map(|t| json!({"id": t.id, "name": t.name}))
        .collect();
    let at = chrono::Utc::now();
    let mut keys = Vec::new();
    for key in keys_of(conn, org_id)? {
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

const SCHEMA: &str = "
CREATE TABLE orgs (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL
);
CREATE TABLE teams (
    id TEXT PRIMARY KEY, org_id TEXT NOT NULL REFERENCES orgs(id),
    name TEXT NOT NULL, created_at TEXT NOT NULL
);
CREATE TABLE projects (
    id TEXT PRIMARY KEY, team_id TEXT NOT NULL REFERENCES teams(id),
    name TEXT NOT NULL, created_at TEXT NOT NULL
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
    tier INTEGER NOT NULL DEFAULT 0,
    rpm INTEGER, tpm INTEGER,
    budget_usd REAL, budget_window_secs INTEGER,
    budget_allow_unpriced INTEGER NOT NULL DEFAULT 0,
    allowed_cidrs TEXT,
    created_at TEXT NOT NULL,
    revoked_at TEXT,
    UNIQUE (org_id, name)
);
CREATE TABLE history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL,
    subject TEXT NOT NULL, detail TEXT NOT NULL
);
CREATE TABLE snapshots (
    org_id TEXT NOT NULL REFERENCES orgs(id),
    generation INTEGER NOT NULL,
    payload TEXT NOT NULL, envelope TEXT NOT NULL, created_at TEXT NOT NULL,
    PRIMARY KEY (org_id, generation)
);
";

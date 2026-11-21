//! The store's two backends behind one small interface (0024 slice 4): SQLite (one node, no
//! infrastructure) and Postgres. The control plane's SQL is written once, with numbered `?N`
//! placeholders; the Postgres side rewrites them to `$N`. A transaction is the unit: a write
//! transaction serializes all writers (SQLite `BEGIN IMMEDIATE`; Postgres a transaction-level
//! advisory lock), so what a method reads to decide is still true when it writes.

use crate::{Error, Result};
use rusqlite::types::ValueRef;

/// A bound value. Every variant is optional and typed, because Postgres types a NULL parameter
/// by what it is sent as.
#[derive(Debug, Clone)]
pub(crate) enum Param {
    Text(Option<String>),
    Int(Option<i64>),
    Real(Option<f64>),
}

pub(crate) fn t(value: &str) -> Param {
    Param::Text(Some(value.to_string()))
}
pub(crate) fn ot(value: Option<&str>) -> Param {
    Param::Text(value.map(str::to_string))
}
pub(crate) fn i(value: i64) -> Param {
    Param::Int(Some(value))
}
pub(crate) fn oi(value: Option<i64>) -> Param {
    Param::Int(value)
}
pub(crate) fn of(value: Option<f64>) -> Param {
    Param::Real(value)
}

#[derive(Debug, Clone)]
pub(crate) enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

#[derive(Debug, Clone)]
pub(crate) struct Row(Vec<Cell>);

fn corrupt(column: usize, what: &str) -> Error {
    Error::Invalid(format!("the stored value in column {column} {what}"))
}

impl Row {
    pub(crate) fn text(&self, column: usize) -> Result<String> {
        match self.0.get(column) {
            Some(Cell::Text(value)) => Ok(value.clone()),
            _ => Err(corrupt(column, "is not text")),
        }
    }
    pub(crate) fn opt_text(&self, column: usize) -> Result<Option<String>> {
        match self.0.get(column) {
            Some(Cell::Null) => Ok(None),
            Some(Cell::Text(value)) => Ok(Some(value.clone())),
            _ => Err(corrupt(column, "is not text")),
        }
    }
    pub(crate) fn int(&self, column: usize) -> Result<i64> {
        match self.0.get(column) {
            Some(Cell::Int(value)) => Ok(*value),
            _ => Err(corrupt(column, "is not an integer")),
        }
    }
    pub(crate) fn opt_int(&self, column: usize) -> Result<Option<i64>> {
        match self.0.get(column) {
            Some(Cell::Null) => Ok(None),
            Some(Cell::Int(value)) => Ok(Some(*value)),
            _ => Err(corrupt(column, "is not an integer")),
        }
    }
    pub(crate) fn opt_real(&self, column: usize) -> Result<Option<f64>> {
        match self.0.get(column) {
            Some(Cell::Null) => Ok(None),
            Some(Cell::Real(value)) => Ok(Some(*value)),
            Some(Cell::Int(value)) => Ok(Some(*value as f64)),
            _ => Err(corrupt(column, "is not a number")),
        }
    }
}

/// What a transaction can do. Unique-constraint violations are an [`Error::Conflict`] with a
/// generic message; callers that know which rule it was say so.
pub(crate) trait Exec {
    fn exec(&mut self, sql: &str, params: &[Param]) -> Result<u64>;
    fn query(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Row>>;
}

/// A Postgres client that is dropped on a thread of its own: the synchronous client drives
/// a runtime when it closes, which a thread inside another runtime (where a `Store` may well
/// be dropped) may not do.
pub(crate) struct PgClient {
    client: Option<Box<postgres::Client>>,
    target: PgTarget,
}

impl PgClient {
    pub(crate) fn new(client: postgres::Client, target: PgTarget) -> Self {
        Self {
            client: Some(Box::new(client)),
            target,
        }
    }

    /// The client, replaced first if its connection has died (a failover, a restart, an idle
    /// kill): a database blip must not need a control-plane restart.
    /// Replace the connection now.
    pub(crate) fn reconnect(&mut self) {
        if let Ok(fresh) = connect_postgres(&self.target) {
            self.client = Some(Box::new(fresh));
        }
    }

    pub(crate) fn client(&mut self) -> &mut postgres::Client {
        if self.client.as_ref().is_some_and(|c| c.is_closed()) {
            if let Ok(fresh) = connect_postgres(&self.target) {
                self.client = Some(Box::new(fresh));
            }
        }
        self.client.as_mut().expect("a client until it is dropped")
    }
}

impl Drop for PgClient {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let _ = std::thread::spawn(move || drop(client)).join();
        }
    }
}

/// How long one SQLite transaction may run (Postgres: `statement_timeout`, 30 s).
const SQLITE_STATEMENT_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

pub(crate) enum Backend {
    Sqlite(rusqlite::Connection),
    Postgres(PgClient),
}

fn database(error: impl std::fmt::Display) -> Error {
    // Detail goes to the log by the caller of the API; it never carries a secret (a
    // connection URL is never part of an error here).
    Error::Database(error.to_string())
}

impl Backend {
    pub(crate) fn is_postgres(&self) -> bool {
        matches!(self, Backend::Postgres(_))
    }

    /// Run `work` in one transaction: committed if it returns `Ok`, rolled back if `Err`.
    pub(crate) fn transaction<T: Send>(
        &mut self,
        write: bool,
        work: impl FnOnce(&mut dyn Exec) -> Result<T> + Send,
    ) -> Result<T> {
        match self {
            Backend::Sqlite(connection) => {
                let behavior = if write {
                    rusqlite::TransactionBehavior::Immediate
                } else {
                    rusqlite::TransactionBehavior::Deferred
                };
                // The same bound Postgres has as `statement_timeout`: a transaction running past it is interrupted
                // (SQLite checks the handler every thousand virtual-machine steps), so one slow query cannot hold the
                // connection, and with it every request, for ever.
                let deadline = std::time::Instant::now() + SQLITE_STATEMENT_BUDGET;
                connection
                    .progress_handler(1000, Some(move || std::time::Instant::now() > deadline));
                let result = (|| {
                    let tx = connection
                        .transaction_with_behavior(behavior)
                        .map_err(database)?;
                    let value = work(&mut SqliteExec(&tx))?;
                    tx.commit().map_err(database)?;
                    Ok(value)
                })();
                connection.progress_handler(1000, None::<fn() -> bool>);
                result
            }
            Backend::Postgres(client) => {
                // The synchronous client drives its own runtime, which a thread inside another
                // (a tokio worker, or a `spawn_blocking` thread of one) may not do: each
                // transaction runs on a thread of its own, scoped to this call.
                std::thread::scope(|scope| {
                    scope
                        .spawn(|| -> Result<T> {
                            // A dead connection (a failover, an idle kill) is found by a ping
                            // before any work, and replaced; nothing is retried once work has begun.
                            if client.client().simple_query("SELECT 1").is_err() {
                                client.reconnect();
                            }
                            let mut tx = client.client().transaction().map_err(database)?;
                            if write {
                                // One writer at a time, for the length of the transaction.
                                tx.execute("SELECT pg_advisory_xact_lock(7240001)", &[])
                                    .map_err(database)?;
                            }
                            let result = work(&mut PgExec(&mut tx));
                            match result {
                                Ok(value) => {
                                    tx.commit().map_err(database)?;
                                    Ok(value)
                                }
                                Err(error) => Err(error),
                            }
                        })
                        .join()
                        .unwrap_or_else(|_| {
                            Err(Error::Database("the database thread failed".into()))
                        })
                })
            }
        }
    }
}

struct SqliteExec<'a>(&'a rusqlite::Transaction<'a>);

impl rusqlite::ToSql for Param {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(match self {
            Param::Text(v) => v.to_sql()?,
            Param::Int(v) => v.to_sql()?,
            Param::Real(v) => v.to_sql()?,
        })
    }
}

fn sqlite_error(error: rusqlite::Error) -> Error {
    match error {
        // Only a uniqueness rule is a conflict; a foreign-key, check or not-null failure is not.
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY =>
        {
            Error::Conflict("a uniqueness rule was violated".into())
        }
        other => database(other),
    }
}

impl Exec for SqliteExec<'_> {
    fn exec(&mut self, sql: &str, params: &[Param]) -> Result<u64> {
        let mut statement = self.0.prepare_cached(sql).map_err(sqlite_error)?;
        statement
            .execute(rusqlite::params_from_iter(params.iter()))
            .map(|n| n as u64)
            .map_err(sqlite_error)
    }

    fn query(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Row>> {
        let mut statement = self.0.prepare_cached(sql).map_err(sqlite_error)?;
        let columns = statement.column_count();
        let mut rows = statement
            .query(rusqlite::params_from_iter(params.iter()))
            .map_err(sqlite_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            let mut cells = Vec::with_capacity(columns);
            for column in 0..columns {
                cells.push(match row.get_ref(column).map_err(sqlite_error)? {
                    ValueRef::Null => Cell::Null,
                    ValueRef::Integer(v) => Cell::Int(v),
                    ValueRef::Real(v) => Cell::Real(v),
                    ValueRef::Text(v) => Cell::Text(String::from_utf8_lossy(v).into_owned()),
                    ValueRef::Blob(_) => return Err(corrupt(column, "is a blob")),
                });
            }
            out.push(Row(cells));
        }
        Ok(out)
    }
}

struct PgExec<'a, 'b>(&'a mut postgres::Transaction<'b>);

/// `?1` to `$1`: the SQL is written once with numbered placeholders.
fn pg_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '?' && chars.peek().is_some_and(char::is_ascii_digit) {
            out.push('$');
        } else {
            out.push(c);
        }
    }
    out
}

fn pg_error(error: postgres::Error) -> Error {
    if error.code() == Some(&postgres::error::SqlState::UNIQUE_VIOLATION) {
        return Error::Conflict("a uniqueness rule was violated".into());
    }
    // The server's message can quote a value; it is not passed on.
    database(
        error
            .code()
            .map(|c| c.code().to_string())
            .unwrap_or_else(|| "postgres".into()),
    )
}

fn pg_params(params: &[Param]) -> Vec<&(dyn postgres::types::ToSql + Sync)> {
    params
        .iter()
        .map(|p| match p {
            Param::Text(v) => v as &(dyn postgres::types::ToSql + Sync),
            Param::Int(v) => v as &(dyn postgres::types::ToSql + Sync),
            Param::Real(v) => v as &(dyn postgres::types::ToSql + Sync),
        })
        .collect()
}

impl Exec for PgExec<'_, '_> {
    fn exec(&mut self, sql: &str, params: &[Param]) -> Result<u64> {
        self.0
            .execute(pg_sql(sql).as_str(), &pg_params(params))
            .map_err(pg_error)
    }

    fn query(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Row>> {
        let rows = self
            .0
            .query(pg_sql(sql).as_str(), &pg_params(params))
            .map_err(pg_error)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut cells = Vec::with_capacity(row.len());
            for (column, meta) in row.columns().iter().enumerate() {
                use postgres::types::Type;
                let ty = meta.type_();
                let cell = if *ty == Type::INT8 {
                    row.try_get::<_, Option<i64>>(column)
                        .map_err(pg_error)?
                        .map_or(Cell::Null, Cell::Int)
                } else if *ty == Type::INT4 {
                    row.try_get::<_, Option<i32>>(column)
                        .map_err(pg_error)?
                        .map_or(Cell::Null, |v| Cell::Int(i64::from(v)))
                } else if *ty == Type::INT2 {
                    row.try_get::<_, Option<i16>>(column)
                        .map_err(pg_error)?
                        .map_or(Cell::Null, |v| Cell::Int(i64::from(v)))
                } else if *ty == Type::FLOAT8 {
                    row.try_get::<_, Option<f64>>(column)
                        .map_err(pg_error)?
                        .map_or(Cell::Null, Cell::Real)
                } else if *ty == Type::TEXT || *ty == Type::VARCHAR {
                    row.try_get::<_, Option<String>>(column)
                        .map_err(pg_error)?
                        .map_or(Cell::Null, Cell::Text)
                } else {
                    return Err(corrupt(column, "has a type the store does not read"));
                };
                cells.push(cell);
            }
            out.push(Row(cells));
        }
        Ok(out)
    }
}

/// How to reach a Postgres database, kept so a dead connection can be replaced.
#[derive(Clone)]
pub(crate) struct PgTarget {
    pub(crate) url: String,
    pub(crate) ca_file: Option<std::path::PathBuf>,
    pub(crate) schema: Option<String>,
}

/// Connect to Postgres. **TLS is required** (rustls, the host verified, the web PKI roots or
/// `ca_file`'s) unless the URL says `sslmode=disable`: `prefer` and `require` both mean
/// required, so a server (or a man in the middle) that refuses TLS fails the connect and
/// never gets the password in the clear. Timeouts and keepalives bound every wait: a lock
/// held by a session that vanished must not hold the control plane for hours.
pub(crate) fn connect_postgres(target: &PgTarget) -> Result<postgres::Client> {
    let mut config: postgres::Config = target.url.parse().map_err(|_| {
        Error::Invalid("the database URL is not a Postgres connection string".into())
    })?;
    config
        .connect_timeout(std::time::Duration::from_secs(10))
        .keepalives(true)
        .keepalives_idle(std::time::Duration::from_secs(30))
        .options("-c statement_timeout=30000 -c lock_timeout=30000 -c idle_in_transaction_session_timeout=60000");
    let failed = |_| Error::Database("cannot connect to the database".into());
    let mut client = if config.get_ssl_mode() == postgres::config::SslMode::Disable {
        config.connect(postgres::NoTls).map_err(failed)?
    } else {
        config.ssl_mode(postgres::config::SslMode::Require);
        let mut roots = rustls::RootCertStore::empty();
        match &target.ca_file {
            Some(path) => {
                let file = std::fs::File::open(path)
                    .map_err(|_| Error::Invalid("the CA file cannot be read".into()))?;
                let mut added = 0;
                for certificate in rustls_pemfile::certs(&mut std::io::BufReader::new(file)) {
                    let certificate = certificate.map_err(|_| {
                        Error::Invalid("the CA file is not PEM certificates".into())
                    })?;
                    roots.add(certificate).map_err(|_| {
                        Error::Invalid("the CA file holds a bad certificate".into())
                    })?;
                    added += 1;
                }
                if added == 0 {
                    return Err(Error::Invalid("the CA file holds no certificate".into()));
                }
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| Error::Database("TLS could not be set up".into()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        config
            .connect(tokio_postgres_rustls::MakeRustlsConnect::new(tls))
            .map_err(failed)?
    };
    if let Some(schema) = &target.schema {
        // The schema name is validated by the caller (letters, digits, underscore; not a
        // reserved one). Created under a lock of its own: two replicas starting together.
        client
            .batch_execute(&format!(
                "BEGIN; SELECT pg_advisory_xact_lock(7240003); \
                 CREATE SCHEMA IF NOT EXISTS {schema}; COMMIT; SET search_path TO {schema}"
            ))
            .map_err(|_| Error::Database("cannot set the schema".into()))?;
    }
    Ok(client)
}

/// A schema name the store will create and use.
pub(crate) fn schema_problem(schema: &str) -> Option<&'static str> {
    let bytes = schema.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return Some("a schema name is 1 to 63 characters");
    }
    if !bytes[0].is_ascii_lowercase() {
        return Some("a schema name starts with a letter a-z");
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
    {
        return Some("a schema name is a-z, 0-9 and '_'");
    }
    if schema.starts_with("pg_") || schema == "information_schema" {
        return Some("a schema name may not be a system schema");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_rewritten_for_postgres_and_nothing_else_is_touched() {
        assert_eq!(
            pg_sql("INSERT INTO t (a, b) VALUES (?1, ?10) WHERE x = 'a?b' AND y = ?2"),
            "INSERT INTO t (a, b) VALUES ($1, $10) WHERE x = 'a?b' AND y = $2"
        );
    }

    #[test]
    fn a_sqlite_transaction_past_its_budget_is_interrupted() {
        let mut backend = Backend::Sqlite(rusqlite::Connection::open_in_memory().unwrap());
        // An unbounded recursive query: only the deadline can end it. The budget is 30 s, so this test arms its own
        // shorter one through the same handler.
        let Backend::Sqlite(connection) = &mut backend else {
            unreachable!()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        connection.progress_handler(1000, Some(move || std::time::Instant::now() > deadline));
        let error = connection
            .query_row(
                "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) SELECT count(*) FROM n",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_err();
        assert!(error.to_string().contains("interrupt"), "{error}");
        // The handler is cleared after a transaction, so the next one is not cut by an old deadline.
        let rows = backend
            .transaction(false, |tx| tx.query("SELECT 1", &[]))
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn a_schema_name_is_a_plain_unreserved_identifier() {
        for good in ["control", "t_0123abc", "a"] {
            assert_eq!(schema_problem(good), None, "{good}");
        }
        for bad in [
            "",
            "1abc",
            "Has",
            "a-b",
            "a b",
            "pg_catalog",
            "information_schema",
            &"x".repeat(64),
        ] {
            assert!(schema_problem(bad).is_some(), "{bad:?}");
        }
    }
}

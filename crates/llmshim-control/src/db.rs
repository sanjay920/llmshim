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
pub(crate) struct PgClient(Option<Box<postgres::Client>>);

impl PgClient {
    pub(crate) fn new(client: postgres::Client) -> Self {
        Self(Some(Box::new(client)))
    }

    pub(crate) fn client(&mut self) -> &mut postgres::Client {
        self.0.as_mut().expect("a client until it is dropped")
    }
}

impl Drop for PgClient {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            let _ = std::thread::spawn(move || drop(client)).join();
        }
    }
}

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
                let tx = connection
                    .transaction_with_behavior(behavior)
                    .map_err(database)?;
                let result = work(&mut SqliteExec(&tx));
                match result {
                    Ok(value) => {
                        tx.commit().map_err(database)?;
                        Ok(value)
                    }
                    Err(error) => Err(error),
                }
            }
            Backend::Postgres(client) => {
                // The synchronous client drives its own runtime, which a thread inside another
                // (a tokio worker, or a `spawn_blocking` thread of one) may not do: each
                // transaction runs on a thread of its own, scoped to this call.
                std::thread::scope(|scope| {
                    scope
                        .spawn(|| -> Result<T> {
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
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
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

/// Connect to Postgres. TLS (rustls, the web PKI roots, the host verified) unless the URL says
/// `sslmode=disable`.
pub(crate) fn connect_postgres(url: &str) -> Result<postgres::Client> {
    let config: postgres::Config = url.parse().map_err(|_| {
        Error::Invalid("the database URL is not a Postgres connection string".into())
    })?;
    let failed = |_| Error::Database("cannot connect to the database".into());
    if config.get_ssl_mode() == postgres::config::SslMode::Disable {
        return config.connect(postgres::NoTls).map_err(failed);
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| Error::Database("TLS could not be set up".into()))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config
        .connect(tokio_postgres_rustls::MakeRustlsConnect::new(tls))
        .map_err(failed)
}

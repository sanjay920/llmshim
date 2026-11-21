//! The request ledger (design note 0051): one row per provider attempt, pushed by the gateways'
//! `[usage_export]` and read by the UI. Metadata only: no prompt, no completion, no header.
//!
//! Delivery from a gateway is at least once and storage is exactly once: the attempt id is the primary
//! key and a repeated row is counted as a duplicate, not stored twice.

use crate::db::{i, of, oi, ot, t, Param};
use crate::{Error, Result, Store};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The table, for a fresh database.
pub(crate) fn schema(pg: bool) -> String {
    schema_shape(pg, true)
}

/// The columns version 8 added (note 0052, slice 5b): where a captured payload is, how big, and until when. An opaque
/// reference only: the payload is on the gateway's side, sealed under a key this crate never has.
pub(crate) fn payload_columns(pg: bool) -> String {
    let (int, if_not_exists) = if pg {
        ("BIGINT", " IF NOT EXISTS")
    } else {
        ("INTEGER", "")
    };
    format!(
        "ALTER TABLE requests ADD COLUMN{if_not_exists} payload_ref TEXT; \
         ALTER TABLE requests ADD COLUMN{if_not_exists} payload_bytes {int}; \
         ALTER TABLE requests ADD COLUMN{if_not_exists} payload_expires_ms {int};"
    )
}

/// The table as version 6 made it (without the payload columns), for the step that creates it in a migration; the
/// later steps add what came after.
pub(crate) fn schema_v6(pg: bool) -> String {
    schema_shape(pg, false)
}

fn schema_shape(pg: bool, payload: bool) -> String {
    let (int, real) = if pg {
        ("BIGINT", "DOUBLE PRECISION")
    } else {
        ("INTEGER", "REAL")
    };
    let payload_columns = if payload {
        format!(", payload_ref TEXT, payload_bytes {int}, payload_expires_ms {int}")
    } else {
        String::new()
    };
    format!(
        "
CREATE TABLE IF NOT EXISTS requests (
    attempt_id TEXT PRIMARY KEY,
    ts TEXT NOT NULL, ts_ms {int} NOT NULL, day TEXT NOT NULL,
    tenant TEXT, key_name TEXT, request_id TEXT, deployment TEXT,
    provider TEXT NOT NULL, model TEXT NOT NULL, kind TEXT NOT NULL, outcome TEXT NOT NULL,
    http_status {int}, duration_ms {int} NOT NULL,
    input_tokens {int} NOT NULL, output_tokens {int} NOT NULL, reasoning_tokens {int},
    uncached_input_tokens {int}, cache_read_tokens {int} NOT NULL, cache_write_tokens {int} NOT NULL,
    cost_usd {real}, cost_source TEXT, usage_reported {int} NOT NULL, admission TEXT NOT NULL,
    tags TEXT, received_ms {int} NOT NULL{payload_columns}
);
CREATE INDEX IF NOT EXISTS requests_by_time ON requests (ts_ms, attempt_id);
CREATE INDEX IF NOT EXISTS requests_by_tenant ON requests (tenant, ts_ms);
CREATE INDEX IF NOT EXISTS requests_by_key ON requests (key_name, ts_ms);
CREATE INDEX IF NOT EXISTS requests_by_model ON requests (model, ts_ms);
CREATE INDEX IF NOT EXISTS requests_by_request ON requests (request_id);
"
    )
}

/// The most rows one ingest call may carry.
pub const MAX_BATCH: usize = 500;
/// The longest any identifying string may be.
const MAX_TEXT: usize = 256;
const OUTCOMES: [&str; 6] = [
    "completed",
    "http_failure",
    "transport_failure",
    "invalid_response",
    "stream_failure",
    "abandoned",
];

/// A validated row.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRequest {
    structure: Option<Vec<crate::structure::Segment>>,
    /// `(reference, bytes, expires_ms)` of a captured payload.
    payload: Option<(String, i64, i64)>,
    attempt_id: String,
    ts: String,
    ts_ms: i64,
    day: String,
    tenant: Option<String>,
    key_name: Option<String>,
    request_id: Option<String>,
    deployment: Option<String>,
    provider: String,
    model: String,
    kind: String,
    outcome: String,
    http_status: Option<i64>,
    duration_ms: i64,
    input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: Option<i64>,
    uncached_input_tokens: Option<i64>,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_usd: Option<f64>,
    cost_source: Option<String>,
    usage_reported: bool,
    admission: String,
    tags: Option<String>,
}

fn text(
    map: &Map<String, Value>,
    field: &'static str,
    required: bool,
) -> std::result::Result<Option<String>, &'static str> {
    match map.get(field) {
        None | Some(Value::Null) if !required => Ok(None),
        Some(Value::String(s))
            if !s.is_empty() && s.len() <= MAX_TEXT && !s.chars().any(char::is_control) =>
        {
            Ok(Some(s.clone()))
        }
        _ => Err(field),
    }
}

fn count(
    map: &Map<String, Value>,
    field: &'static str,
    required: bool,
) -> std::result::Result<Option<i64>, &'static str> {
    // Up to ten billion: far beyond any real request, and a sum over hundreds of millions of rows still
    // fits an i64 (SQLite raises on overflow, which would turn /summary into a 500).
    match map.get(field) {
        None | Some(Value::Null) if !required => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .filter(|n| *n <= 10_000_000_000)
            .map(|n| Some(n as i64))
            .ok_or(field),
        _ => Err(field),
    }
}

impl NewRequest {
    /// Validate one record as the gateway's usage log writes it. Unknown fields are ignored (a newer
    /// gateway); a field of the wrong type, an out-of-range number or an unknown outcome refuses the
    /// row, naming the field and never its value.
    pub fn from_value(value: &Value) -> std::result::Result<Self, &'static str> {
        let map = value.as_object().ok_or("record")?;
        let attempt_id = text(map, "attempt_id", true)?.ok_or("attempt_id")?;
        if attempt_id.len() > 128 || !attempt_id.bytes().all(|b| (b'!'..=b'~').contains(&b)) {
            return Err("attempt_id");
        }
        let ts = text(map, "ts", true)?.ok_or("ts")?;
        let parsed = chrono::DateTime::parse_from_rfc3339(&ts).map_err(|_| "ts")?;
        let ts_ms = parsed.timestamp_millis();
        let utc = parsed.with_timezone(&chrono::Utc);
        let outcome = text(map, "outcome", true)?.ok_or("outcome")?;
        if !OUTCOMES.contains(&outcome.as_str()) {
            return Err("outcome");
        }
        let kind = text(map, "kind", true)?.ok_or("kind")?;
        if kind != "completion" && kind != "stream" {
            return Err("kind");
        }
        let cost_usd = match map.get("cost_usd") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => n
                .as_f64()
                .filter(|c| c.is_finite() && (0.0..=1e9).contains(c))
                .map(Some)
                .ok_or("cost_usd")?,
            _ => return Err("cost_usd"),
        };
        let cost_source = text(map, "cost_source", false)?;
        if cost_source
            .as_deref()
            .is_some_and(|s| !matches!(s, "provider" | "provider_floor" | "catalog"))
        {
            return Err("cost_source");
        }
        let tags = match map.get("tags") {
            None | Some(Value::Null) => None,
            Some(Value::Object(tags))
                if tags.len() <= 16
                    && tags.iter().all(|(k, v)| {
                        k.len() <= 64 && v.as_str().is_some_and(|v| v.len() <= 128)
                    }) =>
            {
                Some(Value::Object(tags.clone()).to_string())
            }
            _ => return Err("tags"),
        };
        Ok(Self {
            attempt_id,
            day: utc.format("%Y-%m-%d").to_string(),
            ts,
            ts_ms,
            tenant: text(map, "tenant", false)?,
            key_name: text(map, "key", false)?,
            request_id: text(map, "request_id", false)?,
            deployment: text(map, "deployment", false)?,
            provider: text(map, "provider", true)?.ok_or("provider")?,
            model: text(map, "model", true)?.ok_or("model")?,
            kind,
            outcome,
            http_status: count(map, "http_status", false)?,
            duration_ms: count(map, "duration_ms", true)?.ok_or("duration_ms")?,
            input_tokens: count(map, "input_tokens", true)?.ok_or("input_tokens")?,
            output_tokens: count(map, "output_tokens", true)?.ok_or("output_tokens")?,
            reasoning_tokens: count(map, "reasoning_tokens", false)?,
            uncached_input_tokens: count(map, "uncached_input_tokens", false)?,
            cache_read_tokens: count(map, "cache_read_tokens", true)?.ok_or("cache_read_tokens")?,
            cache_write_tokens: count(map, "cache_write_tokens", true)?
                .ok_or("cache_write_tokens")?,
            cost_usd,
            cost_source,
            usage_reported: map
                .get("usage_reported")
                .and_then(Value::as_bool)
                .ok_or("usage_reported")?,
            admission: text(map, "admission", true)?.ok_or("admission")?,
            tags,
            // For the cache debugger (note 0052): kept only for a request that names its client, since the
            // client is what pairs a request with the one before it. A field that is not what a gateway sends
            // is dropped, never a reason to lose the row's spend.
            payload: parse_payload(map.get("payload")),
            structure: crate::structure::parse(map.get("structure")).filter(|_| {
                map.get("key")
                    .is_some_and(|k| k.as_str().is_some_and(|k| !k.is_empty()))
            }),
        })
    }
}

/// A record's `payload` field: an opaque reference, its size and its expiry; `None` when it is not what a gateway sends
/// (the row is kept, the reference is not). The reference is never looked into: it is only handed back to a gateway.
fn parse_payload(value: Option<&Value>) -> Option<(String, i64, i64)> {
    let object = value?.as_object()?;
    let reference = object.get("reference")?.as_str()?;
    let bytes = object.get("bytes")?.as_u64().filter(|n| *n <= 16 << 20)?;
    let expires = chrono::DateTime::parse_from_rfc3339(object.get("expires_at")?.as_str()?).ok()?;
    let valid = (1..=2048).contains(&reference.len())
        && reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    valid.then(|| {
        (
            reference.to_owned(),
            bytes as i64,
            expires.timestamp_millis(),
        )
    })
}

/// What one ingest call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct Ingested {
    pub accepted: u64,
    /// Rows whose attempt id was already stored (a retried batch).
    pub duplicate: u64,
    /// Rows refused as malformed (counted, never stored, their content not echoed).
    pub rejected: u64,
}

/// The filters of a request query. Every field narrows.
#[derive(Debug, Clone, Default)]
pub struct RequestQuery {
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub tenant: Option<String>,
    pub key: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub outcome: Option<String>,
    pub request_id: Option<String>,
    pub min_cost_usd: Option<f64>,
    /// `(ts_ms, attempt_id)` of the last row of the previous page.
    pub after: Option<(i64, String)>,
    pub limit: u32,
}

/// One stored row, as the API returns it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RequestRow {
    pub attempt_id: String,
    /// A captured payload, if the client opted in (note 0052, slice 5b): its size and when it expires. The
    /// reference is not here and is never sent to a browser.
    pub payload: Option<PayloadInfo>,
    pub ts: String,
    pub tenant: Option<String>,
    pub key: Option<String>,
    pub request_id: Option<String>,
    pub deployment: Option<String>,
    pub provider: String,
    pub model: String,
    pub kind: String,
    pub outcome: String,
    pub http_status: Option<i64>,
    pub duration_ms: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: Option<i64>,
    pub uncached_input_tokens: Option<i64>,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Cache reads as a share of the whole prompt (uncached, cache read and cache write), 0 to 100;
    /// `None` when the provider did not report the uncached part, which is not zero.
    pub cache_hit_pct: Option<f64>,
    pub cost_usd: Option<f64>,
    pub cost_source: Option<String>,
    pub usage_reported: bool,
    pub admission: String,
    pub tags: Option<Value>,
}

/// That a payload was captured for a request, and until when it can be read.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PayloadInfo {
    pub bytes: i64,
    pub expires_at: String,
    /// Past its expiry: the gateway has deleted it or is about to.
    pub expired: bool,
}

/// A page of rows and where the next one starts.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RequestPage {
    pub requests: Vec<RequestRow>,
    /// Opaque; pass it back as `cursor`. `None` on the last page.
    pub next_cursor: Option<String>,
}

pub(crate) fn encode_cursor(ts_ms: i64, attempt_id: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{ts_ms}\n{attempt_id}"))
}

pub fn decode_cursor(cursor: &str) -> Option<(i64, String)> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (ts, id) = text.split_once('\n')?;
    (id.len() <= 128).then_some((ts.parse().ok()?, id.to_owned()))
}

/// How a summary is grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// One bucket per UTC hour, named by the whole hours since the epoch (`ts_ms / 3600000`): the page
    /// turns it into a time, so no zone is chosen here.
    Hour,
    Day,
    Tenant,
    Key,
    Model,
    Provider,
}

impl Group {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "hour" => Self::Hour,
            "day" => Self::Day,
            "tenant" => Self::Tenant,
            "key" => Self::Key,
            "model" => Self::Model,
            "provider" => Self::Provider,
            _ => return None,
        })
    }

    fn column(self) -> &'static str {
        match self {
            Self::Hour => "CAST(ts_ms / 3600000 AS TEXT)",
            Self::Day => "day",
            Self::Tenant => "tenant",
            Self::Key => "key_name",
            Self::Model => "model",
            Self::Provider => "provider",
        }
    }
}

/// Totals over a set of rows.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct Totals {
    /// Provider attempts (a retry or a fallback is its own attempt).
    pub attempts: i64,
    pub completed: i64,
    pub failed: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Prompt tokens of the rows that reported their uncached part: the denominator of `cache_hit_pct`.
    pub prompt_tokens_known: i64,
    pub cache_hit_pct: Option<f64>,
    /// The sum of the costs that are known. Rows with no cost are counted in `cost_unknown_attempts`,
    /// never added as zero: a sum that must be exact treats them as a gap.
    pub cost_usd: f64,
    pub cost_unknown_attempts: i64,
    /// Attempts whose cost is an estimate from a price list or a lower bound, not the provider's bill.
    pub cost_estimated_attempts: i64,
    pub avg_duration_ms: Option<f64>,
    pub max_duration_ms: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Bucket {
    /// The day, tenant, key, model or provider; `null` for rows with none.
    pub group: Option<String>,
    #[serde(flatten)]
    pub totals: Totals,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Summary {
    pub totals: Totals,
    pub groups: Vec<Bucket>,
}

fn row_to_totals(row: &crate::db::Row, at: usize) -> Result<Totals> {
    let n = |k: usize| row.opt_int(at + k).map(|v| v.unwrap_or(0));
    let cache_read = n(4)?;
    let known = n(7)?;
    let costed = row.opt_real(at + 8)?.unwrap_or(0.0);
    Ok(Totals {
        attempts: n(0)?,
        completed: n(1)?,
        failed: n(2)?,
        input_tokens: n(3)?,
        output_tokens: n(9)?,
        cache_read_tokens: cache_read,
        cache_write_tokens: n(5)?,
        prompt_tokens_known: known,
        // The share of the known prompt that was read from cache (the known part's cache reads).
        cache_hit_pct: (known > 0).then(|| {
            let known_reads = row.opt_int(at + 6).ok().flatten().unwrap_or(0);
            (known_reads as f64) * 100.0 / (known as f64)
        }),
        cost_usd: costed,
        cost_unknown_attempts: n(10)?,
        cost_estimated_attempts: n(11)?,
        avg_duration_ms: row.opt_real(at + 12)?,
        max_duration_ms: n(13)?,
    })
}

/// The aggregate columns, in the order `row_to_totals` reads them.
///
/// Every aggregate is cast to the type the store reads: Postgres sums an integer column as a
/// `NUMERIC` and averages it as one, SQLite does not, and a cast says the same thing in both.
const AGGREGATES: &str = "COUNT(*), \
    CAST(SUM(CASE WHEN outcome = 'completed' THEN 1 ELSE 0 END) AS BIGINT), \
    CAST(SUM(CASE WHEN outcome <> 'completed' THEN 1 ELSE 0 END) AS BIGINT), \
    CAST(SUM(input_tokens) AS BIGINT), CAST(SUM(cache_read_tokens) AS BIGINT), \
    CAST(SUM(cache_write_tokens) AS BIGINT), \
    CAST(SUM(CASE WHEN uncached_input_tokens IS NOT NULL THEN cache_read_tokens ELSE 0 END) AS BIGINT), \
    CAST(SUM(CASE WHEN uncached_input_tokens IS NOT NULL \
        THEN uncached_input_tokens + cache_read_tokens + cache_write_tokens ELSE 0 END) AS BIGINT), \
    SUM(cost_usd), CAST(SUM(output_tokens) AS BIGINT), \
    CAST(SUM(CASE WHEN cost_usd IS NULL THEN 1 ELSE 0 END) AS BIGINT), \
    CAST(SUM(CASE WHEN cost_usd IS NOT NULL AND cost_source IS NOT NULL AND cost_source <> 'provider' \
        THEN 1 ELSE 0 END) AS BIGINT), \
    CAST(AVG(duration_ms) AS DOUBLE PRECISION), MAX(duration_ms)";

impl Store {
    /// Store a batch. Rows are validated one by one (a bad row is counted and skipped, not fatal to the
    /// batch); the stored ones go in one transaction, a repeated attempt id counted as a duplicate.
    pub fn ingest_requests(&mut self, values: &[Value]) -> Result<Ingested> {
        if values.len() > MAX_BATCH {
            return Err(Error::Invalid(format!(
                "a batch holds at most {MAX_BATCH} records"
            )));
        }
        let mut counts = Ingested::default();
        let mut rows = Vec::new();
        for value in values {
            match NewRequest::from_value(value) {
                Ok(row) => rows.push(row),
                Err(_) => counts.rejected += 1,
            }
        }
        let received = chrono::Utc::now().timestamp_millis();
        let inserted = self.backend.transaction(true, |tx| {
            let mut inserted = 0u64;
            for row in &rows {
                let params: Vec<Param> = vec![
                    t(&row.attempt_id), t(&row.ts), i(row.ts_ms), t(&row.day),
                    ot(row.tenant.as_deref()), ot(row.key_name.as_deref()),
                    ot(row.request_id.as_deref()), ot(row.deployment.as_deref()),
                    t(&row.provider), t(&row.model), t(&row.kind), t(&row.outcome),
                    oi(row.http_status), i(row.duration_ms),
                    i(row.input_tokens), i(row.output_tokens), oi(row.reasoning_tokens),
                    oi(row.uncached_input_tokens), i(row.cache_read_tokens), i(row.cache_write_tokens),
                    of(row.cost_usd), ot(row.cost_source.as_deref()),
                    i(i64::from(row.usage_reported)), t(&row.admission),
                    ot(row.tags.as_deref()), i(received),
                    ot(row.payload.as_ref().map(|p| p.0.as_str())), oi(row.payload.as_ref().map(|p| p.1)),
                    oi(row.payload.as_ref().map(|p| p.2)),
                ];
                let stored = tx.exec(
                    "INSERT INTO requests (attempt_id, ts, ts_ms, day, tenant, key_name, request_id, \
                     deployment, provider, model, kind, outcome, http_status, duration_ms, input_tokens, \
                     output_tokens, reasoning_tokens, uncached_input_tokens, cache_read_tokens, \
                     cache_write_tokens, cost_usd, cost_source, usage_reported, admission, tags, received_ms, \
                     payload_ref, payload_bytes, payload_expires_ms) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29) \
                     ON CONFLICT (attempt_id) DO NOTHING",
                    &params,
                )?;
                inserted += stored;
                // The structure goes in with its row, once: a repeated attempt keeps the first.
                if let (1, Some(segments), Some(key)) = (stored, &row.structure, &row.key_name) {
                    tx.exec(
                        "INSERT INTO request_structure (attempt_id, ts_ms, key_name, provider, model, segments) \
                         VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT (attempt_id) DO NOTHING",
                        &[t(&row.attempt_id), i(row.ts_ms), t(key), t(&row.provider), t(&row.model), t(&crate::structure::encode(segments))],
                    )?;
                }
            }
            Ok(inserted)
        })?;
        counts.accepted = inserted;
        counts.duplicate = rows.len() as u64 - inserted;
        Ok(counts)
    }

    /// The filter clause and its parameters, numbered from 1.
    fn request_filter(query: &RequestQuery) -> (String, Vec<Param>) {
        let mut clauses: Vec<String> = Vec::new();
        let mut params: Vec<Param> = Vec::new();
        let mut add = |clause: &str, param: Param| {
            params.push(param);
            clauses.push(clause.replace('?', &format!("?{}", params.len())));
        };
        if let Some(v) = query.from_ms {
            add("ts_ms >= ?", i(v));
        }
        if let Some(v) = query.to_ms {
            add("ts_ms < ?", i(v));
        }
        for (clause, value) in [
            ("tenant = ?", &query.tenant),
            ("key_name = ?", &query.key),
            ("model = ?", &query.model),
            ("provider = ?", &query.provider),
            ("outcome = ?", &query.outcome),
            ("request_id = ?", &query.request_id),
        ] {
            if let Some(value) = value {
                add(clause, t(value));
            }
        }
        if let Some(v) = query.min_cost_usd {
            add("cost_usd >= ?", of(Some(v)));
        }
        let sql = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        (sql, params)
    }

    /// One page, newest first, keyed by `(ts_ms, attempt_id)` so a page boundary never repeats or skips a
    /// row however many arrive meanwhile.
    pub fn query_requests(&mut self, query: &RequestQuery) -> Result<RequestPage> {
        let limit = query.limit.clamp(1, 500);
        let (mut sql, mut params) = Self::request_filter(query);
        if let Some((ts_ms, id)) = &query.after {
            let joiner = if sql.is_empty() { " WHERE" } else { " AND" };
            let (a, b) = (params.len() + 1, params.len() + 2);
            sql.push_str(&format!(
                "{joiner} (ts_ms < ?{a} OR (ts_ms = ?{a} AND attempt_id < ?{b}))"
            ));
            params.push(i(*ts_ms));
            params.push(t(id));
        }
        params.push(i(i64::from(limit) + 1));
        let full = format!(
            "SELECT attempt_id, ts, ts_ms, tenant, key_name, request_id, deployment, provider, model, kind, \
             outcome, http_status, duration_ms, input_tokens, output_tokens, reasoning_tokens, \
             uncached_input_tokens, cache_read_tokens, cache_write_tokens, cost_usd, cost_source, \
             usage_reported, admission, tags, payload_bytes, payload_expires_ms FROM requests{sql} \
             ORDER BY ts_ms DESC, attempt_id DESC LIMIT ?{}",
            params.len()
        );
        self.backend.transaction(false, |tx| {
            let rows = tx.query(&full, &params)?;
            let mut requests = Vec::new();
            let mut cursor_of = Vec::new();
            for row in rows.iter().take(limit as usize) {
                let cache_read = row.int(17)?;
                let write = row.int(18)?;
                let uncached = row.opt_int(16)?;
                cursor_of.push((row.int(2)?, row.text(0)?));
                let payload = match (row.opt_int(24)?, row.opt_int(25)?) {
                    (Some(bytes), Some(expires)) => Some(PayloadInfo {
                        bytes,
                        expires_at: chrono::DateTime::from_timestamp_millis(expires)
                            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                            .unwrap_or_default(),
                        expired: expires < chrono::Utc::now().timestamp_millis(),
                    }),
                    _ => None,
                };
                requests.push(RequestRow {
                    payload,
                    attempt_id: row.text(0)?,
                    ts: row.text(1)?,
                    tenant: row.opt_text(3)?,
                    key: row.opt_text(4)?,
                    request_id: row.opt_text(5)?,
                    deployment: row.opt_text(6)?,
                    provider: row.text(7)?,
                    model: row.text(8)?,
                    kind: row.text(9)?,
                    outcome: row.text(10)?,
                    http_status: row.opt_int(11)?,
                    duration_ms: row.int(12)?,
                    input_tokens: row.int(13)?,
                    output_tokens: row.int(14)?,
                    reasoning_tokens: row.opt_int(15)?,
                    uncached_input_tokens: uncached,
                    cache_read_tokens: cache_read,
                    cache_write_tokens: write,
                    cache_hit_pct: uncached.and_then(|u| {
                        let prompt = u + cache_read + write;
                        (prompt > 0).then(|| cache_read as f64 * 100.0 / prompt as f64)
                    }),
                    cost_usd: row.opt_real(19)?,
                    cost_source: row.opt_text(20)?,
                    usage_reported: row.int(21)? != 0,
                    admission: row.text(22)?,
                    tags: row
                        .opt_text(23)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                });
            }
            let next_cursor = (rows.len() > limit as usize)
                .then(|| cursor_of.last().map(|(ts, id)| encode_cursor(*ts, id)))
                .flatten();
            Ok(RequestPage {
                requests,
                next_cursor,
            })
        })
    }

    /// Totals over the filtered rows, and a breakdown by `group` (at most 500 groups, the largest spend
    /// first).
    pub fn requests_summary(
        &mut self,
        query: &RequestQuery,
        group: Option<Group>,
    ) -> Result<Summary> {
        let (sql, params) = Self::request_filter(query);
        self.backend.transaction(false, |tx| {
            let total = tx.query(&format!("SELECT {AGGREGATES} FROM requests{sql}"), &params)?;
            let totals = match total.first() {
                Some(row) => row_to_totals(row, 0)?,
                None => Totals::default(),
            };
            let mut groups = Vec::new();
            if let Some(group) = group {
                let column = group.column();
                let order = if matches!(group, Group::Day | Group::Hour) {
                    format!("{column} DESC")
                } else {
                    "SUM(cost_usd) IS NULL, SUM(cost_usd) DESC, COUNT(*) DESC".to_owned()
                };
                for row in tx.query(
                    &format!(
                        "SELECT {column}, {AGGREGATES} FROM requests{sql} GROUP BY {column} \
                         ORDER BY {order} LIMIT 500"
                    ),
                    &params,
                )? {
                    groups.push(Bucket {
                        group: row.opt_text(0)?,
                        totals: row_to_totals(&row, 1)?,
                    });
                }
            }
            Ok(Summary { totals, groups })
        })
    }

    /// Delete rows older than `before_ms`; how many went.
    pub fn purge_requests(&mut self, before_ms: i64) -> Result<u64> {
        let gone = self.backend.transaction(true, |tx| {
            tx.exec("DELETE FROM requests WHERE ts_ms < ?1", &[i(before_ms)])
        })?;
        // The structure ages out with its row.
        self.purge_structure(before_ms)?;
        Ok(gone)
    }
}

/// The summary as the API returns it (a flat object).
pub fn summary_json(summary: &Summary) -> Value {
    json!({"totals": summary.totals, "groups": summary.groups})
}

impl Store {
    /// The reference of an attempt's captured payload and when it expires, for the one route that hands it to a
    /// gateway. `None` when there is none.
    pub fn payload_reference(&mut self, attempt_id: &str) -> Result<Option<(String, i64)>> {
        self.backend.transaction(false, |tx| {
            let rows = tx.query(
                "SELECT payload_ref, payload_expires_ms FROM requests WHERE attempt_id = ?1 AND payload_ref IS NOT NULL",
                &[t(attempt_id)],
            )?;
            rows.first().map(|row| Ok((row.text(0)?, row.int(1)?))).transpose()
        })
    }

    /// Record that `actor` asked to read an attempt's payload, in the change history (note 0047): the chain's row is
    /// written before the payload is fetched, so a view with no row cannot exist. Never holds the reference or any
    /// text.
    pub fn record_payload_view(&mut self, attempt_id: &str, actor: &str, via: &str) -> Result<()> {
        crate::actor_check(actor)?;
        // `via` says how the actor was established (`oidc` for a verified person, `break_glass`, `shared_token` for
        // a label the caller gave), so the record is honest about how far its "who" can be trusted.
        self.backend.transaction(true, |tx| {
            crate::history(
                tx,
                actor,
                "payload.viewed",
                attempt_id,
                json!({"attempt_id": attempt_id, "via": via}),
            )
        })
    }
}

/// Distinct values of a column, for the UI's filter lists (bounded).
impl Store {
    pub fn request_facets(
        &mut self,
        from_ms: Option<i64>,
    ) -> Result<BTreeMap<&'static str, Vec<String>>> {
        let mut out = BTreeMap::new();
        for (name, column) in [
            ("tenants", "tenant"),
            ("keys", "key_name"),
            ("models", "model"),
            ("providers", "provider"),
        ] {
            let (clause, params) = match from_ms {
                Some(from) => (" AND ts_ms >= ?1", vec![i(from)]),
                None => ("", vec![]),
            };
            let rows = self.backend.transaction(false, |tx| {
                tx.query(
                    &format!(
                        "SELECT DISTINCT {column} FROM requests WHERE {column} IS NOT NULL{clause} \
                         ORDER BY {column} LIMIT 500"
                    ),
                    &params,
                )?
                .iter()
                .map(|row| row.text(0))
                .collect::<Result<Vec<_>>>()
            })?;
            out.insert(name, rows);
        }
        Ok(out)
    }
}

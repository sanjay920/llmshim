use std::io::{self, Write};
use std::sync::LazyLock;
use std::time::Duration;

use redis::aio::ConnectionManager;
use serde::Serialize;

use super::BusMessage;
use super::QueueProtocol;

pub(super) const DEFAULT_MAX_JOBS: u64 = 10_000;
pub(super) const DEFAULT_MAX_BASE_BYTES: u64 = 512 * 1024 * 1024;
pub(super) const DEFAULT_MAX_TERMINAL_BYTES: u64 = 512 * 1024 * 1024;
pub(super) const DEFAULT_UNARY_TERMINAL_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const DEFAULT_STREAM_TERMINAL_BYTES: u64 = 128 * 1024;
const METADATA_BYTES: u64 = 4 * 1024;
const ACTIVATION_TTL_MS: u64 = 15_000;
const CLEANUP_GRACE_MS: u64 = 60 * 60 * 1000;

fn lifecycle_prefix() -> &'static str {
    "llmshim:gw:lifecycle:v1"
}

pub(super) fn job_prefix() -> String {
    format!("{}:job:", lifecycle_prefix())
}

pub(super) fn counters() -> String {
    counters_key()
}

pub(super) fn reservations() -> String {
    reservations_key()
}

pub(super) fn expiry() -> String {
    expiry_key()
}

pub(super) fn queue_key(protocol: QueueProtocol, provider: &str) -> String {
    format!(
        "{}:q:{}:{provider}",
        lifecycle_prefix(),
        protocol.scope_name()
    )
}

pub(super) fn processing_key(protocol: QueueProtocol, provider: &str) -> String {
    format!(
        "{}:proc:{}:{provider}",
        lifecycle_prefix(),
        protocol.scope_name()
    )
}

pub(super) fn leased_key(protocol: QueueProtocol, provider: &str) -> String {
    format!(
        "{}:leased:{}:{provider}",
        lifecycle_prefix(),
        protocol.scope_name()
    )
}

pub(super) fn owners_key(protocol: QueueProtocol, provider: &str) -> String {
    format!(
        "{}:owners:{}:{provider}",
        lifecycle_prefix(),
        protocol.scope_name()
    )
}

pub(super) fn dlq_key(protocol: QueueProtocol, provider: &str) -> String {
    format!(
        "{}:dlq:{}:{provider}",
        lifecycle_prefix(),
        protocol.scope_name()
    )
}

pub(super) fn response_channel(protocol: QueueProtocol, id: &str) -> String {
    format!("{}:resp:{}:{id}", lifecycle_prefix(), protocol.scope_name())
}

pub(super) fn meta_key(id: &str) -> String {
    format!("{}:job:{id}:meta", lifecycle_prefix())
}

pub(super) fn payload_key(id: &str) -> String {
    format!("{}:job:{id}:payload", lifecycle_prefix())
}

pub(super) fn terminal_key(id: &str) -> String {
    format!("{}:job:{id}:terminal", lifecycle_prefix())
}

fn generation_key() -> String {
    format!("{}:generation", lifecycle_prefix())
}

fn counters_key() -> String {
    format!("{}:counters", lifecycle_prefix())
}

fn reservations_key() -> String {
    format!("{}:reservations", lifecycle_prefix())
}

fn expiry_key() -> String {
    format!("{}:expiry", lifecycle_prefix())
}

const RESERVE_LUA: &str = r#"
    if redis.call('EXISTS', KEYS[5]) == 1 then
        local nonce = redis.call('HGET', KEYS[5], 'nonce')
        if nonce == ARGV[2] then
            return {1, redis.call('HGET', KEYS[5], 'generation')}
        end
        return {-3, false}
    end
    for index = 8, 15 do
        if redis.call('ZCARD', KEYS[index]) > 0 then return {-4, false} end
    end
    local jobs = tonumber(redis.call('HGET', KEYS[2], 'jobs') or '0')
    local base_bytes = tonumber(redis.call('HGET', KEYS[2], 'base_bytes') or '0')
    local charge = string.len(ARGV[3]) + tonumber(ARGV[4])
    if jobs >= tonumber(ARGV[5]) or base_bytes + charge > tonumber(ARGV[6]) then
        return {-1, false}
    end
    redis.call('INCR', KEYS[1])
    local generation = redis.call('GET', KEYS[1])
    local member = ARGV[1] .. ':' .. generation
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    redis.call('HSET', KEYS[5],
        'generation', generation, 'nonce', ARGV[2], 'state', 'pending_origin',
        'protocol', ARGV[7], 'provider', ARGV[8], 'stream', ARGV[9],
        'priority_score', ARGV[10], 'base_charge', charge, 'cancel_requested', '0')
    redis.call('SET', KEYS[6], ARGV[3])
    redis.call('HSET', KEYS[3], member, charge)
    redis.call('HINCRBY', KEYS[2], 'jobs', 1)
    redis.call('HINCRBY', KEYS[2], 'base_bytes', charge)
    redis.call('ZADD', KEYS[4], now_ms + tonumber(ARGV[11]), member)
    redis.call('PEXPIRE', KEYS[5], tonumber(ARGV[11]) + tonumber(ARGV[12]))
    redis.call('PEXPIRE', KEYS[6], tonumber(ARGV[11]) + tonumber(ARGV[12]))
    return {2, generation}
"#;

const ACTIVATE_LUA: &str = r#"
    if redis.call('HGET', KEYS[1], 'generation') ~= ARGV[1]
        or redis.call('HGET', KEYS[1], 'state') ~= 'pending_origin' then return 0 end
    local waiting = 0
    for index = 3, 8 do waiting = waiting + redis.call('ZCARD', KEYS[index]) end
    if waiting >= tonumber(ARGV[6]) then return -1 end
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    redis.call('HSET', KEYS[1], 'state', 'waiting')
    redis.call('ZADD', KEYS[3], ARGV[3], ARGV[2])
    redis.call('ZADD', KEYS[9], now_ms + tonumber(ARGV[4]), ARGV[2])
    redis.call('PEXPIRE', KEYS[1], tonumber(ARGV[4]) + tonumber(ARGV[5]))
    redis.call('PEXPIRE', KEYS[2], tonumber(ARGV[4]) + tonumber(ARGV[5]))
    return 1
"#;

const CANCEL_LUA: &str = r#"
    if redis.call('HGET', KEYS[1], 'generation') ~= ARGV[1] then return 0 end
    local state = redis.call('HGET', KEYS[1], 'state')
    if state == 'processing' then
        redis.call('HSET', KEYS[1], 'cancel_requested', '1')
        redis.call('PUBLISH', ARGV[4], ARGV[5])
        return 2
    end
    if state == 'pending_origin' or state == 'waiting' then
        redis.call('ZREM', KEYS[3], ARGV[2])
        redis.call('SET', KEYS[2], ARGV[5])
        redis.call('HSET', KEYS[1], 'state', 'terminal', 'terminal_charge', '0')
        local redis_time = redis.call('TIME')
        local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
        redis.call('ZADD', KEYS[4], now_ms + tonumber(ARGV[3]), ARGV[2])
        redis.call('PUBLISH', ARGV[4], ARGV[5])
        return 1
    end
    return 3
"#;

const CLEANUP_LUA: &str = r#"
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local expired = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', now_ms, 'LIMIT', 0, ARGV[1])
    local cleaned = 0
    for _, member in ipairs(expired) do
        local id = string.match(member, '^(.*):([^:]*)$')
        if id then
            local meta = ARGV[2] .. ':job:' .. id .. ':meta'
            local state = redis.call('HGET', meta, 'state')
            if state == 'processing' then
                redis.call('ZADD', KEYS[1], now_ms + 60000, member)
            else
                local scope = redis.call('HGET', meta, 'protocol')
                local provider = redis.call('HGET', meta, 'provider')
                if scope and provider then
                    redis.call('ZREM', ARGV[2] .. ':q:' .. scope .. ':' .. provider, member)
                    redis.call('ZREM', ARGV[2] .. ':dlq:' .. scope .. ':' .. provider, member)
                end
                local base_charge = redis.call('HGET', KEYS[2], member)
                if base_charge and redis.call('HDEL', KEYS[2], member) == 1 then
                    redis.call('HINCRBY', KEYS[3], 'jobs', -1)
                    redis.call('HINCRBY', KEYS[3], 'base_bytes', -tonumber(base_charge))
                    local terminal_charge = tonumber(redis.call('HGET', meta, 'terminal_charge') or '0')
                    if terminal_charge > 0 then
                        redis.call('HINCRBY', KEYS[3], 'terminal_bytes', -terminal_charge)
                    end
                    if state == 'dlq' then
                        redis.call('HINCRBY', KEYS[3], 'dlq_bytes', -tonumber(base_charge))
                    end
                end
                redis.call('DEL', meta, ARGV[2] .. ':job:' .. id .. ':payload',
                    ARGV[2] .. ':job:' .. id .. ':terminal')
                redis.call('ZREM', KEYS[1], member)
                cleaned = cleaned + 1
            end
        else
            redis.call('ZREM', KEYS[1], member)
        end
    end
    return cleaned
"#;

const CACHE_PUT_LUA: &str = r#"
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local expired = redis.call('ZRANGEBYSCORE', KEYS[2], '-inf', now_ms, 'LIMIT', 0, 16)
    for _, key in ipairs(expired) do
        local old = redis.call('HGET', KEYS[1], key)
        if old then redis.call('HINCRBY', KEYS[3], 'bytes', -string.len(old)) end
        if redis.call('HDEL', KEYS[1], key) == 1 then redis.call('HINCRBY', KEYS[3], 'count', -1) end
        redis.call('ZREM', KEYS[2], key)
    end
    if string.len(ARGV[2]) > tonumber(ARGV[4]) then return 0 end
    local previous = redis.call('HGET', KEYS[1], ARGV[1])
    local count = tonumber(redis.call('HGET', KEYS[3], 'count') or '0')
    local bytes = tonumber(redis.call('HGET', KEYS[3], 'bytes') or '0')
    local next_bytes = bytes - (previous and string.len(previous) or 0) + string.len(ARGV[2])
    if (not previous and count >= tonumber(ARGV[5])) or next_bytes > tonumber(ARGV[6]) then return 0 end
    redis.call('HSET', KEYS[1], ARGV[1], ARGV[2])
    redis.call('ZADD', KEYS[2], now_ms + tonumber(ARGV[3]), ARGV[1])
    if not previous then redis.call('HINCRBY', KEYS[3], 'count', 1) end
    redis.call('HSET', KEYS[3], 'bytes', next_bytes)
    return 1
"#;

const CACHE_GET_LUA: &str = r#"
    local value = redis.call('HGET', KEYS[1], ARGV[1])
    if not value then return false end
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local expiry = tonumber(redis.call('ZSCORE', KEYS[2], ARGV[1]) or '0')
    if expiry <= now_ms then
        redis.call('HDEL', KEYS[1], ARGV[1])
        redis.call('ZREM', KEYS[2], ARGV[1])
        redis.call('HINCRBY', KEYS[3], 'count', -1)
        redis.call('HINCRBY', KEYS[3], 'bytes', -string.len(value))
        return false
    end
    return value
"#;

const EXTEND_TERMINAL_LUA: &str = r#"
    if redis.call('HGET', KEYS[1], 'generation') ~= ARGV[1]
        or redis.call('HGET', KEYS[1], 'state') ~= 'terminal' then return 0 end
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    redis.call('ZADD', KEYS[4], 'GT', now_ms + tonumber(ARGV[2]), ARGV[3])
    local hard_ttl = tonumber(ARGV[2]) + tonumber(ARGV[4])
    for index = 1, 3 do
        local current_ttl = redis.call('PTTL', KEYS[index])
        if current_ttl < hard_ttl then redis.call('PEXPIRE', KEYS[index], hard_ttl) end
    end
    return 1
"#;

static RESERVE: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(RESERVE_LUA));
static ACTIVATE: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(ACTIVATE_LUA));
static CANCEL: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(CANCEL_LUA));
static CLEANUP: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(CLEANUP_LUA));
static CACHE_PUT: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(CACHE_PUT_LUA));
static CACHE_GET: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(CACHE_GET_LUA));
static EXTEND_TERMINAL: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(EXTEND_TERMINAL_LUA));

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub max_jobs: u64,
    pub max_base_bytes: u64,
    pub max_terminal_bytes: u64,
    pub unary_terminal_bytes: u64,
    pub stream_terminal_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_jobs: DEFAULT_MAX_JOBS,
            max_base_bytes: DEFAULT_MAX_BASE_BYTES,
            max_terminal_bytes: DEFAULT_MAX_TERMINAL_BYTES,
            unary_terminal_bytes: DEFAULT_UNARY_TERMINAL_BYTES,
            stream_terminal_bytes: DEFAULT_STREAM_TERMINAL_BYTES,
        }
    }
}

impl Limits {
    pub(super) fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            max_jobs: positive_env("LLMSHIM_GATEWAY_RETAINED_JOBS", defaults.max_jobs),
            max_base_bytes: positive_env(
                "LLMSHIM_GATEWAY_RETAINED_BASE_BYTES",
                defaults.max_base_bytes,
            ),
            max_terminal_bytes: positive_env(
                "LLMSHIM_GATEWAY_RETAINED_TERMINAL_BYTES",
                defaults.max_terminal_bytes,
            ),
            unary_terminal_bytes: positive_env(
                "LLMSHIM_GATEWAY_UNARY_TERMINAL_BYTES",
                defaults.unary_terminal_bytes,
            ),
            stream_terminal_bytes: positive_env(
                "LLMSHIM_GATEWAY_STREAM_TERMINAL_BYTES",
                defaults.stream_terminal_bytes,
            ),
        }
    }
}

fn positive_env(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

pub(super) struct ReservedJob {
    pub generation: String,
    pub member: String,
}

#[derive(Debug)]
pub(super) enum ReserveFailure {
    Capacity,
    Collision,
    Migration,
    Redis,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn reserve(
    connection: &mut ConnectionManager,
    protocol: QueueProtocol,
    id: &str,
    nonce: &str,
    payload: &str,
    provider: &str,
    stream: bool,
    priority_score: f64,
    limits: Limits,
    old_keys: [&str; 8],
) -> Result<ReservedJob, ReserveFailure> {
    let _ = cleanup_expired(connection, 16).await;
    let meta = meta_key(id);
    let payload_storage = payload_key(id);
    let mut invocation = RESERVE.key(generation_key());
    invocation
        .key(counters_key())
        .key(reservations_key())
        .key(expiry_key())
        .key(&meta)
        .key(&payload_storage)
        .key(terminal_key(id));
    for key in old_keys {
        invocation.key(key);
    }
    invocation
        .arg(id)
        .arg(nonce)
        .arg(payload)
        .arg(METADATA_BYTES)
        .arg(limits.max_jobs)
        .arg(limits.max_base_bytes)
        .arg(protocol.scope_name())
        .arg(provider)
        .arg(u8::from(stream))
        .arg(priority_score)
        .arg(ACTIVATION_TTL_MS)
        .arg(CLEANUP_GRACE_MS);
    let result: (i64, Option<String>) = invocation
        .invoke_async(connection)
        .await
        .map_err(|_| ReserveFailure::Redis)?;
    match result {
        (1 | 2, Some(generation)) => Ok(ReservedJob {
            member: format!("{id}:{generation}"),
            generation,
        }),
        (-1, _) => Err(ReserveFailure::Capacity),
        (-3, _) => Err(ReserveFailure::Collision),
        (-4, _) => Err(ReserveFailure::Migration),
        _ => Err(ReserveFailure::Redis),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn activate(
    connection: &mut ConnectionManager,
    protocol: QueueProtocol,
    provider: &str,
    id: &str,
    reserved: &ReservedJob,
    priority_score: f64,
    waiting_ttl: Duration,
    maximum_waiting: usize,
    capacity_queue_keys: [&str; 5],
) -> redis::RedisResult<bool> {
    let mut invocation = ACTIVATE.key(meta_key(id));
    invocation
        .key(payload_key(id))
        .key(queue_key(protocol, provider))
        .key(capacity_queue_keys[0])
        .key(capacity_queue_keys[1])
        .key(capacity_queue_keys[2])
        .key(capacity_queue_keys[3])
        .key(capacity_queue_keys[4])
        .key(expiry_key());
    invocation
        .arg(&reserved.generation)
        .arg(&reserved.member)
        .arg(priority_score)
        .arg(duration_millis(waiting_ttl))
        .arg(CLEANUP_GRACE_MS)
        .arg(maximum_waiting);
    let activated: i64 = invocation.invoke_async(connection).await?;
    Ok(activated == 1)
}

pub(super) async fn read_terminal(
    connection: &mut ConnectionManager,
    id: &str,
    expected_generation: &str,
) -> redis::RedisResult<Option<BusMessage>> {
    use redis::AsyncCommands;
    let actual_generation: Option<String> = connection.hget(meta_key(id), "generation").await?;
    if actual_generation.as_deref() != Some(expected_generation) {
        return Ok(None);
    }
    let serialized: Option<Vec<u8>> = connection.get(terminal_key(id)).await?;
    Ok(serialized.and_then(|bytes| serde_json::from_slice(&bytes).ok()))
}

pub(super) async fn cancel(
    connection: &mut ConnectionManager,
    protocol: QueueProtocol,
    provider: &str,
    id: &str,
    generation: &str,
    member: &str,
    terminal_ttl: Duration,
) -> redis::RedisResult<i64> {
    let envelope = serde_json::to_vec(&BusMessage::Error("distributed origin canceled".into()))
        .expect("fixed cancellation envelope serializes");
    CANCEL
        .key(meta_key(id))
        .key(terminal_key(id))
        .key(queue_key(protocol, provider))
        .key(expiry_key())
        .arg(generation)
        .arg(member)
        .arg(duration_millis(terminal_ttl))
        .arg(response_channel(protocol, id))
        .arg(envelope)
        .invoke_async(connection)
        .await
}

pub(super) async fn cleanup_expired(
    connection: &mut ConnectionManager,
    maximum_records: usize,
) -> redis::RedisResult<usize> {
    CLEANUP
        .key(expiry_key())
        .key(reservations_key())
        .key(counters_key())
        .arg(maximum_records)
        .arg(lifecycle_prefix())
        .invoke_async(connection)
        .await
}

fn generic_cache_keys() -> (String, String, String) {
    (
        format!("{}:generic-cache:v2:values", lifecycle_prefix()),
        format!("{}:generic-cache:v2:expiry", lifecycle_prefix()),
        format!("{}:generic-cache:v2:counters", lifecycle_prefix()),
    )
}

fn scoped_pointer_keys() -> (String, String, String) {
    (
        format!("{}:scoped-idempotency:v1:values", lifecycle_prefix()),
        format!("{}:scoped-idempotency:v1:expiry", lifecycle_prefix()),
        format!("{}:scoped-idempotency:v1:counters", lifecycle_prefix()),
    )
}

pub(super) async fn generic_cache_put(
    connection: &mut ConnectionManager,
    key: &str,
    serialized_value: &[u8],
    ttl_secs: u64,
) -> redis::RedisResult<bool> {
    let (values, expiry, counters) = generic_cache_keys();
    let stored: i64 = CACHE_PUT
        .key(values)
        .key(expiry)
        .key(counters)
        .arg(key)
        .arg(serialized_value)
        .arg(ttl_secs.clamp(1, 86_400).saturating_mul(1_000))
        .arg(1024 * 1024_u64)
        .arg(100_000_u64)
        .arg(64 * 1024 * 1024_u64)
        .invoke_async(connection)
        .await?;
    Ok(stored == 1)
}

pub(super) async fn generic_cache_get(
    connection: &mut ConnectionManager,
    key: &str,
) -> redis::RedisResult<Option<Vec<u8>>> {
    let (values, expiry, counters) = generic_cache_keys();
    CACHE_GET
        .key(values)
        .key(expiry)
        .key(counters)
        .arg(key)
        .invoke_async(connection)
        .await
}

pub(super) async fn scoped_pointer_put(
    connection: &mut ConnectionManager,
    key: &str,
    pointer: &[u8],
    ttl_secs: u64,
) -> redis::RedisResult<bool> {
    let (values, expiry, counters) = scoped_pointer_keys();
    let stored: i64 = CACHE_PUT
        .key(values)
        .key(expiry)
        .key(counters)
        .arg(key)
        .arg(pointer)
        .arg(ttl_secs.clamp(1, 86_400).saturating_mul(1_000))
        .arg(4 * 1024_u64)
        .arg(100_000_u64)
        .arg(64 * 1024 * 1024_u64)
        .invoke_async(connection)
        .await?;
    Ok(stored == 1)
}

pub(super) async fn scoped_pointer_get(
    connection: &mut ConnectionManager,
    key: &str,
) -> redis::RedisResult<Option<Vec<u8>>> {
    let (values, expiry, counters) = scoped_pointer_keys();
    CACHE_GET
        .key(values)
        .key(expiry)
        .key(counters)
        .arg(key)
        .invoke_async(connection)
        .await
}

pub(super) async fn extend_terminal_retention(
    connection: &mut ConnectionManager,
    id: &str,
    generation: &str,
    ttl_secs: u64,
) -> redis::RedisResult<bool> {
    let member = format!("{id}:{generation}");
    let extended: i64 = EXTEND_TERMINAL
        .key(meta_key(id))
        .key(terminal_key(id))
        .key(payload_key(id))
        .key(expiry_key())
        .arg(generation)
        .arg(ttl_secs.saturating_mul(1_000))
        .arg(member)
        .arg(CLEANUP_GRACE_MS)
        .invoke_async(connection)
        .await?;
    Ok(extended == 1)
}

pub(super) fn serialize_bounded<T: Serialize>(
    value: &T,
    maximum_bytes: usize,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut writer = BoundedWriter {
        bytes: Vec::with_capacity(maximum_bytes.min(64 * 1024)),
        maximum_bytes,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}

struct BoundedWriter {
    bytes: Vec<u8>,
    maximum_bytes: usize,
}

impl Write for BoundedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > self.maximum_bytes.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "serialized terminal exceeds distributed limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::AsyncCommands;
    use serde_json::json;

    #[test]
    fn bounded_serializer_stops_before_escaped_output_exceeds_limit() {
        let value = json!({"text":"\u{0000}\u{0000}"});
        assert!(serialize_bounded(&value, 32).is_ok());
        assert!(serialize_bounded(&value, 16).is_err());
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_generation_cancel_capacity_and_cleanup_are_bounded() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let client = redis::Client::open(redis_url).unwrap();
        let mut connection = ConnectionManager::new(client).await.unwrap();
        let id = format!("lifecycle-{}", uuid::Uuid::new_v4().simple());
        let old_provider = format!("lifecycle-old-{}", uuid::Uuid::new_v4().simple());
        let old_keys = [
            format!("old:1:q:{old_provider}"),
            format!("old:1:p:{old_provider}"),
            format!("old:2:q:{old_provider}"),
            format!("old:2:p:{old_provider}"),
            format!("old:3:q:{old_provider}"),
            format!("old:3:p:{old_provider}"),
            format!("old:4:q:{old_provider}"),
            format!("old:4:p:{old_provider}"),
        ];
        let capacity_keys = [
            old_keys[0].as_str(),
            old_keys[1].as_str(),
            old_keys[2].as_str(),
            old_keys[3].as_str(),
            old_keys[4].as_str(),
        ];
        for key in [
            generation_key(),
            counters_key(),
            reservations_key(),
            expiry_key(),
            meta_key(&id),
            payload_key(&id),
            terminal_key(&id),
            queue_key(QueueProtocol::LegacyUnscoped, &old_provider),
        ] {
            let _: i64 = connection.del(key).await.unwrap();
        }
        let _: () = connection
            .set(generation_key(), "9007199254740992")
            .await
            .unwrap();
        let limits = Limits {
            max_jobs: 1,
            max_base_bytes: 8 * 1024,
            max_terminal_bytes: 1024,
            unary_terminal_bytes: 512,
            stream_terminal_bytes: 64,
        };
        let first = reserve(
            &mut connection,
            QueueProtocol::LegacyUnscoped,
            &id,
            "same-nonce",
            r#"{"payload":"immutable"}"#,
            &old_provider,
            false,
            1.0,
            limits,
            old_keys.each_ref().map(|key| key.as_str()),
        )
        .await
        .unwrap();
        assert_eq!(first.generation, "9007199254740993");
        let repeated = reserve(
            &mut connection,
            QueueProtocol::LegacyUnscoped,
            &id,
            "same-nonce",
            r#"{"payload":"immutable"}"#,
            &old_provider,
            false,
            1.0,
            limits,
            old_keys.each_ref().map(|key| key.as_str()),
        )
        .await
        .unwrap();
        assert_eq!(repeated.generation, first.generation);
        assert!(matches!(
            reserve(
                &mut connection,
                QueueProtocol::LegacyUnscoped,
                "capacity-second",
                "other-nonce",
                "{}",
                &old_provider,
                false,
                2.0,
                limits,
                old_keys.each_ref().map(|key| key.as_str()),
            )
            .await,
            Err(ReserveFailure::Capacity)
        ));
        assert_eq!(
            cancel(
                &mut connection,
                QueueProtocol::LegacyUnscoped,
                &old_provider,
                &id,
                &first.generation,
                &first.member,
                Duration::from_secs(60),
            )
            .await
            .unwrap(),
            1
        );
        assert!(!activate(
            &mut connection,
            QueueProtocol::LegacyUnscoped,
            &old_provider,
            &id,
            &first,
            1.0,
            Duration::from_secs(60),
            10,
            capacity_keys,
        )
        .await
        .unwrap());

        let _: () = connection
            .zadd(expiry_key(), &first.member, 0)
            .await
            .unwrap();
        assert_eq!(cleanup_expired(&mut connection, 1).await.unwrap(), 1);
        let replacement = reserve(
            &mut connection,
            QueueProtocol::LegacyUnscoped,
            &id,
            "same-nonce",
            r#"{"payload":"replacement"}"#,
            &old_provider,
            false,
            1.0,
            limits,
            old_keys.each_ref().map(|key| key.as_str()),
        )
        .await
        .unwrap();
        assert_eq!(replacement.generation, "9007199254740994");
        assert!(!activate(
            &mut connection,
            QueueProtocol::LegacyUnscoped,
            &old_provider,
            &id,
            &first,
            1.0,
            Duration::from_secs(60),
            10,
            capacity_keys,
        )
        .await
        .unwrap());
        assert!(activate(
            &mut connection,
            QueueProtocol::LegacyUnscoped,
            &old_provider,
            &id,
            &replacement,
            1.0,
            Duration::from_secs(60),
            10,
            capacity_keys,
        )
        .await
        .unwrap());
        let payload: String = connection.get(payload_key(&id)).await.unwrap();
        assert_eq!(payload, r#"{"payload":"replacement"}"#);
    }
}

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
const MIN_TERMINAL_BYTES: u64 = 4 * 1024;

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

pub(super) fn terminal_reservations() -> String {
    terminal_reservations_key()
}

pub(super) fn dlq_reservations() -> String {
    dlq_reservations_key()
}

pub(super) fn reservation_states() -> String {
    reservation_states_key()
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

fn terminal_reservations_key() -> String {
    format!("{}:reservations:terminal", lifecycle_prefix())
}

fn dlq_reservations_key() -> String {
    format!("{}:reservations:dlq", lifecycle_prefix())
}

fn reservation_states_key() -> String {
    format!("{}:reservations:state", lifecycle_prefix())
}

fn reservation_scopes_key() -> String {
    format!("{}:reservations:scope", lifecycle_prefix())
}

fn reservation_providers_key() -> String {
    format!("{}:reservations:provider", lifecycle_prefix())
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
    redis.call('HSET', KEYS[16], member, 0)
    redis.call('HSET', KEYS[17], member, 0)
    redis.call('HSET', KEYS[18], member, 'pending_origin')
    redis.call('HSET', KEYS[19], member, ARGV[7])
    redis.call('HSET', KEYS[20], member, ARGV[8])
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
    redis.call('HSET', KEYS[10], ARGV[2], 'waiting')
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
        redis.call('HSET', KEYS[5], ARGV[2], 'terminal')
        local redis_time = redis.call('TIME')
        local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
        redis.call('ZADD', KEYS[4], now_ms + tonumber(ARGV[3]), ARGV[2])
        local hard_ttl = tonumber(ARGV[3]) + tonumber(ARGV[6])
        redis.call('PEXPIRE', KEYS[1], hard_ttl)
        redis.call('PEXPIRE', KEYS[2], hard_ttl)
        redis.call('PEXPIRE', KEYS[6], hard_ttl)
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
        local base_raw = redis.call('HGET', KEYS[2], member)
        local terminal_raw = redis.call('HGET', KEYS[4], member)
        local dlq_raw = redis.call('HGET', KEYS[5], member)
        local state = redis.call('HGET', KEYS[6], member)
        local scope = redis.call('HGET', KEYS[7], member)
        local provider = redis.call('HGET', KEYS[8], member)
        local base_charge = tonumber(base_raw)
        local terminal_charge = tonumber(terminal_raw)
        local dlq_charge = tonumber(dlq_raw)
        if not id or not base_charge or not terminal_charge or not dlq_charge
            or not state or not scope or not provider then
            redis.call('ZADD', KEYS[1], now_ms + 3600000, member)
        else
            local meta = ARGV[2] .. ':job:' .. id .. ':meta'
            if state == 'processing' and redis.call('EXISTS', meta) == 1 then
                redis.call('ZADD', KEYS[1], now_ms + 60000, member)
            elseif redis.call('HDEL', KEYS[2], member) == 1 then
                redis.call('ZREM', ARGV[2] .. ':q:' .. scope .. ':' .. provider, member)
                redis.call('ZREM', ARGV[2] .. ':proc:' .. scope .. ':' .. provider, member)
                redis.call('ZREM', ARGV[2] .. ':dlq:' .. scope .. ':' .. provider, member)
                redis.call('HDEL', ARGV[2] .. ':leased:' .. scope .. ':' .. provider, member)
                redis.call('HDEL', ARGV[2] .. ':owners:' .. scope .. ':' .. provider, member)
                redis.call('HINCRBY', KEYS[3], 'jobs', -1)
                redis.call('HINCRBY', KEYS[3], 'base_bytes', -base_charge)
                if terminal_charge > 0 then
                    redis.call('HINCRBY', KEYS[3], 'terminal_bytes', -terminal_charge)
                end
                if dlq_charge > 0 then
                    redis.call('HINCRBY', KEYS[3], 'dlq_bytes', -dlq_charge)
                end
                for index = 4, 8 do redis.call('HDEL', KEYS[index], member) end
                redis.call('DEL', meta, ARGV[2] .. ':job:' .. id .. ':payload',
                    ARGV[2] .. ':job:' .. id .. ':terminal')
                redis.call('ZREM', KEYS[1], member)
                cleaned = cleaned + 1
            end
        end
    end
    return cleaned
"#;

const CACHE_PUT_LUA: &str = r#"
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local expired = redis.call('ZRANGEBYSCORE', KEYS[2], '-inf', now_ms, 'LIMIT', 0, 16)
    for _, value_key in ipairs(expired) do
        local charge_raw = redis.call('HGET', KEYS[3], value_key)
        local charge = tonumber(charge_raw or '')
        if charge and redis.call('HDEL', KEYS[3], value_key) == 1 then
            redis.call('HINCRBY', KEYS[4], 'bytes', -charge)
            redis.call('HINCRBY', KEYS[4], 'count', -1)
            redis.call('DEL', value_key)
            redis.call('ZREM', KEYS[2], value_key)
        elseif not charge_raw then
            redis.call('ZREM', KEYS[2], value_key)
        else
            redis.call('ZADD', KEYS[2], now_ms + 3600000, value_key)
        end
    end
    if string.len(ARGV[2]) > tonumber(ARGV[4]) then return 0 end
    local previous_charge = tonumber(redis.call('HGET', KEYS[3], ARGV[1]) or '0')
    local count = tonumber(redis.call('HGET', KEYS[4], 'count') or '0')
    local bytes = tonumber(redis.call('HGET', KEYS[4], 'bytes') or '0')
    local next_bytes = bytes - previous_charge + string.len(ARGV[2])
    if (previous_charge == 0 and count >= tonumber(ARGV[5]))
        or next_bytes > tonumber(ARGV[6]) then return 0 end
    redis.call('SET', KEYS[1], ARGV[2], 'PX', ARGV[3])
    redis.call('ZADD', KEYS[2], now_ms + tonumber(ARGV[3]), ARGV[1])
    redis.call('HSET', KEYS[3], ARGV[1], string.len(ARGV[2]))
    if previous_charge == 0 then redis.call('HINCRBY', KEYS[4], 'count', 1) end
    redis.call('HSET', KEYS[4], 'bytes', next_bytes)
    return 1
"#;

const CACHE_GET_LUA: &str = r#"
    local value = redis.call('GET', KEYS[1])
    if not value then
        local charge_raw = redis.call('HGET', KEYS[3], ARGV[1])
        local charge = tonumber(charge_raw or '')
        if charge and redis.call('HDEL', KEYS[3], ARGV[1]) == 1 then
            redis.call('HINCRBY', KEYS[4], 'count', -1)
            redis.call('HINCRBY', KEYS[4], 'bytes', -charge)
            redis.call('ZREM', KEYS[2], ARGV[1])
        elseif not charge_raw then
            redis.call('ZREM', KEYS[2], ARGV[1])
        end
        return false
    end
    return value
"#;

const CACHE_CLEANUP_LUA: &str = r#"
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local expired = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', now_ms, 'LIMIT', 0, ARGV[1])
    local cleaned = 0
    for _, value_key in ipairs(expired) do
        local charge_raw = redis.call('HGET', KEYS[2], value_key)
        local charge = tonumber(charge_raw or '')
        if charge and redis.call('HDEL', KEYS[2], value_key) == 1 then
            redis.call('HINCRBY', KEYS[3], 'count', -1)
            redis.call('HINCRBY', KEYS[3], 'bytes', -charge)
            redis.call('DEL', value_key)
            redis.call('ZREM', KEYS[1], value_key)
            cleaned = cleaned + 1
        elseif not charge_raw then
            redis.call('ZREM', KEYS[1], value_key)
        else
            redis.call('ZADD', KEYS[1], now_ms + 3600000, value_key)
        end
    end
    return cleaned
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

const READ_TERMINAL_LUA: &str = r#"
    if redis.call('HGET', KEYS[1], 'generation') ~= ARGV[1] then return false end
    return redis.call('GET', KEYS[2])
"#;

static RESERVE: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(RESERVE_LUA));
static ACTIVATE: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(ACTIVATE_LUA));
static CANCEL: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(CANCEL_LUA));
static CLEANUP: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(CLEANUP_LUA));
static CACHE_PUT: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(CACHE_PUT_LUA));
static CACHE_GET: LazyLock<redis::Script> = LazyLock::new(|| redis::Script::new(CACHE_GET_LUA));
static CACHE_CLEANUP: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(CACHE_CLEANUP_LUA));
static EXTEND_TERMINAL: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(EXTEND_TERMINAL_LUA));
static READ_TERMINAL: LazyLock<redis::Script> =
    LazyLock::new(|| redis::Script::new(READ_TERMINAL_LUA));

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
            )
            .max(MIN_TERMINAL_BYTES),
            stream_terminal_bytes: positive_env(
                "LLMSHIM_GATEWAY_STREAM_TERMINAL_BYTES",
                defaults.stream_terminal_bytes,
            )
            .max(MIN_TERMINAL_BYTES),
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
        .key(terminal_reservations_key())
        .key(dlq_reservations_key())
        .key(reservation_states_key())
        .key(reservation_scopes_key())
        .key(reservation_providers_key());
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
        .key(expiry_key())
        .key(reservation_states_key());
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
    let serialized: Option<Vec<u8>> = READ_TERMINAL
        .key(meta_key(id))
        .key(terminal_key(id))
        .arg(expected_generation)
        .invoke_async(connection)
        .await?;
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
        .key(reservation_states_key())
        .key(payload_key(id))
        .arg(generation)
        .arg(member)
        .arg(duration_millis(terminal_ttl))
        .arg(response_channel(protocol, id))
        .arg(envelope)
        .arg(CLEANUP_GRACE_MS)
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
        .key(terminal_reservations_key())
        .key(dlq_reservations_key())
        .key(reservation_states_key())
        .key(reservation_scopes_key())
        .key(reservation_providers_key())
        .arg(maximum_records)
        .arg(lifecycle_prefix())
        .invoke_async(connection)
        .await
}

fn generic_cache_keys(key: &str) -> (String, String, String, String) {
    (
        format!("{}:generic-cache:v3:value:{key}", lifecycle_prefix()),
        format!("{}:generic-cache:v3:expiry", lifecycle_prefix()),
        format!("{}:generic-cache:v3:reservations", lifecycle_prefix()),
        format!("{}:generic-cache:v3:counters", lifecycle_prefix()),
    )
}

fn scoped_pointer_keys(key: &str) -> (String, String, String, String) {
    (
        format!("{}:scoped-idempotency:v2:value:{key}", lifecycle_prefix()),
        format!("{}:scoped-idempotency:v2:expiry", lifecycle_prefix()),
        format!("{}:scoped-idempotency:v2:reservations", lifecycle_prefix()),
        format!("{}:scoped-idempotency:v2:counters", lifecycle_prefix()),
    )
}

pub(super) async fn generic_cache_put(
    connection: &mut ConnectionManager,
    key: &str,
    serialized_value: &[u8],
    ttl_secs: u64,
) -> redis::RedisResult<bool> {
    let (value_key, expiry, reservations, counters) = generic_cache_keys(key);
    let stored: i64 = CACHE_PUT
        .key(&value_key)
        .key(expiry)
        .key(reservations)
        .key(counters)
        .arg(&value_key)
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
    let (value_key, expiry, reservations, counters) = generic_cache_keys(key);
    CACHE_GET
        .key(&value_key)
        .key(expiry)
        .key(reservations)
        .key(counters)
        .arg(value_key)
        .invoke_async(connection)
        .await
}

pub(super) async fn scoped_pointer_put(
    connection: &mut ConnectionManager,
    key: &str,
    pointer: &[u8],
    ttl_secs: u64,
) -> redis::RedisResult<bool> {
    let (value_key, expiry, reservations, counters) = scoped_pointer_keys(key);
    let stored: i64 = CACHE_PUT
        .key(&value_key)
        .key(expiry)
        .key(reservations)
        .key(counters)
        .arg(&value_key)
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
    let (value_key, expiry, reservations, counters) = scoped_pointer_keys(key);
    CACHE_GET
        .key(&value_key)
        .key(expiry)
        .key(reservations)
        .key(counters)
        .arg(value_key)
        .invoke_async(connection)
        .await
}

pub(super) async fn cleanup_caches(
    connection: &mut ConnectionManager,
    maximum_records: usize,
) -> redis::RedisResult<usize> {
    let (_, generic_expiry, generic_reservations, generic_counters) = generic_cache_keys("");
    let (_, scoped_expiry, scoped_reservations, scoped_counters) = scoped_pointer_keys("");
    let generic_cleaned: usize = CACHE_CLEANUP
        .key(generic_expiry)
        .key(generic_reservations)
        .key(generic_counters)
        .arg(maximum_records)
        .invoke_async(connection)
        .await?;
    let scoped_cleaned: usize = CACHE_CLEANUP
        .key(scoped_expiry)
        .key(scoped_reservations)
        .key(scoped_counters)
        .arg(maximum_records)
        .invoke_async(connection)
        .await?;
    Ok(generic_cleaned.saturating_add(scoped_cleaned))
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
        let replacement_terminal = BusMessage::Unary(json!({"generation":"replacement"}));
        let _: () = connection
            .set(
                terminal_key(&id),
                serde_json::to_vec(&replacement_terminal).unwrap(),
            )
            .await
            .unwrap();
        assert!(read_terminal(&mut connection, &id, &first.generation)
            .await
            .unwrap()
            .is_none());
        assert!(matches!(
            read_terminal(&mut connection, &id, &replacement.generation)
                .await
                .unwrap(),
            Some(BusMessage::Unary(value)) if value == json!({"generation":"replacement"})
        ));
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_cleanup_recovers_every_state_after_payload_metadata_expire() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let client = redis::Client::open(redis_url).unwrap();
        let mut connection = ConnectionManager::new(client).await.unwrap();
        let provider = format!("cleanup-recovery-{}", uuid::Uuid::new_v4().simple());
        let scope = QueueProtocol::LegacyUnscoped.scope_name();
        let states = [
            ("waiting", 0_u64, 0_u64),
            ("processing", 64, 0),
            ("terminal", 48, 0),
            ("terminal", 72, 0),
            ("dlq", 0, 128),
            ("terminal", 0, 0),
        ];
        let baseline_jobs: i64 = connection
            .hget(counters_key(), "jobs")
            .await
            .unwrap_or_default();
        let baseline_base: i64 = connection
            .hget(counters_key(), "base_bytes")
            .await
            .unwrap_or_default();
        let baseline_terminal: i64 = connection
            .hget(counters_key(), "terminal_bytes")
            .await
            .unwrap_or_default();
        let baseline_dlq: i64 = connection
            .hget(counters_key(), "dlq_bytes")
            .await
            .unwrap_or_default();
        let mut members = Vec::new();
        let mut total_base = 0_i64;
        let mut total_terminal = 0_i64;
        let mut total_dlq = 0_i64;
        for (index, (state, terminal_charge, dlq_charge)) in states.into_iter().enumerate() {
            let id = format!("cleanup-recovery-{}-{index}", uuid::Uuid::new_v4().simple());
            let member = format!("{id}:{}", index + 1);
            let base_charge = 128_i64 + index as i64;
            total_base += base_charge;
            total_terminal += terminal_charge as i64;
            total_dlq += dlq_charge as i64;
            let _: () = connection
                .hset(reservations_key(), &member, base_charge)
                .await
                .unwrap();
            let _: () = connection
                .hset(terminal_reservations_key(), &member, terminal_charge)
                .await
                .unwrap();
            let _: () = connection
                .hset(dlq_reservations_key(), &member, dlq_charge)
                .await
                .unwrap();
            let _: () = connection
                .hset(reservation_states_key(), &member, state)
                .await
                .unwrap();
            let _: () = connection
                .hset(reservation_scopes_key(), &member, scope)
                .await
                .unwrap();
            let _: () = connection
                .hset(reservation_providers_key(), &member, &provider)
                .await
                .unwrap();
            match state {
                "waiting" => {
                    let _: () = connection
                        .zadd(
                            queue_key(QueueProtocol::LegacyUnscoped, &provider),
                            &member,
                            1,
                        )
                        .await
                        .unwrap();
                }
                "processing" => {
                    let _: () = connection
                        .zadd(
                            processing_key(QueueProtocol::LegacyUnscoped, &provider),
                            &member,
                            1,
                        )
                        .await
                        .unwrap();
                    let _: () = connection
                        .hset(
                            leased_key(QueueProtocol::LegacyUnscoped, &provider),
                            &member,
                            1,
                        )
                        .await
                        .unwrap();
                    let _: () = connection
                        .hset(
                            owners_key(QueueProtocol::LegacyUnscoped, &provider),
                            &member,
                            "expired-owner",
                        )
                        .await
                        .unwrap();
                }
                "dlq" => {
                    let _: () = connection
                        .zadd(
                            dlq_key(QueueProtocol::LegacyUnscoped, &provider),
                            &member,
                            1,
                        )
                        .await
                        .unwrap();
                }
                _ => {}
            }
            if index == 3 {
                let _: () = redis::cmd("HSET")
                    .arg(meta_key(&id))
                    .arg("generation")
                    .arg((index + 1).to_string())
                    .arg("state")
                    .arg("terminal")
                    .query_async(&mut connection)
                    .await
                    .unwrap();
            } else {
                let _: () = connection.set(meta_key(&id), "expired").await.unwrap();
            }
            let _: () = connection.set(payload_key(&id), "private").await.unwrap();
            let _: () = connection.set(terminal_key(&id), "private").await.unwrap();
            if index == 3 {
                assert!(extend_terminal_retention(
                    &mut connection,
                    &id,
                    &(index + 1).to_string(),
                    86_400,
                )
                .await
                .unwrap());
            }
            let _: i64 = connection
                .del((meta_key(&id), payload_key(&id), terminal_key(&id)))
                .await
                .unwrap();
            let _: () = connection
                .zadd(
                    expiry_key(),
                    &member,
                    -9_000_000_000_000_000_i64 + index as i64,
                )
                .await
                .unwrap();
            members.push(member);
        }
        let _: i64 = connection
            .hincr(counters_key(), "jobs", states.len() as i64)
            .await
            .unwrap();
        let _: i64 = connection
            .hincr(counters_key(), "base_bytes", total_base)
            .await
            .unwrap();
        let _: i64 = connection
            .hincr(counters_key(), "terminal_bytes", total_terminal)
            .await
            .unwrap();
        let _: i64 = connection
            .hincr(counters_key(), "dlq_bytes", total_dlq)
            .await
            .unwrap();

        assert_eq!(
            cleanup_expired(&mut connection, states.len())
                .await
                .unwrap(),
            states.len()
        );
        assert_eq!(
            connection
                .hget::<_, _, i64>(counters_key(), "jobs")
                .await
                .unwrap_or_default(),
            baseline_jobs
        );
        assert_eq!(
            connection
                .hget::<_, _, i64>(counters_key(), "base_bytes")
                .await
                .unwrap_or_default(),
            baseline_base
        );
        assert_eq!(
            connection
                .hget::<_, _, i64>(counters_key(), "terminal_bytes")
                .await
                .unwrap_or_default(),
            baseline_terminal
        );
        assert_eq!(
            connection
                .hget::<_, _, i64>(counters_key(), "dlq_bytes")
                .await
                .unwrap_or_default(),
            baseline_dlq
        );
        for member in members {
            assert!(!connection
                .hexists::<_, _, bool>(reservations_key(), &member)
                .await
                .unwrap());
            assert!(!connection
                .zscore::<_, _, Option<f64>>(
                    processing_key(QueueProtocol::LegacyUnscoped, &provider),
                    &member,
                )
                .await
                .unwrap()
                .is_some());
            assert!(!connection
                .zscore::<_, _, Option<f64>>(
                    dlq_key(QueueProtocol::LegacyUnscoped, &provider),
                    &member,
                )
                .await
                .unwrap()
                .is_some());
        }

        let corrupt_member = format!("corrupt-{}:1", uuid::Uuid::new_v4().simple());
        let _: () = connection
            .hset(reservations_key(), &corrupt_member, 64_u64)
            .await
            .unwrap();
        let _: () = connection
            .zadd(expiry_key(), &corrupt_member, -9_000_000_000_000_100_i64)
            .await
            .unwrap();
        assert_eq!(cleanup_expired(&mut connection, 1).await.unwrap(), 0);
        assert!(connection
            .hexists::<_, _, bool>(reservations_key(), &corrupt_member)
            .await
            .unwrap());
        assert!(connection
            .zscore::<_, _, Option<f64>>(expiry_key(), &corrupt_member)
            .await
            .unwrap()
            .is_some());
        let _: i64 = connection
            .hdel(reservations_key(), &corrupt_member)
            .await
            .unwrap();
        let _: i64 = connection
            .zrem(expiry_key(), &corrupt_member)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_cache_values_expire_idle_and_restart_cleanup_releases_accounting() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let client = redis::Client::open(redis_url).unwrap();
        let mut connection = ConnectionManager::new(client).await.unwrap();
        let cache_keys = [
            generic_cache_keys(&format!("idle-{}", uuid::Uuid::new_v4().simple())),
            scoped_pointer_keys(&format!("idle-{}", uuid::Uuid::new_v4().simple())),
        ];
        for (value_key, expiry, reservations, counters) in &cache_keys {
            let stored: i64 = CACHE_PUT
                .key(value_key)
                .key(expiry)
                .key(reservations)
                .key(counters)
                .arg(value_key)
                .arg(b"private-idle-value")
                .arg(5_u64)
                .arg(1024_u64)
                .arg(100_000_u64)
                .arg(64 * 1024 * 1024_u64)
                .invoke_async(&mut connection)
                .await
                .unwrap();
            assert_eq!(stored, 1);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        for (value_key, _, reservations, counters) in &cache_keys {
            assert!(!connection.exists::<_, bool>(value_key).await.unwrap());
            assert!(connection
                .hexists::<_, _, bool>(reservations, value_key)
                .await
                .unwrap());
            assert!(
                connection
                    .hget::<_, _, i64>(counters, "count")
                    .await
                    .unwrap()
                    > 0
            );
        }
        assert_eq!(cleanup_caches(&mut connection, 16).await.unwrap(), 2);
        for (value_key, _, reservations, counters) in &cache_keys {
            assert!(!connection
                .hexists::<_, _, bool>(reservations, value_key)
                .await
                .unwrap());
            assert_eq!(
                connection
                    .hget::<_, _, i64>(counters, "count")
                    .await
                    .unwrap_or_default(),
                0
            );
            assert_eq!(
                connection
                    .hget::<_, _, i64>(counters, "bytes")
                    .await
                    .unwrap_or_default(),
                0
            );
        }
    }
}

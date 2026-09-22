//! Redis-backed **distributed gateway** (features `gateway` + `redis-coordination`).
//!
//! The in-memory [`Scheduler`](super::Scheduler) governs one process. A fleet of
//! gateway replicas (Cloud Run / ECS) needs a *shared* priority queue so any
//! instance can serve any request, ordered globally by tier then FIFO (with
//! aging). Built-in HTTP jobs carry a server-created policy scope so the shared
//! coordinator atomically gates provider and tenant limits at each real send;
//! unscoped custom jobs retain the standalone Redis limiter behavior.
//!
//! As the advisor put it, you can't stretch the in-process `Job`/`RequestQueue`
//! (it holds a `oneshot`) across processes — so distributed mode is a **separate
//! seam**:
//!
//! * a **serializable priority queue** — a Redis sorted set per provider scored
//!   by a *virtual deadline* `enqueue_ms − tier·aging_step`, popped with
//!   `ZPOPMIN`. Higher tier and older age both yield an earlier deadline, so
//!   priority, FIFO, and **anti-starvation aging** all fall out of one static
//!   score with no re-scoring;
//! * an **at-least-once lease** — leasing atomically moves a job to a
//!   `processing` set with a visibility deadline (recording its score); a
//!   background **reaper** requeues leases whose deadline passed, so a worker
//!   that crashes mid-dispatch doesn't drop the request; and
//! * a **response bus** — Redis pub/sub on a per-request channel carrying typed
//!   [`BusMessage`]s, so the worker that dispatches a job streams the result
//!   (unary or chunked) back to the *origin* instance's open HTTP connection.
//!
//! ```text
//!   instance A: submit ─subscribe(resp:ID)─ZADD q:prov─┐            await bus
//!                                                       ▼
//!   shared Redis:   [ ZSET q:prov ] [ ZSET processing ] [ pub/sub resp:ID ]
//!                                                       ▲
//!   instance B: worker ─lease(ZPOPMIN→processing)─rate─dispatch─publish─ack
//!                                    reaper: expired processing → requeue
//! ```
//!
//! At-least-once means a redelivered job may run twice (idempotent upstream
//! calls, a wasted call at worst); streams refresh their lease as they run to
//! avoid mid-stream redelivery.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;

use super::{Dispatch, GatewayConfig, GatewayError, GatewayRequest, StreamChunk};
use crate::proxy::ratelimit::{RateKey, RateLimiter, RetryAfter};

mod lifecycle;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Virtual-deadline score: **smaller = dispatched sooner** (`ZPOPMIN`). Higher
/// tier subtracts more, so it leads; and a smaller `enqueue_ms` (older job)
/// leads within a tier and eventually overtakes newer higher tiers — aging with
/// no re-scoring. Stays well within f64's exact-integer range.
fn deadline_score(tier: u8, enqueue_ms: u64, aging_step_ms: u64) -> f64 {
    enqueue_ms as f64 - (tier as u64 * aging_step_ms) as f64
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueueProtocol {
    LegacyUnscoped,
    ScopedV1,
}

impl QueueProtocol {
    fn scope_name(self) -> &'static str {
        match self {
            Self::LegacyUnscoped => "unscoped",
            Self::ScopedV1 => "scoped",
        }
    }
}

fn protocol_key(protocol: QueueProtocol, kind: &str, identifier: &str) -> String {
    match kind {
        "q" => lifecycle::queue_key(protocol, identifier),
        "proc" => lifecycle::processing_key(protocol, identifier),
        "leased" => lifecycle::leased_key(protocol, identifier),
        "owners" => lifecycle::owners_key(protocol, identifier),
        "dlq" => lifecycle::dlq_key(protocol, identifier),
        "resp" => lifecycle::response_channel(protocol, identifier),
        _ => format!(
            "llmshim:gw:lifecycle:v2:{}:{kind}:{identifier}",
            protocol.scope_name()
        ),
    }
}

fn l1_protocol_key(protocol: QueueProtocol, kind: &str, identifier: &str) -> String {
    match protocol {
        QueueProtocol::LegacyUnscoped => {
            format!("llmshim:gw:fenced:v1:{kind}:{identifier}")
        }
        QueueProtocol::ScopedV1 => format!("llmshim:gw:scoped:v2:{kind}:{identifier}"),
    }
}

fn released_protocol_key(protocol: QueueProtocol, kind: &str, identifier: &str) -> String {
    match protocol {
        QueueProtocol::LegacyUnscoped => format!("llmshim:gw:{kind}:{identifier}"),
        QueueProtocol::ScopedV1 => format!("llmshim:gw:scoped:v1:{kind}:{identifier}"),
    }
}

fn prior_lifecycle_key(protocol: QueueProtocol, kind: &str, identifier: &str) -> String {
    format!(
        "llmshim:gw:lifecycle:v1:{}:{kind}:{identifier}",
        protocol.scope_name()
    )
}

fn protocol_queue_key(protocol: QueueProtocol, provider: &str) -> String {
    protocol_key(protocol, "q", provider)
}
fn protocol_processing_key(protocol: QueueProtocol, provider: &str) -> String {
    protocol_key(protocol, "proc", provider)
}
fn protocol_leased_key(protocol: QueueProtocol, provider: &str) -> String {
    protocol_key(protocol, "leased", provider)
}
fn protocol_owners_key(protocol: QueueProtocol, provider: &str) -> String {
    protocol_key(protocol, "owners", provider)
}
fn protocol_response_channel(protocol: QueueProtocol, id: &str) -> String {
    protocol_key(protocol, "resp", id)
}
fn released_queue_key(protocol: QueueProtocol, provider: &str) -> String {
    released_protocol_key(protocol, "q", provider)
}
fn released_processing_key(protocol: QueueProtocol, provider: &str) -> String {
    released_protocol_key(protocol, "proc", provider)
}
fn released_dlq_key(protocol: QueueProtocol, provider: &str) -> String {
    released_protocol_key(protocol, "dlq", provider)
}

#[cfg(test)]
fn queue_key(provider: &str) -> String {
    protocol_queue_key(QueueProtocol::LegacyUnscoped, provider)
}
#[cfg(test)]
fn processing_key(provider: &str) -> String {
    protocol_processing_key(QueueProtocol::LegacyUnscoped, provider)
}
#[cfg(test)]
fn leased_key(provider: &str) -> String {
    protocol_leased_key(QueueProtocol::LegacyUnscoped, provider)
}
fn scoped_idempotency_key(context: &crate::gateway::idempotency::IdempotencyContext) -> String {
    format!("llmshim:gw:idem:v2:{}", context.storage_key())
}
fn generic_idempotency_key(client_key: &str) -> String {
    format!(
        "llmshim:gw:idem:generic:v1:{}",
        crate::gateway::idempotency::generic_storage_key(client_key)
    )
}

// Atomic lease: pop the earliest-deadline job, move it to `processing` with a
// visibility deadline, and record its score for redelivery. KEYS: queue,
// processing, leased, owners, lifecycle counters. ARGV: lease_duration_ms,
// owner_token, terminal pool max, unary headroom, stream headroom, job prefix,
// retained processing ttl ms, processing lifetime ms.
const LEASE_LUA: &str = r#"
    if not ARGV[6] then
        local legacy_top = redis.call('ZPOPMIN', KEYS[1], 1)
        if #legacy_top == 0 then return false end
        redis.call('ZADD', KEYS[2], ARGV[1], legacy_top[1])
        redis.call('HSET', KEYS[3], legacy_top[1], legacy_top[2])
        redis.call('HSET', KEYS[4], legacy_top[1], ARGV[2])
        return {legacy_top[1], legacy_top[2]}
    end
    local top = redis.call('ZRANGE', KEYS[1], 0, 0, 'WITHSCORES')
    if #top == 0 then return false end
    local m = top[1]
    local s = top[2]
    local id, generation = string.match(m, '^(.*):([^:]*)$')
    if not id then redis.call('ZREM', KEYS[1], m); return false end
    local meta = ARGV[6] .. id .. ':meta'
    local payload_key = ARGV[6] .. id .. ':payload'
    if redis.call('HGET', meta, 'generation') ~= generation
        or redis.call('HGET', meta, 'state') ~= 'waiting' then
        redis.call('ZREM', KEYS[1], m)
        return false
    end
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local origin_expiry = tonumber(redis.call('HGET', meta, 'origin_expires_at_ms') or '0')
    local origin_max = tonumber(redis.call('HGET', meta, 'origin_max_expires_at_ms') or '0')
    if origin_expiry <= now_ms or origin_max <= now_ms then
        redis.call('HSET', meta, 'cancel_requested', '1')
    end
    if redis.call('HGET', meta, 'cancel_requested') == '1' then return false end
    local terminal_charge = tonumber(ARGV[4])
    if redis.call('HGET', meta, 'stream') == '1' then terminal_charge = tonumber(ARGV[5]) end
    local terminal_bytes = tonumber(redis.call('HGET', KEYS[5], 'terminal_bytes') or '0')
    local existing_charge = tonumber(redis.call('HGET', meta, 'terminal_charge') or '0')
    if existing_charge > 0 then terminal_charge = existing_charge
    elseif terminal_bytes + terminal_charge > tonumber(ARGV[3]) then return false end
    local payload = redis.call('GET', payload_key)
    if not payload then return false end
    redis.call('ZREM', KEYS[1], m)
    redis.call('ZADD', KEYS[2], now_ms + ARGV[1], m)
    redis.call('HSET', KEYS[3], m, s)
    redis.call('HSET', KEYS[4], m, ARGV[2])
    if existing_charge == 0 then redis.call('HINCRBY', KEYS[5], 'terminal_bytes', terminal_charge) end
    redis.call('HSET', meta, 'state', 'processing', 'owner', ARGV[2],
        'terminal_charge', terminal_charge)
    redis.call('HSET', KEYS[6], m, terminal_charge)
    redis.call('HSET', KEYS[7], m, 'processing')
    redis.call('ZADD', KEYS[8], now_ms + tonumber(ARGV[8]), m)
    local retained_ttl = tonumber(ARGV[7]) or 25200000
    redis.call('PEXPIRE', meta, retained_ttl)
    redis.call('PEXPIRE', payload_key, retained_ttl)
    return {m, s, payload}
"#;

// Ack only the caller's delivery. KEYS: processing, leased, owners.
// ARGV: member, owner_token.
const ACK_LUA: &str = r#"
    if redis.call('HGET', KEYS[3], ARGV[1]) ~= ARGV[2] then return 0 end
    redis.call('ZREM', KEYS[1], ARGV[1])
    redis.call('HDEL', KEYS[2], ARGV[1])
    redis.call('HDEL', KEYS[3], ARGV[1])
    return 1
"#;

// Release a lease back to the queue (e.g. rate-limited), keeping its score.
// KEYS: queue, processing, leased, owners, counters, metadata, terminal
// reservation, reservation state.
// ARGV: member, score, owner_token, generation.
const RELEASE_LUA: &str = r#"
    if redis.call('HGET', KEYS[4], ARGV[1]) ~= ARGV[3] then return 0 end
    if redis.call('HGET', KEYS[6], 'generation') ~= ARGV[4] then return 0 end
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local origin_expiry = tonumber(redis.call('HGET', KEYS[6], 'origin_expires_at_ms') or '0')
    local origin_max = tonumber(redis.call('HGET', KEYS[6], 'origin_max_expires_at_ms') or '0')
    if origin_expiry <= now_ms or origin_max <= now_ms then
        redis.call('HSET', KEYS[6], 'cancel_requested', '1')
    end
    if redis.call('HGET', KEYS[6], 'cancel_requested') == '1' then return -1 end
    local terminal_charge = tonumber(redis.call('HGET', KEYS[6], 'terminal_charge') or '0')
    if terminal_charge > 0 then
        redis.call('HINCRBY', KEYS[5], 'terminal_bytes', -terminal_charge)
    end
    redis.call('HSET', KEYS[6], 'state', 'waiting', 'owner', '', 'terminal_charge', '0')
    redis.call('HSET', KEYS[7], ARGV[1], 0)
    redis.call('HSET', KEYS[8], ARGV[1], 'waiting')
    redis.call('ZADD', KEYS[1], ARGV[2], ARGV[1])
    redis.call('ZREM', KEYS[2], ARGV[1])
    redis.call('HDEL', KEYS[3], ARGV[1])
    redis.call('HDEL', KEYS[4], ARGV[1])
    return 1
"#;

// Reap expired leases back to the queue with their original score. KEYS:
// processing, queue, leased, owners, counters, expiry, terminal reservation,
// reservation state. ARGV: limit, job prefix, terminal ttl ms, cancellation
// envelope, cleanup grace ms.
const REAP_LUA: &str = r#"
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local expired = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', now_ms, 'LIMIT', 0, ARGV[1])
    local n = 0
    for _, m in ipairs(expired) do
        local id, generation = string.match(m, '^(.*):([^:]*)$')
        local meta = id and (ARGV[2] .. id .. ':meta') or nil
        local s = redis.call('HGET', KEYS[3], m)
        local origin_expired = false
        if meta then
            local origin_expiry = tonumber(redis.call('HGET', meta, 'origin_expires_at_ms') or '0')
            local origin_max = tonumber(redis.call('HGET', meta, 'origin_max_expires_at_ms') or '0')
            origin_expired = origin_expiry <= now_ms or origin_max <= now_ms
            if origin_expired then redis.call('HSET', meta, 'cancel_requested', '1') end
        end
        if meta and redis.call('HGET', meta, 'generation') == generation
            and redis.call('HGET', meta, 'cancel_requested') == '1' then
            local terminal_charge = tonumber(redis.call('HGET', meta, 'terminal_charge') or '0')
            if terminal_charge > 0 then
                redis.call('HINCRBY', KEYS[5], 'terminal_bytes', string.len(ARGV[4]) - terminal_charge)
            end
            redis.call('SET', ARGV[2] .. id .. ':terminal', ARGV[4])
            redis.call('HSET', meta, 'state', 'terminal', 'terminal_charge', string.len(ARGV[4]))
            redis.call('HSET', KEYS[7], m, string.len(ARGV[4]))
            redis.call('HSET', KEYS[8], m, 'terminal')
            redis.call('ZADD', KEYS[6], now_ms + tonumber(ARGV[3]), m)
            redis.call('ZREM', KEYS[9], m)
            local hard_ttl = tonumber(ARGV[3]) + tonumber(ARGV[5])
            redis.call('PEXPIRE', meta, hard_ttl)
            redis.call('PEXPIRE', ARGV[2] .. id .. ':payload', hard_ttl)
            redis.call('PEXPIRE', ARGV[2] .. id .. ':terminal', hard_ttl)
        elseif meta and redis.call('HGET', meta, 'generation') == generation then
            if s then redis.call('ZADD', KEYS[2], s, m) end
            redis.call('HSET', meta, 'state', 'waiting', 'owner', '')
            redis.call('HSET', KEYS[8], m, 'waiting')
        end
        redis.call('ZREM', KEYS[1], m)
        redis.call('HDEL', KEYS[3], m)
        redis.call('HDEL', KEYS[4], m)
        n = n + 1
    end
    return n
"#;

// Extend only the caller's current delivery. KEYS: processing, owners.
// ARGV: member, owner_token, lease_duration_ms.
const REFRESH_LUA: &str = r#"
    if redis.call('HGET', KEYS[2], ARGV[1]) ~= ARGV[2] then return 0 end
    if not redis.call('ZSCORE', KEYS[1], ARGV[1]) then return 0 end
    if redis.call('HGET', KEYS[3], 'generation') ~= ARGV[4] then return 0 end
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local origin_expiry = tonumber(redis.call('HGET', KEYS[3], 'origin_expires_at_ms') or '0')
    local origin_max = tonumber(redis.call('HGET', KEYS[3], 'origin_max_expires_at_ms') or '0')
    if origin_expiry <= now_ms or origin_max <= now_ms then
        redis.call('HSET', KEYS[3], 'cancel_requested', '1')
    end
    if redis.call('HGET', KEYS[3], 'cancel_requested') == '1' then return -1 end
    redis.call('ZADD', KEYS[1], 'XX', now_ms + ARGV[3], ARGV[1])
    return 1
"#;

// Publish only while the caller owns the current delivery. KEYS: processing,
// owners, metadata. ARGV: member, owner_token, channel, payload, generation. A zero subscriber count
// is still a successful fenced publish.
const PUBLISH_LUA: &str = r#"
    if redis.call('HGET', KEYS[2], ARGV[1]) ~= ARGV[2] then return -1 end
    if not redis.call('ZSCORE', KEYS[1], ARGV[1]) then return -1 end
    if redis.call('HGET', KEYS[3], 'generation') ~= ARGV[5] then return -1 end
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local origin_expiry = tonumber(redis.call('HGET', KEYS[3], 'origin_expires_at_ms') or '0')
    local origin_max = tonumber(redis.call('HGET', KEYS[3], 'origin_max_expires_at_ms') or '0')
    if origin_expiry <= now_ms or origin_max <= now_ms then
        redis.call('HSET', KEYS[3], 'cancel_requested', '1')
    end
    if redis.call('HGET', KEYS[3], 'cancel_requested') == '1' then return -2 end
    return redis.call('PUBLISH', ARGV[3], ARGV[4])
"#;

// Complete only the caller's delivery and install the done marker atomically.
// KEYS: processing, leased, owners, done. ARGV: member, owner_token, ttl_secs.
#[cfg(test)]
const COMPLETE_LUA: &str = r#"
    if redis.call('HGET', KEYS[3], ARGV[1]) ~= ARGV[2] then return 0 end
    if not redis.call('ZSCORE', KEYS[1], ARGV[1]) then return 0 end
    redis.call('SET', KEYS[4], 1, 'EX', ARGV[3])
    redis.call('ZREM', KEYS[1], ARGV[1])
    redis.call('HDEL', KEYS[2], ARGV[1])
    redis.call('HDEL', KEYS[3], ARGV[1])
    return 1
"#;

// Publish and retain a terminal envelope while completing the same delivery.
// KEYS: processing, leased, owners, done, lifecycle counters, job metadata,
// terminal payload, expiry index, request payload, terminal reservation,
// reservation state. ARGV: member, owner_token, ttl_secs, channel, payload,
// generation, cleanup_grace_ms, cancellation envelope.
const TERMINAL_PUBLISH_COMPLETE_LUA: &str = r#"
    if redis.call('HGET', KEYS[3], ARGV[1]) ~= ARGV[2] then return 0 end
    if not redis.call('ZSCORE', KEYS[1], ARGV[1]) then return 0 end
    if redis.call('HGET', KEYS[6], 'generation') ~= ARGV[6] then return 0 end
    local reserved = tonumber(redis.call('HGET', KEYS[6], 'terminal_charge') or '0')
    local terminal_payload = ARGV[5]
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local origin_expiry = tonumber(redis.call('HGET', KEYS[6], 'origin_expires_at_ms') or '0')
    local origin_max = tonumber(redis.call('HGET', KEYS[6], 'origin_max_expires_at_ms') or '0')
    if origin_expiry <= now_ms or origin_max <= now_ms then
        redis.call('HSET', KEYS[6], 'cancel_requested', '1')
    end
    if redis.call('HGET', KEYS[6], 'cancel_requested') == '1' then
        terminal_payload = ARGV[8]
    end
    local actual = string.len(terminal_payload)
    if actual > reserved then return -2 end
    redis.call('SET', KEYS[7], terminal_payload)
    redis.call('HSET', KEYS[6], 'state', 'terminal', 'terminal_charge', actual)
    redis.call('HSET', KEYS[10], ARGV[1], actual)
    redis.call('HSET', KEYS[11], ARGV[1], 'terminal')
    redis.call('HINCRBY', KEYS[5], 'terminal_bytes', actual - reserved)
    redis.call('ZADD', KEYS[8], now_ms + tonumber(ARGV[3]) * 1000, ARGV[1])
    redis.call('PEXPIRE', KEYS[6], tonumber(ARGV[3]) * 1000 + tonumber(ARGV[7]))
    redis.call('PEXPIRE', KEYS[7], tonumber(ARGV[3]) * 1000 + tonumber(ARGV[7]))
    redis.call('PEXPIRE', KEYS[9], tonumber(ARGV[3]) * 1000 + tonumber(ARGV[7]))
    redis.call('PUBLISH', ARGV[4], terminal_payload)
    redis.call('SET', KEYS[4], 1, 'EX', ARGV[3])
    redis.call('ZREM', KEYS[1], ARGV[1])
    redis.call('HDEL', KEYS[2], ARGV[1])
    redis.call('HDEL', KEYS[3], ARGV[1])
    redis.call('ZREM', KEYS[12], ARGV[1])
    return 1
"#;

// Move only the caller's delivery to the bounded lifecycle DLQ. KEYS:
// processing, leased, owners, dlq, counters, metadata, expiry, reservations,
// payload, terminal, terminal reservation, DLQ reservation, reservation state.
// ARGV: member, owner_token, generation, ttl_ms, max_count, max_bytes,
// terminal_ttl_ms, channel, DLQ envelope, cancellation envelope, cleanup grace ms.
const DEAD_LETTER_LUA: &str = r#"
    if redis.call('HGET', KEYS[3], ARGV[1]) ~= ARGV[2] then return 0 end
    if not redis.call('ZSCORE', KEYS[1], ARGV[1]) then return 0 end
    if redis.call('HGET', KEYS[6], 'generation') ~= ARGV[3] then return 0 end
    local base_charge = tonumber(redis.call('HGET', KEYS[6], 'base_charge') or '0')
    local redis_time = redis.call('TIME')
    local now_ms = redis_time[1] * 1000 + math.floor(redis_time[2] / 1000)
    local origin_expiry = tonumber(redis.call('HGET', KEYS[6], 'origin_expires_at_ms') or '0')
    local origin_max = tonumber(redis.call('HGET', KEYS[6], 'origin_max_expires_at_ms') or '0')
    local canceled = redis.call('HGET', KEYS[6], 'cancel_requested') == '1'
        or origin_expiry <= now_ms or origin_max <= now_ms
    if canceled then redis.call('HSET', KEYS[6], 'cancel_requested', '1') end
    local dlq_bytes = tonumber(redis.call('HGET', KEYS[5], 'dlq_bytes') or '0')
    local overflow = redis.call('ZCARD', KEYS[4]) >= tonumber(ARGV[5])
        or dlq_bytes + base_charge > tonumber(ARGV[6])
    local terminal_charge = tonumber(redis.call('HGET', KEYS[6], 'terminal_charge') or '0')
    if terminal_charge > 0 then
        redis.call('HINCRBY', KEYS[5], 'terminal_bytes', -terminal_charge)
    end
    local terminal_payload = canceled and ARGV[10] or ARGV[9]
    redis.call('SET', KEYS[10], terminal_payload)
    redis.call('HSET', KEYS[11], ARGV[1], 0)
    if canceled then
        redis.call('HSET', KEYS[6], 'state', 'terminal', 'terminal_charge', '0')
        redis.call('HSET', KEYS[12], ARGV[1], 0)
        redis.call('HSET', KEYS[13], ARGV[1], 'terminal')
        redis.call('ZADD', KEYS[7], now_ms + tonumber(ARGV[7]), ARGV[1])
        redis.call('ZREM', KEYS[4], ARGV[1])
    elseif overflow then
        redis.call('HINCRBY', KEYS[5], 'dlq_overflow', 1)
        redis.call('HSET', KEYS[6], 'state', 'terminal', 'terminal_charge', '0')
        redis.call('HSET', KEYS[12], ARGV[1], 0)
        redis.call('HSET', KEYS[13], ARGV[1], 'terminal')
        redis.call('ZADD', KEYS[7], now_ms + tonumber(ARGV[7]), ARGV[1])
    else
        redis.call('ZADD', KEYS[4], now_ms, ARGV[1])
        redis.call('ZADD', KEYS[7], now_ms + tonumber(ARGV[4]), ARGV[1])
        redis.call('HINCRBY', KEYS[5], 'dlq_bytes', base_charge)
        redis.call('HSET', KEYS[6], 'state', 'dlq', 'terminal_charge', '0')
        redis.call('HSET', KEYS[12], ARGV[1], base_charge)
        redis.call('HSET', KEYS[13], ARGV[1], 'dlq')
    end
    local hard_ttl = (canceled or overflow) and (tonumber(ARGV[7]) + tonumber(ARGV[11]))
        or (tonumber(ARGV[4]) + tonumber(ARGV[11]))
    redis.call('PEXPIRE', KEYS[6], hard_ttl)
    redis.call('PEXPIRE', KEYS[9], hard_ttl)
    redis.call('PEXPIRE', KEYS[10], hard_ttl)
    redis.call('PUBLISH', ARGV[8], terminal_payload)
    redis.call('ZREM', KEYS[1], ARGV[1])
    redis.call('HDEL', KEYS[2], ARGV[1])
    redis.call('HDEL', KEYS[3], ARGV[1])
    redis.call('ZREM', KEYS[14], ARGV[1])
    return 1
"#;

const BUMP_ATTEMPTS_LUA: &str = r#"
    local delivery_attempts = redis.call('INCR', KEYS[1])
    redis.call('EXPIRE', KEYS[1], ARGV[1])
    return delivery_attempts
"#;

/// A queued unit of work, serialized into the Redis sorted set.
#[derive(Clone, Serialize, Deserialize)]
struct JobDescriptor {
    id: String,
    provider: String,
    tier: u8,
    permits: u32,
    payload: Value,
    #[serde(default)]
    policy_envelope_version: u8,
    #[serde(default)]
    trusted_unscoped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy_scope: Option<crate::gateway::attempt::TrustedPolicyScope>,
    #[serde(default)]
    stream: bool,
    /// Enqueue time (epoch ms) — drives the deadline score and the queue-wait
    /// metric.
    #[serde(default)]
    enqueue_ms: u64,
}

#[derive(Clone)]
pub(crate) struct PreparedSubmission {
    descriptor: JobDescriptor,
    member: String,
    protocol: QueueProtocol,
    serialized_payload: String,
    reservation_nonce: String,
    origin_token: String,
}

pub(crate) struct AcceptedSubmission {
    prepared: PreparedSubmission,
    pubsub: redis::aio::PubSub,
    origin_deadline: tokio::time::Instant,
    origin_guard: OriginDropGuard,
}

#[derive(Clone)]
struct OriginCancelCommand {
    protocol: QueueProtocol,
    provider: String,
    id: String,
    generation: String,
    member: String,
    origin_token: String,
}

struct OriginDropGuard {
    sender: mpsc::Sender<OriginCancelCommand>,
    command: Option<OriginCancelCommand>,
}

impl OriginDropGuard {
    fn disarm(&mut self) {
        self.command.take();
    }
}

impl Drop for OriginDropGuard {
    fn drop(&mut self) {
        if let Some(command) = self.command.take() {
            let _ = self.sender.try_send(command);
        }
    }
}

#[derive(Clone)]
struct LeaseOwnership {
    protocol: QueueProtocol,
    provider: String,
    member: String,
    original_score: String,
    owner_token: String,
    serialized_payload: String,
}

enum LeaseHeartbeatEnd {
    Lost,
    Canceled,
}

impl LeaseOwnership {
    fn id_and_generation(&self) -> Option<(&str, &str)> {
        self.member.rsplit_once(':')
    }
}

impl PreparedSubmission {
    fn new(descriptor: JobDescriptor) -> Result<Self, GatewayError> {
        let serialized_payload =
            serde_json::to_string(&descriptor).map_err(|error| redis_err(&error))?;
        let protocol = if descriptor.policy_scope.is_some() {
            QueueProtocol::ScopedV1
        } else {
            QueueProtocol::LegacyUnscoped
        };
        Ok(Self {
            member: descriptor.id.clone(),
            descriptor,
            protocol,
            serialized_payload,
            reservation_nonce: uuid::Uuid::new_v4().simple().to_string(),
            origin_token: uuid::Uuid::new_v4().simple().to_string(),
        })
    }
}

fn descriptor_matches_protocol(protocol: QueueProtocol, descriptor: &JobDescriptor) -> bool {
    match protocol {
        QueueProtocol::LegacyUnscoped => descriptor.policy_scope.is_none(),
        QueueProtocol::ScopedV1 => {
            descriptor.policy_envelope_version == 1
                && !descriptor.trusted_unscoped
                && descriptor
                    .policy_scope
                    .as_ref()
                    .is_some_and(crate::gateway::attempt::TrustedPolicyScope::is_current)
        }
    }
}

fn protocol_done_key(protocol: QueueProtocol, id: &str) -> String {
    protocol_key(protocol, "done", id)
}
fn protocol_attempts_key(protocol: QueueProtocol, id: &str) -> String {
    protocol_key(protocol, "attempts", id)
}
fn protocol_dlq_key(protocol: QueueProtocol, provider: &str) -> String {
    protocol_key(protocol, "dlq", provider)
}
/// A message on a request's response channel.
#[derive(Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
enum BusMessage {
    /// Unary success.
    Unary(Value),
    /// A streaming chunk (raw provider SSE `data:` payload).
    Chunk(String),
    /// Stream complete.
    End,
    /// Failure (either mode).
    Error(String),
}

#[derive(Serialize, Deserialize)]
struct ScopedIdempotencyPointer {
    job_id: String,
    generation: String,
    request_fingerprint: String,
}

#[derive(Clone)]
pub(crate) struct LifecycleReference {
    job_id: String,
    generation: String,
}

/// A Redis-backed distributed gateway. One per instance; runs the origin side
/// (`submit` / `submit_stream`) and the worker + reaper side.
pub struct DistributedGateway {
    client: redis::Client,
    connections: Arc<crate::redis_operation::RedisConnectionManagerCache>,
    dispatch: Arc<dyn Dispatch>,
    limiter: Arc<dyn RateLimiter>,
    attempt_coordinator: Option<Arc<crate::gateway::attempt::AttemptCoordinator>>,
    config: GatewayConfig,
    nonce: u128,
    counter: AtomicU64,
    lease: redis::Script,
    ack: redis::Script,
    release: redis::Script,
    reap: redis::Script,
    refresh: redis::Script,
    fenced_publish: redis::Script,
    #[cfg(test)]
    complete: redis::Script,
    terminal_publish_complete: redis::Script,
    dead_letter_transition: redis::Script,
    bump_attempts_script: redis::Script,
    redis_operation_timeout: Duration,
    worker_job_timeout: Duration,
    lifecycle_limits: lifecycle::Limits,
    origin_lease_timeout: Duration,
    origin_cancel_sender: mpsc::Sender<OriginCancelCommand>,
    #[cfg(test)]
    terminal_operation_delay: Duration,
}

impl DistributedGateway {
    /// Connect to Redis and build the gateway. The rate limiter should be the
    /// shared [`RedisRateLimiter`] so the whole fleet coordinates.
    pub async fn connect(
        redis_url: &str,
        dispatch: Arc<dyn Dispatch>,
        limiter: Arc<dyn RateLimiter>,
        config: GatewayConfig,
    ) -> redis::RedisResult<Arc<Self>> {
        Self::connect_inner(redis_url, dispatch, limiter, config, None).await
    }

    pub(crate) async fn connect_with_coordinator(
        redis_url: &str,
        dispatch: Arc<dyn Dispatch>,
        limiter: Arc<dyn RateLimiter>,
        config: GatewayConfig,
        attempt_coordinator: Arc<crate::gateway::attempt::AttemptCoordinator>,
    ) -> redis::RedisResult<Arc<Self>> {
        Self::connect_inner(
            redis_url,
            dispatch,
            limiter,
            config,
            Some(attempt_coordinator),
        )
        .await
    }

    async fn connect_inner(
        redis_url: &str,
        dispatch: Arc<dyn Dispatch>,
        limiter: Arc<dyn RateLimiter>,
        config: GatewayConfig,
        attempt_coordinator: Option<Arc<crate::gateway::attempt::AttemptCoordinator>>,
    ) -> redis::RedisResult<Arc<Self>> {
        let client = redis::Client::open(redis_url)?;
        let nonce = uuid::Uuid::new_v4().as_u128();
        let configured_redis_operation_timeout = positive_duration_from_env(
            "LLMSHIM_GATEWAY_REDIS_OPERATION_TIMEOUT_MS",
            Duration::from_secs(5),
        );
        let redis_operation_timeout = configured_redis_operation_timeout
            .min((config.lease_timeout / 4).max(Duration::from_millis(1)));
        let worker_job_timeout = positive_duration_from_env(
            "LLMSHIM_GATEWAY_WORKER_JOB_TIMEOUT_MS",
            Duration::from_secs(6 * 60 * 60),
        );
        let origin_lease_timeout = positive_duration_from_env(
            "LLMSHIM_GATEWAY_ORIGIN_LEASE_MS",
            config.lease_timeout.min(Duration::from_secs(60)),
        )
        .min(Duration::from_secs(5 * 60));
        let lifecycle_limits = lifecycle::Limits::from_env();
        let cancellation_capacity = usize::try_from(lifecycle_limits.max_jobs)
            .unwrap_or(usize::MAX)
            .min(config.max_queue_depth.max(1))
            .clamp(1, 10_000);
        let (origin_cancel_sender, origin_cancel_receiver) = mpsc::channel(cancellation_capacity);
        let connections = Arc::new(crate::redis_operation::RedisConnectionManagerCache::new(
            client.clone(),
        ));
        run_redis_operation(
            &connections,
            redis_operation_timeout,
            |mut connection| async move {
                redis::cmd("PING")
                    .query_async::<String>(&mut connection)
                    .await
            },
        )
        .await?;
        let gateway = Arc::new(Self {
            client,
            connections,
            dispatch,
            limiter,
            attempt_coordinator,
            config,
            nonce,
            counter: AtomicU64::new(0),
            lease: redis::Script::new(LEASE_LUA),
            ack: redis::Script::new(ACK_LUA),
            release: redis::Script::new(RELEASE_LUA),
            reap: redis::Script::new(REAP_LUA),
            refresh: redis::Script::new(REFRESH_LUA),
            fenced_publish: redis::Script::new(PUBLISH_LUA),
            #[cfg(test)]
            complete: redis::Script::new(COMPLETE_LUA),
            terminal_publish_complete: redis::Script::new(TERMINAL_PUBLISH_COMPLETE_LUA),
            dead_letter_transition: redis::Script::new(DEAD_LETTER_LUA),
            bump_attempts_script: redis::Script::new(BUMP_ATTEMPTS_LUA),
            redis_operation_timeout,
            worker_job_timeout,
            lifecycle_limits,
            origin_lease_timeout,
            origin_cancel_sender,
            #[cfg(test)]
            terminal_operation_delay: Duration::ZERO,
        });
        tokio::spawn(origin_cancellation_pump(
            gateway.connections.clone(),
            gateway.redis_operation_timeout,
            origin_cancel_receiver,
        ));
        Ok(gateway)
    }

    async fn redis_operation<T, F, Fut>(&self, operation: F) -> redis::RedisResult<T>
    where
        F: FnOnce(ConnectionManager) -> Fut,
        Fut: std::future::Future<Output = redis::RedisResult<T>>,
    {
        run_redis_operation(&self.connections, self.redis_operation_timeout, operation).await
    }

    /// Generic Redis cache lookup retained for API compatibility.
    #[deprecated(note = "gateway HTTP replay uses credential-scoped idempotency")]
    pub async fn idem_get(&self, key: &str) -> Option<Value> {
        let storage_key = generic_idempotency_key(key);
        self.redis_operation(|mut connection| async move {
            lifecycle::generic_cache_get(&mut connection, &storage_key).await
        })
        .await
        .ok()
        .flatten()
        .and_then(|value| serde_json::from_slice(&value).ok())
    }

    /// Generic Redis cache insertion retained for API compatibility.
    #[deprecated(note = "gateway HTTP replay uses credential-scoped idempotency")]
    pub async fn idem_put(&self, key: &str, value: &Value, ttl_secs: u64) {
        if let Ok(serialized_value) = serde_json::to_string(value) {
            let storage_key = generic_idempotency_key(key);
            let _ = self
                .redis_operation(|mut connection| async move {
                    lifecycle::generic_cache_put(
                        &mut connection,
                        &storage_key,
                        serialized_value.as_bytes(),
                        ttl_secs,
                    )
                    .await
                })
                .await;
        }
    }

    /// HTTP-gateway lookup for one credential-, route-, and request-bound key.
    pub(crate) async fn scoped_idem_lookup(
        &self,
        context: &crate::gateway::idempotency::IdempotencyContext,
    ) -> crate::gateway::idempotency::IdempotencyLookup {
        let storage_key = scoped_idempotency_key(context);
        let Some(serialized_pointer) = self
            .redis_operation(|mut connection| async move {
                lifecycle::scoped_pointer_get(&mut connection, &storage_key).await
            })
            .await
            .ok()
            .flatten()
        else {
            return crate::gateway::idempotency::IdempotencyLookup::Miss;
        };
        let Ok(pointer) = serde_json::from_slice::<ScopedIdempotencyPointer>(&serialized_pointer)
        else {
            return crate::gateway::idempotency::IdempotencyLookup::Miss;
        };
        if pointer.request_fingerprint != context.request_fingerprint() {
            return crate::gateway::idempotency::IdempotencyLookup::Conflict;
        }
        let pointer_job_id = pointer.job_id;
        let pointer_generation = pointer.generation;
        match self
            .redis_operation(|mut connection| async move {
                lifecycle::read_terminal(&mut connection, &pointer_job_id, &pointer_generation)
                    .await
            })
            .await
        {
            Ok(Some(BusMessage::Unary(value))) => {
                crate::gateway::idempotency::IdempotencyLookup::Replay(value)
            }
            _ => crate::gateway::idempotency::IdempotencyLookup::Miss,
        }
    }

    /// Store an HTTP-gateway response under its request-bound context.
    pub(crate) async fn scoped_idem_store(
        &self,
        context: &crate::gateway::idempotency::IdempotencyContext,
        lifecycle_reference: &LifecycleReference,
        ttl_secs: u64,
    ) {
        let effective_ttl_secs = ttl_secs.clamp(1, 86_400);
        let pointer = ScopedIdempotencyPointer {
            job_id: lifecycle_reference.job_id.clone(),
            generation: lifecycle_reference.generation.clone(),
            request_fingerprint: context.request_fingerprint().into(),
        };
        if let Ok(serialized_pointer) = serde_json::to_vec(&pointer) {
            let job_id = lifecycle_reference.job_id.clone();
            let generation = lifecycle_reference.generation.clone();
            if !matches!(
                self.redis_operation(|mut connection| async move {
                    lifecycle::extend_terminal_retention(
                        &mut connection,
                        &job_id,
                        &generation,
                        effective_ttl_secs,
                    )
                    .await
                })
                .await,
                Ok(true)
            ) {
                return;
            }
            let storage_key = scoped_idempotency_key(context);
            let _ = self
                .redis_operation(|mut connection| async move {
                    lifecycle::scoped_pointer_put(
                        &mut connection,
                        &storage_key,
                        &serialized_pointer,
                        effective_ttl_secs,
                    )
                    .await
                })
                .await;
        }
    }

    /// Liveness check: `PING` Redis (readiness gate for the fleet).
    pub async fn ping(&self) -> bool {
        matches!(
            self.redis_operation(|mut connection| async move {
                redis::cmd("PING")
                    .query_async::<String>(&mut connection)
                    .await
            })
            .await,
            Ok(response) if response == "PONG"
        )
    }

    #[cfg(test)]
    pub(crate) async fn connection_for_test(&self) -> redis::RedisResult<ConnectionManager> {
        ConnectionManager::new(self.client.clone()).await
    }

    /// Waiting-queue depth per provider (for metrics / introspection).
    pub async fn queue_depths(&self, providers: &[String]) -> Vec<(String, usize)> {
        let mut out = Vec::with_capacity(providers.len());
        for p in providers {
            let mut pipeline = redis::pipe();
            pipeline
                .zcard(protocol_queue_key(QueueProtocol::LegacyUnscoped, p))
                .zcard(protocol_queue_key(QueueProtocol::ScopedV1, p))
                .zcard(released_queue_key(QueueProtocol::LegacyUnscoped, p))
                .zcard(released_queue_key(QueueProtocol::ScopedV1, p))
                .zcard(l1_protocol_key(QueueProtocol::LegacyUnscoped, "q", p))
                .zcard(l1_protocol_key(QueueProtocol::ScopedV1, "q", p));
            pipeline
                .zcard(prior_lifecycle_key(QueueProtocol::LegacyUnscoped, "q", p))
                .zcard(prior_lifecycle_key(QueueProtocol::ScopedV1, "q", p));
            let (
                legacy,
                scoped,
                released_unscoped,
                released_scoped,
                l1_unscoped,
                l1_scoped,
                prior_unscoped,
                prior_scoped,
            ): (u64, u64, u64, u64, u64, u64, u64, u64) = self
                .redis_operation(|mut connection| async move {
                    pipeline.query_async(&mut connection).await
                })
                .await
                .ok()
                .unwrap_or_default();
            out.push((
                p.clone(),
                legacy
                    .saturating_add(scoped)
                    .saturating_add(released_unscoped)
                    .saturating_add(released_scoped)
                    .saturating_add(l1_unscoped)
                    .saturating_add(l1_scoped)
                    .saturating_add(prior_unscoped)
                    .saturating_add(prior_scoped) as usize,
            ));
        }
        out
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{:x}-{:x}", self.nonce, n)
    }

    fn aging_step_ms(&self) -> u64 {
        self.config.aging_step.as_millis() as u64
    }

    /// Enqueue a descriptor onto its provider's priority queue, first shedding
    /// with `Overloaded` if the waiting queue is at capacity.
    async fn enqueue_prepared(
        &self,
        prepared_submission: &mut PreparedSubmission,
        origin_deadline: tokio::time::Instant,
    ) -> Result<(), GatewayError> {
        let descriptor = &prepared_submission.descriptor;
        let protocol = prepared_submission.protocol;
        let id = descriptor.id.clone();
        let provider = descriptor.provider.clone();
        let reservation_nonce = prepared_submission.reservation_nonce.clone();
        let serialized_payload = prepared_submission.serialized_payload.clone();
        let stream = descriptor.stream;
        let priority_score =
            deadline_score(descriptor.tier, descriptor.enqueue_ms, self.aging_step_ms());
        let old_keys = [
            released_queue_key(QueueProtocol::LegacyUnscoped, &provider),
            released_processing_key(QueueProtocol::LegacyUnscoped, &provider),
            released_queue_key(QueueProtocol::ScopedV1, &provider),
            released_processing_key(QueueProtocol::ScopedV1, &provider),
            l1_protocol_key(QueueProtocol::LegacyUnscoped, "q", &provider),
            l1_protocol_key(QueueProtocol::LegacyUnscoped, "proc", &provider),
            l1_protocol_key(QueueProtocol::ScopedV1, "q", &provider),
            l1_protocol_key(QueueProtocol::ScopedV1, "proc", &provider),
            prior_lifecycle_key(QueueProtocol::LegacyUnscoped, "q", &provider),
            prior_lifecycle_key(QueueProtocol::LegacyUnscoped, "proc", &provider),
            prior_lifecycle_key(QueueProtocol::ScopedV1, "q", &provider),
            prior_lifecycle_key(QueueProtocol::ScopedV1, "proc", &provider),
        ];
        let limits = self.lifecycle_limits;
        let origin_total_lifetime =
            origin_deadline.saturating_duration_since(tokio::time::Instant::now());
        if origin_total_lifetime.is_zero() {
            return Err(GatewayError::Timeout);
        }
        let origin_lease = self.origin_lease_timeout.min(origin_total_lifetime);
        let origin_token = prepared_submission.origin_token.clone();
        let reserved = self
            .redis_operation(|mut connection| async move {
                match lifecycle::reserve(
                    &mut connection,
                    protocol,
                    &id,
                    &reservation_nonce,
                    &serialized_payload,
                    &provider,
                    stream,
                    priority_score,
                    limits,
                    old_keys.each_ref().map(|key| key.as_str()),
                    origin_lease,
                    origin_total_lifetime,
                    &origin_token,
                )
                .await
                {
                    Err(lifecycle::ReserveFailure::Redis) => Err(redis::RedisError::from((
                        redis::ErrorKind::Io,
                        "distributed lifecycle reserve failed",
                    ))),
                    result => Ok(result),
                }
            })
            .await
            .map_err(|error| redis_err(&error))?
            .map_err(|failure| match failure {
                lifecycle::ReserveFailure::Capacity => {
                    GatewayError::Overloaded(self.config.overloaded_retry_after)
                }
                lifecycle::ReserveFailure::Migration => GatewayError::Upstream(
                    "distributed gateway lifecycle migration is still draining".into(),
                ),
                lifecycle::ReserveFailure::Collision => {
                    GatewayError::Upstream("distributed gateway job identity conflict".into())
                }
                lifecycle::ReserveFailure::Redis => {
                    GatewayError::Upstream("distributed gateway lifecycle unavailable".into())
                }
            })?;
        prepared_submission.member = reserved.member.clone();
        let provider = descriptor.provider.clone();
        let id = descriptor.id.clone();
        let request_timeout = self.config.request_timeout;
        let max_queue_depth = self.config.max_queue_depth;
        let activation = self
            .redis_operation(|mut connection| async move {
                let other_protocol = match protocol {
                    QueueProtocol::LegacyUnscoped => QueueProtocol::ScopedV1,
                    QueueProtocol::ScopedV1 => QueueProtocol::LegacyUnscoped,
                };
                let capacity_keys = [
                    protocol_queue_key(other_protocol, &provider),
                    released_queue_key(QueueProtocol::LegacyUnscoped, &provider),
                    released_queue_key(QueueProtocol::ScopedV1, &provider),
                    l1_protocol_key(QueueProtocol::LegacyUnscoped, "q", &provider),
                    l1_protocol_key(QueueProtocol::ScopedV1, "q", &provider),
                    prior_lifecycle_key(QueueProtocol::LegacyUnscoped, "q", &provider),
                    prior_lifecycle_key(QueueProtocol::ScopedV1, "q", &provider),
                ];
                lifecycle::activate(
                    &mut connection,
                    protocol,
                    &provider,
                    &id,
                    &reserved,
                    priority_score,
                    request_timeout,
                    max_queue_depth,
                    capacity_keys.each_ref().map(|key| key.as_str()),
                )
                .await
            })
            .await;
        match activation {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.cancel_prepared(prepared_submission).await;
                Err(GatewayError::Overloaded(self.config.overloaded_retry_after))
            }
            Err(error) => {
                self.cancel_prepared(prepared_submission).await;
                Err(redis_err(&error))
            }
        }
    }

    #[cfg(test)]
    async fn enqueue(&self, prepared_submission: &PreparedSubmission) -> Result<(), GatewayError> {
        let mut cloned_submission = prepared_submission.clone();
        self.enqueue_prepared(
            &mut cloned_submission,
            tokio::time::Instant::now() + self.config.request_timeout,
        )
        .await
    }

    /// Origin side (unary): enqueue by priority and await the result over the bus.
    pub async fn submit(&self, req: GatewayRequest) -> Result<Value, GatewayError> {
        let prepared = self.prepare_submission(req, false, None)?;
        self.submit_prepared(prepared).await
    }

    pub(crate) fn prepare_with_policy(
        &self,
        req: GatewayRequest,
        stream: bool,
        policy_scope: crate::gateway::attempt::TrustedPolicyScope,
    ) -> Result<PreparedSubmission, GatewayError> {
        self.prepare_submission(req, stream, Some(policy_scope))
    }

    pub(crate) async fn submit_prepared(
        &self,
        prepared: PreparedSubmission,
    ) -> Result<Value, GatewayError> {
        let accepted = self.accept_prepared(prepared, None).await?;
        self.await_accepted(accepted).await
    }

    pub(crate) async fn submit_prepared_with_reference(
        &self,
        prepared: PreparedSubmission,
        logical_deadline: Option<tokio::time::Instant>,
    ) -> Result<(Value, LifecycleReference), GatewayError> {
        let accepted = self.accept_prepared(prepared, logical_deadline).await?;
        self.await_accepted_with_reference(accepted).await
    }

    pub(crate) async fn accept_prepared(
        &self,
        mut prepared: PreparedSubmission,
        logical_deadline: Option<tokio::time::Instant>,
    ) -> Result<AcceptedSubmission, GatewayError> {
        let now = tokio::time::Instant::now();
        let configured_deadline = now
            .checked_add(self.config.request_timeout)
            .unwrap_or(now + Duration::from_secs(6 * 60 * 60));
        let origin_deadline = logical_deadline
            .map(|deadline| deadline.min(configured_deadline))
            .unwrap_or(configured_deadline);
        if origin_deadline <= now {
            return Err(GatewayError::Timeout);
        }
        let channel = protocol_response_channel(prepared.protocol, &prepared.descriptor.id);

        // Subscribe BEFORE enqueue so we can't miss the (fire-and-forget) publish.
        let mut pubsub =
            tokio::time::timeout(self.redis_operation_timeout, self.client.get_async_pubsub())
                .await
                .map_err(|_| {
                    GatewayError::Upstream("distributed response subscription timed out".into())
                })?
                .map_err(|e| redis_err(&e))?;
        tokio::time::timeout(self.redis_operation_timeout, pubsub.subscribe(&channel))
            .await
            .map_err(|_| GatewayError::Upstream("distributed response subscribe timed out".into()))?
            .map_err(|e| redis_err(&e))?;
        self.enqueue_prepared(&mut prepared, origin_deadline)
            .await?;
        let (_, generation) = prepared
            .member
            .rsplit_once(':')
            .ok_or(GatewayError::Shutdown)?;
        let origin_guard = OriginDropGuard {
            sender: self.origin_cancel_sender.clone(),
            command: Some(OriginCancelCommand {
                protocol: prepared.protocol,
                provider: prepared.descriptor.provider.clone(),
                id: prepared.descriptor.id.clone(),
                generation: generation.to_string(),
                member: prepared.member.clone(),
                origin_token: prepared.origin_token.clone(),
            }),
        };
        Ok(AcceptedSubmission {
            prepared,
            pubsub,
            origin_deadline,
            origin_guard,
        })
    }

    pub(crate) async fn await_accepted(
        &self,
        accepted: AcceptedSubmission,
    ) -> Result<Value, GatewayError> {
        self.await_accepted_with_reference(accepted)
            .await
            .map(|(value, _)| value)
    }

    async fn await_accepted_with_reference(
        &self,
        mut accepted: AcceptedSubmission,
    ) -> Result<(Value, LifecycleReference), GatewayError> {
        use futures::StreamExt;
        let Some((_, generation)) = accepted.prepared.member.rsplit_once(':') else {
            return Err(GatewayError::Shutdown);
        };
        let lifecycle_reference = LifecycleReference {
            job_id: accepted.prepared.descriptor.id.clone(),
            generation: generation.into(),
        };
        let mut messages = accepted.pubsub.on_message();
        let heartbeat_interval = (self.origin_lease_timeout / 3).max(Duration::from_millis(1));
        let mut heartbeat = tokio::time::interval(heartbeat_interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat.tick().await;
        loop {
            tokio::select! {
            biased;
            _ = tokio::time::sleep_until(accepted.origin_deadline) => {
                let job_id = accepted.prepared.descriptor.id.clone();
                let generation = lifecycle_reference.generation.clone();
                if let Ok(Some(terminal)) = self
                    .redis_operation(|mut connection| async move {
                        lifecycle::read_terminal(&mut connection, &job_id, &generation).await
                    })
                    .await
                {
                    accepted.origin_guard.disarm();
                    return match terminal {
                        BusMessage::Unary(value) => Ok((value, lifecycle_reference)),
                        BusMessage::Error(error) => Err(GatewayError::Upstream(error)),
                        _ => Err(GatewayError::Upstream("unexpected durable terminal message".into())),
                    };
                }
                let cancel_status = self.cancel_origin(accepted.origin_guard.command.as_ref().unwrap().clone()).await;
                if cancel_status > 0 {
                    accepted.origin_guard.disarm();
                }
                if cancel_status == 3 {
                    let job_id = accepted.prepared.descriptor.id.clone();
                    let generation = lifecycle_reference.generation.clone();
                    if let Ok(Some(terminal)) = self.redis_operation(|mut connection| async move {
                        lifecycle::read_terminal(&mut connection, &job_id, &generation).await
                    }).await {
                        return match terminal {
                            BusMessage::Unary(value) => Ok((value, lifecycle_reference)),
                            BusMessage::Error(error) => Err(GatewayError::Upstream(error)),
                            _ => Err(GatewayError::Timeout),
                        };
                    }
                }
                return Err(GatewayError::Timeout);
            }
            _ = heartbeat.tick() => {
                match self.heartbeat_origin(&accepted.prepared).await {
                    1 => {}
                    -1 => {
                        accepted.origin_guard.disarm();
                        return Err(GatewayError::Upstream("distributed origin canceled".into()));
                    }
                    _ => return Err(GatewayError::Upstream("distributed origin heartbeat unavailable".into())),
                }
            }
            message = messages.next() => match message {
            Some(msg) => {
                let payload: String = msg.get_payload().map_err(|e| redis_err(&e))?;
                match serde_json::from_str::<BusMessage>(&payload) {
                    Ok(BusMessage::Unary(value)) => {
                        accepted.origin_guard.disarm();
                        return Ok((value, lifecycle_reference.clone()));
                    }
                    Ok(BusMessage::Error(error)) => {
                        accepted.origin_guard.disarm();
                        return Err(GatewayError::Upstream(error));
                    }
                    Ok(_) | Err(_) => {
                        if self.cancel_origin(accepted.origin_guard.command.as_ref().unwrap().clone()).await > 0 {
                            accepted.origin_guard.disarm();
                        }
                        return Err(GatewayError::Upstream("invalid distributed response envelope".into()));
                    }
                }
            }
            None => {
                let job_id = accepted.prepared.descriptor.id.clone();
                let generation = lifecycle_reference.generation.clone();
                if let Ok(Some(terminal)) = self
                    .redis_operation(|mut connection| async move {
                        lifecycle::read_terminal(&mut connection, &job_id, &generation).await
                    })
                    .await
                {
                    accepted.origin_guard.disarm();
                    return match terminal {
                        BusMessage::Unary(value) => Ok((value, lifecycle_reference)),
                        BusMessage::Error(error) => Err(GatewayError::Upstream(error)),
                        _ => Err(GatewayError::Upstream(
                            "unexpected durable terminal message".into(),
                        )),
                    };
                }
                let cancel_status = self.cancel_origin(accepted.origin_guard.command.as_ref().unwrap().clone()).await;
                if cancel_status > 0 {
                    accepted.origin_guard.disarm();
                }
                if cancel_status == 3 {
                    let job_id = accepted.prepared.descriptor.id.clone();
                    let generation = lifecycle_reference.generation.clone();
                    if let Ok(Some(terminal)) = self.redis_operation(|mut connection| async move {
                        lifecycle::read_terminal(&mut connection, &job_id, &generation).await
                    }).await {
                        return match terminal {
                            BusMessage::Unary(value) => Ok((value, lifecycle_reference)),
                            BusMessage::Error(error) => Err(GatewayError::Upstream(error)),
                            _ => Err(GatewayError::Shutdown),
                        };
                    }
                }
                return Err(GatewayError::Shutdown);
            }
            }
            }
        }
    }

    async fn heartbeat_origin(&self, prepared: &PreparedSubmission) -> i64 {
        let Some((_, generation)) = prepared.member.rsplit_once(':') else {
            return 0;
        };
        let id = prepared.descriptor.id.clone();
        let member = prepared.member.clone();
        let generation = generation.to_string();
        let origin_token = prepared.origin_token.clone();
        let origin_lease = self.origin_lease_timeout;
        self.redis_operation(|mut connection| async move {
            lifecycle::heartbeat_origin(
                &mut connection,
                &id,
                &generation,
                &member,
                &origin_token,
                origin_lease,
            )
            .await
        })
        .await
        .unwrap_or_default()
    }

    /// Origin side (streaming): enqueue by priority and return a channel of raw
    /// provider chunks routed back over the bus.
    pub async fn submit_stream(
        &self,
        req: GatewayRequest,
    ) -> Result<mpsc::Receiver<StreamChunk>, GatewayError> {
        let prepared = self.prepare_submission(req, true, None)?;
        self.submit_stream_prepared(prepared).await
    }

    pub(crate) async fn submit_stream_prepared(
        &self,
        prepared: PreparedSubmission,
    ) -> Result<mpsc::Receiver<StreamChunk>, GatewayError> {
        let accepted = self.accept_prepared(prepared, None).await?;
        Ok(self.stream_accepted(accepted))
    }

    pub(crate) fn stream_accepted(
        &self,
        mut accepted: AcceptedSubmission,
    ) -> mpsc::Receiver<StreamChunk> {
        let (chunk_tx, chunk_rx) = mpsc::channel(16);
        let origin_deadline = accepted.origin_deadline;
        let heartbeat_interval = (self.origin_lease_timeout / 3).max(Duration::from_millis(1));
        let connections = self.connections.clone();
        let redis_operation_timeout = self.redis_operation_timeout;
        let origin_lease = self.origin_lease_timeout;

        tokio::spawn(async move {
            use futures::StreamExt;
            let mut messages = accepted.pubsub.on_message();
            let mut heartbeat = tokio::time::interval(heartbeat_interval);
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            heartbeat.tick().await;
            loop {
                tokio::select! {
                    biased;
                    _ = chunk_tx.closed() => {
                        if cancel_origin_with_connections(
                            &connections,
                            redis_operation_timeout,
                            accepted.origin_guard.command.as_ref().unwrap().clone(),
                        ).await {
                            accepted.origin_guard.disarm();
                        }
                        return;
                    }
                    _ = tokio::time::sleep_until(origin_deadline) => {
                        if cancel_origin_with_connections(
                            &connections,
                            redis_operation_timeout,
                            accepted.origin_guard.command.as_ref().unwrap().clone(),
                        ).await {
                            accepted.origin_guard.disarm();
                        }
                        let _ = chunk_tx.send(Err(GatewayError::Timeout)).await;
                        return;
                    }
                    _ = heartbeat.tick() => {
                        let prepared = &accepted.prepared;
                        let Some((_, generation)) = prepared.member.rsplit_once(':') else { return; };
                        let id = prepared.descriptor.id.clone();
                        let member = prepared.member.clone();
                        let generation = generation.to_string();
                        let origin_token = prepared.origin_token.clone();
                        let status = run_redis_operation(
                            &connections,
                            redis_operation_timeout,
                            |mut connection| async move {
                                lifecycle::heartbeat_origin(
                                    &mut connection,
                                    &id,
                                    &generation,
                                    &member,
                                    &origin_token,
                                    origin_lease,
                                ).await
                            },
                        ).await.unwrap_or_default();
                        if status != 1 {
                            if status == -1 {
                                accepted.origin_guard.disarm();
                            }
                            let _ = chunk_tx.send(Err(GatewayError::Upstream(
                                if status == -1 { "distributed origin canceled" } else { "distributed origin heartbeat unavailable" }.into()
                            ))).await;
                            return;
                        }
                    }
                    message = messages.next() => {
                        let Some(message) = message else {
                            if cancel_origin_with_connections(
                                &connections,
                                redis_operation_timeout,
                                accepted.origin_guard.command.as_ref().unwrap().clone(),
                            ).await {
                                accepted.origin_guard.disarm();
                            }
                            let _ = chunk_tx.send(Err(GatewayError::Upstream(
                                "distributed response channel lost".into(),
                            ))).await;
                            return;
                        };
                        let payload: String = match message.get_payload() {
                            Ok(payload) => payload,
                            Err(_) => {
                                let _ = chunk_tx.send(Err(GatewayError::Upstream(
                                    "distributed response channel lost".into(),
                                ))).await;
                                return;
                            }
                        };
                        match serde_json::from_str::<BusMessage>(&payload) {
                            Ok(BusMessage::Chunk(chunk)) => {
                                tokio::select! {
                                    biased;
                                    _ = chunk_tx.closed() => continue,
                                    _ = tokio::time::sleep_until(origin_deadline) => continue,
                                    sent = chunk_tx.send(Ok(chunk)) => {
                                        if sent.is_err() { continue; }
                                    }
                                }
                            }
                            Ok(BusMessage::End) => {
                                accepted.origin_guard.disarm();
                                return;
                            }
                            Ok(BusMessage::Error(error)) => {
                                accepted.origin_guard.disarm();
                                let _ = chunk_tx.send(Err(GatewayError::Upstream(error))).await;
                                return;
                            }
                            Ok(BusMessage::Unary(_)) | Err(_) => {
                                let _ = chunk_tx.send(Err(GatewayError::Upstream(
                                    "distributed response channel lost".into(),
                                ))).await;
                                return;
                            }
                        }
                    }
                }
            }
        });

        chunk_rx
    }

    fn prepare_submission(
        &self,
        req: GatewayRequest,
        stream: bool,
        policy_scope: Option<crate::gateway::attempt::TrustedPolicyScope>,
    ) -> Result<PreparedSubmission, GatewayError> {
        let descriptor = JobDescriptor {
            id: self.next_id(),
            provider: req.provider,
            tier: req.tier,
            permits: req.permits.max(1),
            payload: req.payload,
            policy_envelope_version: 1,
            trusted_unscoped: policy_scope.is_none(),
            policy_scope,
            stream,
            enqueue_ms: now_ms(),
        };
        PreparedSubmission::new(descriptor)
    }

    /// TTL for the done / attempts markers — a few lease windows, long enough to
    /// outlast redelivery but short enough not to leak keys.
    fn marker_ttl_secs(&self) -> u64 {
        (self.config.lease_timeout.as_secs() * 3).max(60)
    }

    async fn is_done(&self, protocol: QueueProtocol, id: &str) -> Option<bool> {
        let key = protocol_done_key(protocol, id);
        self.redis_operation(|mut connection| async move { connection.exists(key).await })
            .await
            .ok()
    }

    /// Increment and return this job's delivery-attempt count.
    async fn bump_attempts(&self, protocol: QueueProtocol, id: &str) -> Option<u32> {
        let key = protocol_attempts_key(protocol, id);
        let mut invocation = self.bump_attempts_script.key(key);
        invocation.arg(self.marker_ttl_secs());
        self.redis_operation(|mut connection| async move {
            invocation.invoke_async::<u32>(&mut connection).await
        })
        .await
        .ok()
    }

    /// Number of dead-lettered jobs for a provider (introspection).
    pub async fn dead_letter_len(&self, provider: &str) -> usize {
        let mut pipeline = redis::pipe();
        pipeline
            .zcard(protocol_dlq_key(QueueProtocol::LegacyUnscoped, provider))
            .zcard(protocol_dlq_key(QueueProtocol::ScopedV1, provider))
            .llen(released_dlq_key(QueueProtocol::LegacyUnscoped, provider))
            .llen(released_dlq_key(QueueProtocol::ScopedV1, provider));
        let (legacy, scoped, released_unscoped, released_scoped): (u64, u64, u64, u64) = self
            .redis_operation(
                |mut connection| async move { pipeline.query_async(&mut connection).await },
            )
            .await
            .ok()
            .unwrap_or_default();
        legacy
            .saturating_add(scoped)
            .saturating_add(released_unscoped)
            .saturating_add(released_scoped) as usize
    }

    async fn cancel_prepared(&self, prepared: &PreparedSubmission) {
        let Some((_, generation)) = prepared.member.rsplit_once(':') else {
            return;
        };
        let _ = self
            .cancel_origin(OriginCancelCommand {
                protocol: prepared.protocol,
                provider: prepared.descriptor.provider.clone(),
                id: prepared.descriptor.id.clone(),
                generation: generation.to_string(),
                member: prepared.member.clone(),
                origin_token: prepared.origin_token.clone(),
            })
            .await;
    }

    async fn cancel_origin(&self, command: OriginCancelCommand) -> i64 {
        self.redis_operation(|mut connection| async move {
            lifecycle::cancel(
                &mut connection,
                command.protocol,
                &command.provider,
                &command.id,
                &command.generation,
                &command.member,
                Duration::from_secs(10 * 60),
                &command.origin_token,
            )
            .await
        })
        .await
        .unwrap_or_default()
    }

    /// Spawn one worker loop per provider plus a reaper for redelivery.
    pub fn spawn_workers(self: &Arc<Self>, providers: Vec<String>) -> Vec<JoinHandle<()>> {
        let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(providers.len() * 2 + 1);
        for provider in &providers {
            let preparation_capacity = Arc::new(Semaphore::new(
                self.config.max_concurrency_per_provider.max(1),
            ));
            for protocol in [QueueProtocol::LegacyUnscoped, QueueProtocol::ScopedV1] {
                let me = self.clone();
                let provider = provider.clone();
                let preparation_capacity = preparation_capacity.clone();
                handles.push(tokio::spawn(async move {
                    me.worker(provider, protocol, preparation_capacity).await
                }));
            }
        }
        let me = self.clone();
        handles.push(tokio::spawn(async move { me.reaper(providers).await }));
        handles
    }

    async fn worker(
        self: Arc<Self>,
        provider: String,
        protocol: QueueProtocol,
        preparation_capacity: Arc<Semaphore>,
    ) {
        const IDLE_POLL: Duration = Duration::from_millis(50);
        let qkey = protocol_queue_key(protocol, &provider);
        let pkey = protocol_processing_key(protocol, &provider);
        let lkey = protocol_leased_key(protocol, &provider);
        let rate_key = RateKey::provider(provider.clone());

        loop {
            let preparation_permit = match preparation_capacity.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };
            let owner_token = uuid::Uuid::new_v4().simple().to_string();
            let lease_duration_ms = duration_millis_u64(self.config.lease_timeout);
            let mut lease_invocation = self.lease.key(&qkey);
            lease_invocation
                .key(&pkey)
                .key(&lkey)
                .key(protocol_owners_key(protocol, &provider))
                .key(lifecycle::counters())
                .key(lifecycle::terminal_reservations())
                .key(lifecycle::reservation_states())
                .key(lifecycle::expiry())
                .arg(lease_duration_ms)
                .arg(&owner_token)
                .arg(self.lifecycle_limits.max_terminal_bytes)
                .arg(self.lifecycle_limits.unary_terminal_bytes)
                .arg(self.lifecycle_limits.stream_terminal_bytes)
                .arg(lifecycle::job_prefix())
                .arg(duration_millis_u64(
                    self.worker_job_timeout
                        .saturating_add(Duration::from_secs(60 * 60)),
                ))
                .arg(duration_millis_u64(self.worker_job_timeout));
            let leased: Option<(String, String, String)> = match self
                .redis_operation(|mut connection| async move {
                    lease_invocation.invoke_async(&mut connection).await
                })
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    drop(preparation_permit);
                    eprintln!("gateway worker[{provider}]: lease error: {error}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let Some((member, score, serialized_payload)) = leased else {
                drop(preparation_permit);
                tokio::time::sleep(IDLE_POLL).await; // queue empty
                continue;
            };
            let ownership = LeaseOwnership {
                protocol,
                provider: provider.clone(),
                member,
                original_score: score,
                owner_token,
                serialized_payload,
            };
            let me = self.clone();
            let rate_key = rate_key.clone();
            tokio::spawn(async move {
                let _preparation_permit = preparation_permit;
                me.execute_leased_job(ownership, rate_key).await;
            });
        }
    }

    async fn execute_leased_job(&self, ownership: LeaseOwnership, rate_key: RateKey) {
        enum LeasedJobOutcome {
            Finished,
            OwnershipLost,
            Canceled,
            DeadlineExceeded,
        }

        let mut job = Box::pin(self.process_leased_job(&ownership, rate_key));
        let mut heartbeat = Box::pin(self.heartbeat_lease(&ownership));
        let mut deadline = Box::pin(tokio::time::sleep(self.worker_job_timeout));
        let outcome = tokio::select! {
            biased;
            _ = &mut job => LeasedJobOutcome::Finished,
            heartbeat_end = &mut heartbeat => match heartbeat_end {
                LeaseHeartbeatEnd::Lost => LeasedJobOutcome::OwnershipLost,
                LeaseHeartbeatEnd::Canceled => LeasedJobOutcome::Canceled,
            },
            _ = &mut deadline => LeasedJobOutcome::DeadlineExceeded,
        };

        match outcome {
            LeasedJobOutcome::Finished => {
                drop(job);
                drop(heartbeat);
                drop(deadline);
            }
            LeasedJobOutcome::OwnershipLost => {
                drop(job);
                drop(heartbeat);
                drop(deadline);
                eprintln!(
                    "gateway worker[{}]: lease ownership lost; abandoning local work",
                    ownership.provider
                );
            }
            LeasedJobOutcome::Canceled => {
                drop(job);
                drop(heartbeat);
                drop(deadline);
                if let Ok(descriptor) =
                    serde_json::from_str::<JobDescriptor>(&ownership.serialized_payload)
                {
                    let _ = self
                        .terminal_owned(
                            &ownership,
                            &descriptor.id,
                            descriptor.stream,
                            &BusMessage::Error("distributed origin canceled".into()),
                        )
                        .await;
                }
            }
            LeasedJobOutcome::DeadlineExceeded => {
                drop(job);
                drop(deadline);
                let mut terminal = Box::pin(self.deadline_cleanup_owned(&ownership));
                tokio::select! {
                    biased;
                    _ = &mut terminal => {}
                    _ = &mut heartbeat => {
                        eprintln!(
                            "gateway worker[{}]: lease ownership lost during deadline cleanup",
                            ownership.provider
                        );
                    }
                }
                drop(terminal);
                drop(heartbeat);
            }
        }
    }

    async fn heartbeat_lease(&self, ownership: &LeaseOwnership) -> LeaseHeartbeatEnd {
        let heartbeat_interval = (self.config.lease_timeout / 3).max(Duration::from_millis(1));
        loop {
            tokio::time::sleep(heartbeat_interval).await;
            match self.refresh_owned(ownership).await {
                1 => {}
                -1 => return LeaseHeartbeatEnd::Canceled,
                _ => return LeaseHeartbeatEnd::Lost,
            }
        }
    }

    async fn process_leased_job(&self, ownership: &LeaseOwnership, rate_key: RateKey) {
        let descriptor: JobDescriptor = match serde_json::from_str(&ownership.serialized_payload) {
            Ok(descriptor) => descriptor,
            Err(_) => {
                let _ = self.ack_owned(ownership).await;
                return;
            }
        };

        if !descriptor_matches_protocol(ownership.protocol, &descriptor) {
            let _ = self
                .terminal_owned(
                    ownership,
                    &descriptor.id,
                    descriptor.stream,
                    &BusMessage::Error("llmshim-coordinator-unavailable".into()),
                )
                .await;
            return;
        }

        match self.is_done(ownership.protocol, &descriptor.id).await {
            Some(true) => {
                let _ = self.ack_owned(ownership).await;
                return;
            }
            Some(false) => {}
            None => return,
        }

        let Some(delivery_attempts) = self.bump_attempts(ownership.protocol, &descriptor.id).await
        else {
            return;
        };
        if delivery_attempts > self.config.max_attempts {
            eprintln!(
                "gateway worker[{}]: dead-lettering job {} after {delivery_attempts} attempts",
                ownership.provider, descriptor.id
            );
            if self.dead_letter_owned(ownership).await {
                crate::gateway::metrics::incr(
                    crate::gateway::metrics::REJECTED,
                    &[
                        ("provider", ownership.provider.as_str()),
                        ("reason", "dead_letter"),
                    ],
                );
            }
            return;
        }

        let policy_gated = descriptor.policy_scope.is_some();
        let rate_admission = if policy_gated {
            Ok(())
        } else {
            self.limiter.acquire(&rate_key, descriptor.permits).await
        };
        if let Err(RetryAfter(wait)) = rate_admission {
            tokio::time::sleep(wait.min(Duration::from_millis(500))).await;
            let _ = self.release_owned(ownership).await;
            return;
        }

        match self.refresh_owned(ownership).await {
            1 => {}
            -1 => {
                let _ = self
                    .terminal_owned(
                        ownership,
                        &descriptor.id,
                        descriptor.stream,
                        &BusMessage::Error("distributed origin canceled".into()),
                    )
                    .await;
                return;
            }
            _ => return,
        }

        self.run_and_publish(ownership, descriptor).await;
    }

    /// Dispatch a leased job and terminate it only while this delivery still owns
    /// the lease.
    async fn run_and_publish(&self, ownership: &LeaseOwnership, desc: JobDescriptor) {
        use crate::gateway::metrics;
        let channel = protocol_response_channel(ownership.protocol, &desc.id);
        let provider = desc.provider.clone();
        let plabels: &[(&str, &str)] = &[("provider", &provider)];
        let _inflight = metrics::inflight(&provider);
        metrics::observe_ms(
            metrics::QUEUE_WAIT,
            plabels,
            now_ms().saturating_sub(desc.enqueue_ms) as f64,
        );
        let started_at = now_ms();

        let policy_context = match (&desc.policy_scope, &self.attempt_coordinator) {
            (Some(scope), Some(coordinator)) => Some(coordinator.context(scope.clone())),
            (Some(_), None) => {
                let _ = self
                    .terminal_owned(
                        ownership,
                        &desc.id,
                        desc.stream,
                        &BusMessage::Error("llmshim-coordinator-unavailable".into()),
                    )
                    .await;
                return;
            }
            (None, _) => None,
        };

        let policy_gated = desc.policy_scope.is_some();
        if desc.stream {
            let opened = match policy_context {
                Some(context) => {
                    self.dispatch
                        .dispatch_stream_with_policy(&provider, desc.payload, context)
                        .await
                }
                None => self.dispatch.dispatch_stream(&provider, desc.payload).await,
            };
            match opened {
                Ok(mut upstream) => {
                    metrics::incr(metrics::DISPATCHED, plabels);
                    use futures::StreamExt;
                    while let Some(item) = upstream.next().await {
                        match item {
                            Ok(chunk) => {
                                if !self
                                    .publish_owned(ownership, &channel, &BusMessage::Chunk(chunk))
                                    .await
                                {
                                    let _ = self
                                        .terminal_owned(
                                            ownership,
                                            &desc.id,
                                            true,
                                            &BusMessage::Error(
                                                "distributed response publication failed".into(),
                                            ),
                                        )
                                        .await;
                                    return;
                                }
                            }
                            Err(error) => {
                                let _ = self
                                    .terminal_owned(
                                        ownership,
                                        &desc.id,
                                        true,
                                        &BusMessage::Error(error.to_string()),
                                    )
                                    .await;
                                return;
                            }
                        }
                    }
                    if !self
                        .terminal_owned(ownership, &desc.id, true, &BusMessage::End)
                        .await
                    {
                        return;
                    }
                    metrics::observe_ms(
                        metrics::UPSTREAM_LATENCY,
                        plabels,
                        now_ms().saturating_sub(started_at) as f64,
                    );
                }
                Err(err) => {
                    metrics::incr(
                        metrics::REJECTED,
                        &[("provider", &provider), ("reason", "upstream")],
                    );
                    if !policy_gated {
                        self.penalize_if_429(&provider, &err).await;
                    }
                    let _ = self
                        .terminal_owned(ownership, &desc.id, true, &BusMessage::Error(err.message))
                        .await;
                }
            }
        } else {
            let dispatched = match policy_context {
                Some(context) => {
                    self.dispatch
                        .dispatch_with_policy(&provider, desc.payload, context)
                        .await
                }
                None => self.dispatch.dispatch(&provider, desc.payload).await,
            };
            let msg = match dispatched {
                Ok(value) => {
                    metrics::incr(metrics::DISPATCHED, plabels);
                    metrics::observe_ms(
                        metrics::UPSTREAM_LATENCY,
                        plabels,
                        now_ms().saturating_sub(started_at) as f64,
                    );
                    BusMessage::Unary(value)
                }
                Err(err) => {
                    metrics::incr(
                        metrics::REJECTED,
                        &[("provider", &provider), ("reason", "upstream")],
                    );
                    if !policy_gated {
                        self.penalize_if_429(&provider, &err).await;
                    }
                    BusMessage::Error(err.message)
                }
            };
            let _ = self.terminal_owned(ownership, &desc.id, false, &msg).await;
        }
    }

    async fn deadline_cleanup_owned(&self, ownership: &LeaseOwnership) -> bool {
        #[cfg(test)]
        if !self.terminal_operation_delay.is_zero() {
            tokio::time::sleep(self.terminal_operation_delay).await;
        }
        let Ok(descriptor) = serde_json::from_str::<JobDescriptor>(&ownership.serialized_payload)
        else {
            return self.ack_owned(ownership).await;
        };
        let message = BusMessage::Error("distributed worker deadline exceeded".into());
        self.terminal_owned(ownership, &descriptor.id, descriptor.stream, &message)
            .await
    }

    async fn terminal_owned(
        &self,
        ownership: &LeaseOwnership,
        id: &str,
        stream: bool,
        message: &BusMessage,
    ) -> bool {
        let maximum_bytes = if stream {
            self.lifecycle_limits.stream_terminal_bytes
        } else {
            self.lifecycle_limits.unary_terminal_bytes
        } as usize;
        let payload = match lifecycle::serialize_bounded(message, maximum_bytes) {
            Ok(payload) => payload,
            Err(_) => match lifecycle::serialize_bounded(
                &BusMessage::Error("normalized response exceeds distributed terminal limit".into()),
                maximum_bytes,
            ) {
                Ok(payload) => payload,
                Err(_) => return false,
            },
        };
        let Some((_, generation)) = ownership.id_and_generation() else {
            return false;
        };
        let channel = protocol_response_channel(ownership.protocol, id);
        let mut invocation = self.terminal_publish_complete.key(protocol_processing_key(
            ownership.protocol,
            &ownership.provider,
        ));
        invocation
            .key(protocol_leased_key(ownership.protocol, &ownership.provider))
            .key(protocol_owners_key(ownership.protocol, &ownership.provider))
            .key(protocol_done_key(ownership.protocol, id))
            .key(lifecycle::counters())
            .key(lifecycle::meta_key(id))
            .key(lifecycle::terminal_key(id))
            .key(lifecycle::expiry())
            .key(lifecycle::payload_key(id))
            .key(lifecycle::terminal_reservations())
            .key(lifecycle::reservation_states())
            .key(lifecycle::origin_expiry())
            .arg(&ownership.member)
            .arg(&ownership.owner_token)
            .arg(10 * 60_u64)
            .arg(channel)
            .arg(payload)
            .arg(generation)
            .arg(60 * 60 * 1000_u64)
            .arg(
                serde_json::to_vec(&BusMessage::Error("distributed origin canceled".into()))
                    .expect("fixed cancellation envelope serializes"),
            );
        matches!(
            self.redis_operation(|mut connection| async move {
                invocation.invoke_async::<i64>(&mut connection).await
            })
            .await,
            Ok(1)
        )
    }

    async fn publish_owned(
        &self,
        ownership: &LeaseOwnership,
        channel: &str,
        message: &BusMessage,
    ) -> bool {
        let Some((id, generation)) = ownership.id_and_generation() else {
            return false;
        };
        let Ok(payload) = serde_json::to_string(message) else {
            return false;
        };
        let mut invocation = self.fenced_publish.key(protocol_processing_key(
            ownership.protocol,
            &ownership.provider,
        ));
        invocation
            .key(protocol_owners_key(ownership.protocol, &ownership.provider))
            .key(lifecycle::meta_key(id))
            .arg(&ownership.member)
            .arg(&ownership.owner_token)
            .arg(channel)
            .arg(payload)
            .arg(generation);
        matches!(
            self.redis_operation(|mut connection| async move {
                invocation.invoke_async::<i64>(&mut connection).await
            })
            .await,
            Ok(subscriber_count) if subscriber_count >= 0
        )
    }

    async fn ack_owned(&self, ownership: &LeaseOwnership) -> bool {
        let mut invocation = self.ack.key(protocol_processing_key(
            ownership.protocol,
            &ownership.provider,
        ));
        invocation
            .key(protocol_leased_key(ownership.protocol, &ownership.provider))
            .key(protocol_owners_key(ownership.protocol, &ownership.provider))
            .arg(&ownership.member)
            .arg(&ownership.owner_token);
        matches!(
            self.redis_operation(|mut connection| async move {
                invocation.invoke_async::<i64>(&mut connection).await
            })
            .await,
            Ok(1)
        )
    }

    async fn refresh_owned(&self, ownership: &LeaseOwnership) -> i64 {
        let Some((id, generation)) = ownership.id_and_generation() else {
            return 0;
        };
        let lease_duration_ms = duration_millis_u64(self.config.lease_timeout);
        let mut invocation = self.refresh.key(protocol_processing_key(
            ownership.protocol,
            &ownership.provider,
        ));
        invocation
            .key(protocol_owners_key(ownership.protocol, &ownership.provider))
            .key(lifecycle::meta_key(id))
            .arg(&ownership.member)
            .arg(&ownership.owner_token)
            .arg(lease_duration_ms)
            .arg(generation);
        self.redis_operation(|mut connection| async move {
            invocation.invoke_async::<i64>(&mut connection).await
        })
        .await
        .unwrap_or_default()
    }

    async fn release_owned(&self, ownership: &LeaseOwnership) -> bool {
        let Some((id, generation)) = ownership.id_and_generation() else {
            return false;
        };
        let mut invocation = self
            .release
            .key(protocol_queue_key(ownership.protocol, &ownership.provider));
        invocation
            .key(protocol_processing_key(
                ownership.protocol,
                &ownership.provider,
            ))
            .key(protocol_leased_key(ownership.protocol, &ownership.provider))
            .key(protocol_owners_key(ownership.protocol, &ownership.provider))
            .key(lifecycle::counters())
            .key(lifecycle::meta_key(id))
            .key(lifecycle::terminal_reservations())
            .key(lifecycle::reservation_states())
            .key(lifecycle::origin_expiry())
            .arg(&ownership.member)
            .arg(&ownership.original_score)
            .arg(&ownership.owner_token)
            .arg(generation);
        matches!(
            self.redis_operation(|mut connection| async move {
                invocation.invoke_async::<i64>(&mut connection).await
            })
            .await,
            Ok(1)
        )
    }

    #[cfg(test)]
    async fn complete_owned(&self, ownership: &LeaseOwnership, id: &str) -> bool {
        let mut invocation = self.complete.key(protocol_processing_key(
            ownership.protocol,
            &ownership.provider,
        ));
        invocation
            .key(protocol_leased_key(ownership.protocol, &ownership.provider))
            .key(protocol_owners_key(ownership.protocol, &ownership.provider))
            .key(protocol_done_key(ownership.protocol, id))
            .arg(&ownership.member)
            .arg(&ownership.owner_token)
            .arg(self.marker_ttl_secs());
        matches!(
            self.redis_operation(|mut connection| async move {
                invocation.invoke_async::<i64>(&mut connection).await
            })
            .await,
            Ok(1)
        )
    }

    async fn dead_letter_owned(&self, ownership: &LeaseOwnership) -> bool {
        let Some((id, generation)) = ownership.id_and_generation() else {
            return false;
        };
        let mut invocation = self.dead_letter_transition.key(protocol_processing_key(
            ownership.protocol,
            &ownership.provider,
        ));
        invocation
            .key(protocol_leased_key(ownership.protocol, &ownership.provider))
            .key(protocol_owners_key(ownership.protocol, &ownership.provider))
            .key(protocol_dlq_key(ownership.protocol, &ownership.provider))
            .key(lifecycle::counters())
            .key(lifecycle::meta_key(id))
            .key(lifecycle::expiry())
            .key(lifecycle::reservations())
            .key(lifecycle::payload_key(id))
            .key(lifecycle::terminal_key(id))
            .key(lifecycle::terminal_reservations())
            .key(lifecycle::dlq_reservations())
            .key(lifecycle::reservation_states())
            .key(lifecycle::origin_expiry())
            .arg(&ownership.member)
            .arg(&ownership.owner_token)
            .arg(generation)
            .arg(24 * 60 * 60 * 1000_u64)
            .arg(1_000_u64)
            .arg(64 * 1024 * 1024_u64)
            .arg(10 * 60 * 1000_u64)
            .arg(protocol_response_channel(ownership.protocol, id))
            .arg(
                serde_json::to_vec(&BusMessage::Error(
                    "distributed job moved to dead letter retention".into(),
                ))
                .expect("fixed dead-letter envelope serializes"),
            )
            .arg(
                serde_json::to_vec(&BusMessage::Error("distributed origin canceled".into()))
                    .expect("fixed cancellation envelope serializes"),
            )
            .arg(60 * 60 * 1000_u64);
        matches!(
            self.redis_operation(|mut connection| async move {
                invocation.invoke_async::<i64>(&mut connection).await
            })
            .await,
            Ok(1)
        )
    }

    async fn penalize_if_429(&self, provider: &str, err: &super::DispatchError) {
        if let Some(retry_after) = err.retry_after {
            self.limiter
                .penalize(&RateKey::provider(provider.to_string()), retry_after)
                .await;
        }
    }

    /// Run the reaper loop: periodically requeue leases whose visibility deadline
    /// has passed (a crashed or stuck worker).
    async fn reaper(self: Arc<Self>, providers: Vec<String>) {
        let interval = (self.config.lease_timeout / 3)
            .min(self.origin_lease_timeout / 3)
            .max(Duration::from_millis(1));
        loop {
            tokio::time::sleep(interval).await;
            let _ = self
                .redis_operation(|mut connection| async move {
                    lifecycle::expire_origins(&mut connection, 128, Duration::from_secs(10 * 60))
                        .await
                })
                .await;
            let _ = self
                .redis_operation(|mut connection| async move {
                    lifecycle::cleanup_expired(&mut connection, 128).await
                })
                .await;
            let _ = self
                .redis_operation(|mut connection| async move {
                    lifecycle::cleanup_caches(&mut connection, 128).await
                })
                .await;
            for provider in &providers {
                for protocol in [QueueProtocol::LegacyUnscoped, QueueProtocol::ScopedV1] {
                    let reaped = self.reap_once_protocol(protocol, provider).await;
                    if reaped > 0 {
                        eprintln!(
                            "gateway reaper[{provider}]: redelivered {reaped} expired lease(s)"
                        );
                    }
                }
            }
        }
    }

    /// Requeue any leases for `provider` whose visibility deadline has passed.
    /// Returns how many were redelivered. Public so a fleet can also drive
    /// reaping from an external scheduler.
    pub async fn reap_once(&self, provider: &str) -> i64 {
        let legacy = self
            .reap_once_protocol(QueueProtocol::LegacyUnscoped, provider)
            .await;
        let scoped = self
            .reap_once_protocol(QueueProtocol::ScopedV1, provider)
            .await;
        legacy.saturating_add(scoped)
    }

    async fn reap_once_protocol(&self, protocol: QueueProtocol, provider: &str) -> i64 {
        let mut invocation = self.reap.key(protocol_processing_key(protocol, provider));
        invocation
            .key(protocol_queue_key(protocol, provider))
            .key(protocol_leased_key(protocol, provider))
            .key(protocol_owners_key(protocol, provider))
            .key(lifecycle::counters())
            .key(lifecycle::expiry())
            .key(lifecycle::terminal_reservations())
            .key(lifecycle::reservation_states())
            .key(lifecycle::origin_expiry())
            .arg(256)
            .arg(lifecycle::job_prefix())
            .arg(10 * 60 * 1000_u64)
            .arg(
                serde_json::to_vec(&BusMessage::Error("distributed origin canceled".into()))
                    .expect("fixed cancellation envelope serializes"),
            )
            .arg(60 * 60 * 1000_u64);
        match self
            .redis_operation(|mut connection| async move {
                invocation.invoke_async(&mut connection).await
            })
            .await
        {
            Ok(reaped) => reaped,
            Err(error) => {
                eprintln!("gateway reaper[{provider}]: Redis transition failed: {error}");
                0
            }
        }
    }
}

fn positive_duration_from_env(name: &str, fallback: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|milliseconds| *milliseconds > 0)
        .map(Duration::from_millis)
        .unwrap_or(fallback)
}

async fn origin_cancellation_pump(
    connections: Arc<crate::redis_operation::RedisConnectionManagerCache>,
    timeout: Duration,
    mut receiver: mpsc::Receiver<OriginCancelCommand>,
) {
    while let Some(command) = receiver.recv().await {
        let _ = cancel_origin_with_connections(&connections, timeout, command).await;
    }
}

async fn cancel_origin_with_connections(
    connections: &crate::redis_operation::RedisConnectionManagerCache,
    timeout: Duration,
    command: OriginCancelCommand,
) -> bool {
    matches!(
        run_redis_operation(connections, timeout, |mut connection| async move {
            lifecycle::cancel(
                &mut connection,
                command.protocol,
                &command.provider,
                &command.id,
                &command.generation,
                &command.member,
                Duration::from_secs(10 * 60),
                &command.origin_token,
            )
            .await
        })
        .await,
        Ok(status) if status > 0
    )
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

async fn run_redis_operation<T, F, Fut>(
    connections: &crate::redis_operation::RedisConnectionManagerCache,
    timeout: Duration,
    operation: F,
) -> redis::RedisResult<T>
where
    F: FnOnce(ConnectionManager) -> Fut,
    Fut: std::future::Future<Output = redis::RedisResult<T>>,
{
    tokio::time::timeout(timeout, connections.run(operation))
        .await
        .map_err(|_| redis::RedisError::from((redis::ErrorKind::Io, "Redis operation timed out")))?
}

/// Wrap any error as a gateway upstream error (redis failures fail the request,
/// not the process).
fn redis_err(e: &dyn std::fmt::Display) -> GatewayError {
    GatewayError::Upstream(format!("distributed gateway: {e}"))
}

/// Fleet-wide spend ledger: one counter per `(tenant, window)` in the same
/// Redis the queue uses, so a dollar cap is global rather than per replica.
/// A Redis failure loses the charge rather than the request — the same
/// fail-open posture the shared rate limiter takes.
#[async_trait::async_trait]
impl crate::gateway::quota::SpendStore for DistributedGateway {
    async fn spent(&self, tenant: &str, window: Duration) -> f64 {
        let key = spend_key(tenant, window);
        let raw: Option<String> = self
            .redis_operation(|mut connection| async move { connection.get(key).await })
            .await
            .ok()
            .flatten();
        raw.and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0)
    }

    async fn record(&self, tenant: &str, window: Duration, usd: f64) {
        let key = spend_key(tenant, window);
        // Two windows of slack so a late charge still lands on its own window.
        let ttl = window.as_millis().saturating_mul(2).min(u64::MAX as u128) as u64;
        let _ = self
            .redis_operation(|mut connection| async move {
                let _: f64 = connection.incr(&key, usd).await?;
                let _: bool = connection.pexpire(&key, ttl as i64).await?;
                redis::RedisResult::Ok(())
            })
            .await;
    }
}

fn spend_key(tenant: &str, window: Duration) -> String {
    format!(
        "llmshim:spend:{tenant}:{}",
        crate::gateway::quota::window_index(window)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP: u64 = 5000; // aging_step ms

    #[test]
    fn deadline_orders_by_tier_then_age() {
        // Higher tier → smaller (earlier) deadline → dispatched first (ZPOPMIN).
        assert!(deadline_score(5, 1000, STEP) < deadline_score(4, 1000, STEP));
        assert!(deadline_score(1, 1000, STEP) < deadline_score(0, 1000, STEP));
        // Within a tier, older (smaller enqueue_ms) → smaller deadline (FIFO).
        assert!(deadline_score(3, 1000, STEP) < deadline_score(3, 2000, STEP));
    }

    #[test]
    fn aging_lets_old_low_tier_overtake_new_high_tier() {
        // A tier-0 job enqueued long enough before a tier-5 job wins: its
        // deadline (1_000_000) is earlier than the tier-5 deadline
        // (1_030_000 - 5*5000 = 1_005_000).
        let old_low = deadline_score(0, 1_000_000, STEP);
        let new_high = deadline_score(5, 1_030_000, STEP);
        assert!(
            old_low < new_high,
            "aged low tier should overtake fresh high tier"
        );
        // But a *recent* low tier does not.
        let recent_low = deadline_score(0, 1_029_000, STEP);
        assert!(recent_low > new_high);
    }

    #[test]
    fn deadline_is_exact_in_f64() {
        for (tier, ms) in [
            (0u8, 0u64),
            (255, 2_000_000_000_000),
            (7, 1_700_000_000_000),
        ] {
            let s = deadline_score(tier, ms, STEP);
            assert_eq!(s, s.trunc(), "score must be an exact integer f64");
            assert!(s.abs() < 2f64.powi(53));
        }
    }

    #[test]
    fn descriptor_and_bus_messages_round_trip() {
        let policy_scope = crate::gateway::attempt::TrustedPolicyScope::from_identity(
            &crate::gateway::auth::Identity {
                tenant: "server-owned".into(),
                tier: 3,
                rpm: Some(1),
                tpm: Some(42),
                budget_usd: None,
                budget_window_secs: None,
                budget_allow_unpriced: false,
            },
        );
        let desc = JobDescriptor {
            id: "abc-1".into(),
            provider: "openai".into(),
            tier: 3,
            permits: 42,
            payload: serde_json::json!({"model": "gpt-5.5"}),
            policy_envelope_version: 1,
            trusted_unscoped: false,
            policy_scope: Some(policy_scope),
            stream: true,
            enqueue_ms: 1_700_000_000_000,
        };
        let back: JobDescriptor =
            serde_json::from_str(&serde_json::to_string(&desc).unwrap()).unwrap();
        assert_eq!(back.id, "abc-1");
        assert!(back.stream);
        assert!(back.policy_scope.is_some());
        let serialized = serde_json::to_string(&desc).unwrap();
        assert!(!serialized.contains("server-owned"));
        let prepared = PreparedSubmission::new(desc).unwrap();
        assert_eq!(prepared.serialized_payload, serialized);
        let prepared_descriptor: JobDescriptor =
            serde_json::from_str(&prepared.serialized_payload).unwrap();
        assert_eq!(prepared_descriptor.id, "abc-1");
        assert!(prepared_descriptor.stream);

        let legacy: JobDescriptor = serde_json::from_value(serde_json::json!({
            "id": "old",
            "provider": "openai",
            "tier": 1,
            "permits": 1,
            "payload": {"model": "gpt-5.5"},
            "policy_scope": {"tenant_key":"old-hash","rpm":1,"tpm":10}
        }))
        .unwrap();
        assert!(!descriptor_matches_protocol(
            QueueProtocol::ScopedV1,
            &legacy
        ));
        assert!(!descriptor_matches_protocol(
            QueueProtocol::LegacyUnscoped,
            &legacy
        ));

        let legacy_custom: JobDescriptor = serde_json::from_value(serde_json::json!({
            "id": "old-custom",
            "provider": "custom",
            "tier": 0,
            "permits": 1,
            "payload": {}
        }))
        .unwrap();
        assert!(descriptor_matches_protocol(
            QueueProtocol::LegacyUnscoped,
            &legacy_custom
        ));

        let trusted_unscoped = JobDescriptor {
            id: "custom".into(),
            provider: "custom".into(),
            tier: 0,
            permits: 1,
            payload: serde_json::json!({}),
            policy_envelope_version: 1,
            trusted_unscoped: true,
            policy_scope: None,
            stream: false,
            enqueue_ms: 0,
        };
        assert!(descriptor_matches_protocol(
            QueueProtocol::LegacyUnscoped,
            &trusted_unscoped
        ));

        for msg in [
            BusMessage::Unary(serde_json::json!({"a": 1})),
            BusMessage::Chunk("hello".into()),
            BusMessage::End,
            BusMessage::Error("boom".into()),
        ] {
            let s = serde_json::to_string(&msg).unwrap();
            let _back: BusMessage = serde_json::from_str(&s).unwrap();
        }
    }

    // ---- integration (require a live Redis via LLMSHIM_REDIS_URL) -----------
    // Run: LLMSHIM_REDIS_URL=redis://127.0.0.1:6379 \
    //        cargo test --features gateway-redis -- --ignored

    use super::super::DispatchError;
    use crate::proxy::ratelimit::{InMemoryRateLimiter, RateLimitConfig};

    struct EchoDispatch;
    #[async_trait::async_trait]
    impl Dispatch for EchoDispatch {
        async fn dispatch(&self, _p: &str, payload: Value) -> Result<Value, DispatchError> {
            Ok(serde_json::json!({ "echo": payload }))
        }
        async fn dispatch_stream(
            &self,
            _p: &str,
            _payload: Value,
        ) -> Result<super::super::ChunkStream, DispatchError> {
            let chunks: Vec<StreamChunk> = vec![Ok("a".into()), Ok("b".into()), Ok("c".into())];
            Ok(Box::pin(futures::stream::iter(chunks)))
        }
    }

    struct LatchBlockedDispatch {
        dispatch_starts: Arc<std::sync::atomic::AtomicUsize>,
        dispatch_started: Arc<tokio::sync::Notify>,
        release_dispatches: Arc<Semaphore>,
    }

    #[async_trait::async_trait]
    impl Dispatch for LatchBlockedDispatch {
        async fn dispatch(&self, _provider: &str, payload: Value) -> Result<Value, DispatchError> {
            self.dispatch_starts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.dispatch_started.notify_one();
            self.release_dispatches
                .acquire()
                .await
                .expect("test release semaphore remains open")
                .forget();
            Ok(payload)
        }
    }

    struct QuietStreamDispatch {
        dispatch_starts: Arc<std::sync::atomic::AtomicUsize>,
        dispatch_started: Arc<tokio::sync::Notify>,
        release_streams: Arc<Semaphore>,
    }

    struct PendingDispatch {
        dispatch_starts: Arc<std::sync::atomic::AtomicUsize>,
        dispatch_started: Arc<tokio::sync::Notify>,
        dispatch_drops: Arc<std::sync::atomic::AtomicUsize>,
        dispatch_dropped: Arc<tokio::sync::Notify>,
    }

    struct ProgressStreamDispatch;

    struct DispatchDropGuard {
        dispatch_drops: Arc<std::sync::atomic::AtomicUsize>,
        dispatch_dropped: Arc<tokio::sync::Notify>,
    }

    impl Drop for DispatchDropGuard {
        fn drop(&mut self) {
            self.dispatch_drops
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.dispatch_dropped.notify_one();
        }
    }

    #[async_trait::async_trait]
    impl Dispatch for PendingDispatch {
        async fn dispatch(&self, _provider: &str, _payload: Value) -> Result<Value, DispatchError> {
            self.dispatch_starts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.dispatch_started.notify_one();
            let _drop_guard = DispatchDropGuard {
                dispatch_drops: self.dispatch_drops.clone(),
                dispatch_dropped: self.dispatch_dropped.clone(),
            };
            std::future::pending().await
        }
    }

    #[async_trait::async_trait]
    impl Dispatch for QuietStreamDispatch {
        async fn dispatch(&self, _provider: &str, _payload: Value) -> Result<Value, DispatchError> {
            unreachable!("quiet stream fixture only dispatches streams")
        }

        async fn dispatch_stream(
            &self,
            _provider: &str,
            _payload: Value,
        ) -> Result<super::super::ChunkStream, DispatchError> {
            self.dispatch_starts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.dispatch_started.notify_one();
            let release_streams = self.release_streams.clone();
            Ok(Box::pin(futures::stream::once(async move {
                release_streams
                    .acquire()
                    .await
                    .expect("test release semaphore remains open")
                    .forget();
                Ok("terminal".into())
            })))
        }
    }

    #[async_trait::async_trait]
    impl Dispatch for ProgressStreamDispatch {
        async fn dispatch(&self, _provider: &str, _payload: Value) -> Result<Value, DispatchError> {
            unreachable!("progress stream fixture only dispatches streams")
        }

        async fn dispatch_stream(
            &self,
            _provider: &str,
            _payload: Value,
        ) -> Result<super::super::ChunkStream, DispatchError> {
            Ok(Box::pin(async_stream::stream! {
                loop {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    yield Ok("progress".into());
                }
            }))
        }
    }

    fn unlimited() -> Arc<dyn RateLimiter> {
        Arc::new(InMemoryRateLimiter::new(RateLimitConfig::default()))
    }

    async fn redis_server_time_ms(connection: &mut ConnectionManager) -> u64 {
        let (seconds, microseconds): (u64, u64) =
            redis::cmd("TIME").query_async(connection).await.unwrap();
        seconds
            .saturating_mul(1_000)
            .saturating_add(microseconds / 1_000)
    }

    async fn test_gateway(provider_seed: &str) -> Option<Arc<DistributedGateway>> {
        let url = std::env::var("LLMSHIM_REDIS_URL").ok()?;
        // Clean the keys this test uses so reruns are deterministic.
        let client = redis::Client::open(url.clone()).unwrap();
        let mut conn = ConnectionManager::new(client).await.unwrap();
        for k in [
            queue_key(provider_seed),
            processing_key(provider_seed),
            leased_key(provider_seed),
            protocol_owners_key(QueueProtocol::LegacyUnscoped, provider_seed),
        ] {
            let _: Result<i64, _> = conn.del(k).await;
        }
        DistributedGateway::connect(
            &url,
            Arc::new(EchoDispatch),
            unlimited(),
            GatewayConfig::default(),
        )
        .await
        .ok()
    }

    async fn job_origin_token(connection: &mut ConnectionManager, id: &str) -> String {
        connection
            .hget(lifecycle::meta_key(id), "origin_token")
            .await
            .unwrap()
    }

    async fn lease_test_job(
        gateway: &Arc<DistributedGateway>,
        provider: &str,
        stream: bool,
        owner_token: &str,
    ) -> (LeaseOwnership, String) {
        let mut prepared = gateway
            .prepare_submission(
                GatewayRequest {
                    provider: provider.into(),
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"race"}),
                },
                stream,
                None,
            )
            .unwrap();
        let id = prepared.descriptor.id.clone();
        gateway
            .enqueue_prepared(
                &mut prepared,
                tokio::time::Instant::now() + gateway.config.request_timeout,
            )
            .await
            .unwrap();
        let protocol = QueueProtocol::LegacyUnscoped;
        let mut connection = gateway.connection_for_test().await.unwrap();
        let leased: Option<(String, String, String)> = gateway
            .lease
            .key(protocol_queue_key(protocol, provider))
            .key(protocol_processing_key(protocol, provider))
            .key(protocol_leased_key(protocol, provider))
            .key(protocol_owners_key(protocol, provider))
            .key(lifecycle::counters())
            .key(lifecycle::terminal_reservations())
            .key(lifecycle::reservation_states())
            .key(lifecycle::expiry())
            .arg(60_000_u64)
            .arg(owner_token)
            .arg(gateway.lifecycle_limits.max_terminal_bytes)
            .arg(gateway.lifecycle_limits.unary_terminal_bytes)
            .arg(gateway.lifecycle_limits.stream_terminal_bytes)
            .arg(lifecycle::job_prefix())
            .arg(7 * 60 * 60 * 1_000_u64)
            .arg(6 * 60 * 60 * 1_000_u64)
            .invoke_async(&mut connection)
            .await
            .unwrap();
        let (member, original_score, serialized_payload) = leased.expect("job should lease");
        (
            LeaseOwnership {
                protocol,
                provider: provider.into(),
                member,
                original_score,
                owner_token: owner_token.into(),
                serialized_payload,
            },
            id,
        )
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_timed_out_operation_retires_only_its_cached_connection_generation() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let mut gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(EchoDispatch),
            unlimited(),
            GatewayConfig::default(),
        )
        .await
        .unwrap();
        Arc::get_mut(&mut gateway).unwrap().redis_operation_timeout = Duration::from_millis(20);
        let connection_name = format!("llmshim-retire-{}", uuid::Uuid::new_v4().simple());
        let set_name = connection_name.clone();
        gateway
            .redis_operation(|mut connection| async move {
                redis::cmd("CLIENT")
                    .arg("SETNAME")
                    .arg(set_name)
                    .query_async::<String>(&mut connection)
                    .await
            })
            .await
            .unwrap();

        let observer_client = redis::Client::open(redis_url).unwrap();
        let mut observer = ConnectionManager::new(observer_client).await.unwrap();
        let clients: String = redis::cmd("CLIENT")
            .arg("LIST")
            .query_async(&mut observer)
            .await
            .unwrap();
        assert!(clients.contains(&connection_name));

        let blocking_key = format!("retire-block-{}", uuid::Uuid::new_v4().simple());
        assert!(gateway
            .redis_operation(|mut connection| async move {
                redis::cmd("BRPOP")
                    .arg(blocking_key)
                    .arg(1_u64)
                    .query_async::<Option<(String, String)>>(&mut connection)
                    .await
            })
            .await
            .is_err());

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let clients: String = redis::cmd("CLIENT")
                    .arg("LIST")
                    .query_async(&mut observer)
                    .await
                    .unwrap();
                if !clients.contains(&connection_name) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retired cached generation should close its socket");
        assert!(gateway.ping().await);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_concurrent_origins_share_one_remaining_queue_slot() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider_name = format!("queue-admission-{}", uuid::Uuid::new_v4());
        let mut gateways = Vec::new();
        for _ in 0..3 {
            gateways.push(
                DistributedGateway::connect(
                    &redis_url,
                    Arc::new(EchoDispatch),
                    unlimited(),
                    GatewayConfig {
                        max_queue_depth: 2,
                        ..GatewayConfig::default()
                    },
                )
                .await
                .unwrap(),
            );
        }
        let prepare_submission = |request_id: &str, tier: u8| {
            PreparedSubmission::new(JobDescriptor {
                id: format!("{provider_name}-{request_id}"),
                provider: provider_name.clone(),
                tier,
                permits: 1,
                payload: serde_json::json!({"request": request_id}),
                policy_envelope_version: 1,
                trusted_unscoped: true,
                policy_scope: None,
                stream: false,
                enqueue_ms: 1_700_000_000_000,
            })
            .unwrap()
        };
        let existing_submission = prepare_submission("existing", 0);
        gateways[0].enqueue(&existing_submission).await.unwrap();
        let prepared_submissions = [
            prepare_submission("origin-a", 1),
            prepare_submission("origin-b", 2),
            prepare_submission("origin-c", 3),
        ];
        let results = futures::future::join_all(
            gateways
                .iter()
                .zip(prepared_submissions.iter())
                .map(|(gateway, prepared)| gateway.enqueue(prepared)),
        )
        .await;
        let mut connection = gateways[0].connection_for_test().await.unwrap();
        let queued_members: Vec<String> = connection
            .zrange(queue_key(&provider_name), 0, -1)
            .await
            .unwrap();
        let _: i64 = connection.del(queue_key(&provider_name)).await.unwrap();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(queued_members.len(), 2);
        for (result, prepared) in results.iter().zip(prepared_submissions.iter()) {
            if result.is_ok() {
                assert!(queued_members
                    .iter()
                    .any(|member| member.starts_with(&format!("{}:", prepared.descriptor.id))));
            } else {
                assert!(matches!(result, Err(GatewayError::Overloaded(_))));
            }
        }
        assert!(queued_members
            .iter()
            .any(|member| member.starts_with(&format!("{}:", existing_submission.descriptor.id))));
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_queue_depth_limit_is_shared_across_scoped_and_legacy_protocols() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("queue-protocol-cap-{}", uuid::Uuid::new_v4());
        let config = GatewayConfig {
            max_queue_depth: 2,
            ..GatewayConfig::default()
        };
        let first = DistributedGateway::connect(
            &redis_url,
            Arc::new(EchoDispatch),
            unlimited(),
            config.clone(),
        )
        .await
        .unwrap();
        let second =
            DistributedGateway::connect(&redis_url, Arc::new(EchoDispatch), unlimited(), config)
                .await
                .unwrap();
        let existing = first
            .prepare_submission(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"id":"existing"}),
                },
                false,
                None,
            )
            .unwrap();
        let mut connection = first.connection_for_test().await.unwrap();
        let released_queue = released_queue_key(QueueProtocol::LegacyUnscoped, &provider);
        let _: () = connection
            .zadd(&released_queue, &existing.member, 0_u64)
            .await
            .unwrap();
        let scoped = first
            .prepare_with_policy(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 1,
                    permits: 1,
                    payload: serde_json::json!({"id":"scoped"}),
                },
                false,
                crate::gateway::attempt::TrustedPolicyScope::from_identity(
                    &crate::gateway::auth::Identity {
                        tenant: "queue-protocol-cap".into(),
                        tier: 0,
                        rpm: None,
                        tpm: None,
                        budget_usd: None,
                        budget_window_secs: None,
                        budget_allow_unpriced: false,
                    },
                ),
            )
            .unwrap();
        let competing_legacy = second
            .prepare_submission(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 2,
                    permits: 1,
                    payload: serde_json::json!({"id":"legacy"}),
                },
                false,
                None,
            )
            .unwrap();
        let (scoped_result, legacy_result) =
            tokio::join!(first.enqueue(&scoped), second.enqueue(&competing_legacy));
        assert!(matches!(scoped_result, Err(GatewayError::Upstream(_))));
        assert!(matches!(legacy_result, Err(GatewayError::Upstream(_))));
        let legacy_depth: u64 = connection
            .zcard(protocol_queue_key(QueueProtocol::LegacyUnscoped, &provider))
            .await
            .unwrap();
        let scoped_depth: u64 = connection
            .zcard(protocol_queue_key(QueueProtocol::ScopedV1, &provider))
            .await
            .unwrap();
        let released_depth: u64 = connection.zcard(&released_queue).await.unwrap();
        assert_eq!(legacy_depth + scoped_depth, 0);
        assert_eq!(released_depth, 1);
        for protocol in [QueueProtocol::LegacyUnscoped, QueueProtocol::ScopedV1] {
            let _: i64 = connection
                .del(protocol_queue_key(protocol, &provider))
                .await
                .unwrap();
        }
        let _: i64 = connection.del(released_queue).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_queue_admission_zero_capacity_and_storage_errors_fail_closed() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider_name = format!("queue-failure-{}", uuid::Uuid::new_v4());
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(EchoDispatch),
            unlimited(),
            GatewayConfig {
                max_queue_depth: 0,
                overloaded_retry_after: Duration::from_secs(7),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        let prepared_submission = PreparedSubmission::new(JobDescriptor {
            id: format!("refused-{}", uuid::Uuid::new_v4().simple()),
            provider: provider_name.clone(),
            tier: 0,
            permits: 1,
            payload: serde_json::json!({"request": "synthetic payload"}),
            policy_envelope_version: 1,
            trusted_unscoped: true,
            policy_scope: None,
            stream: false,
            enqueue_ms: 1_700_000_000_000,
        })
        .unwrap();
        assert!(matches!(
            gateway.enqueue(&prepared_submission).await,
            Err(GatewayError::Overloaded(delay)) if delay == Duration::from_secs(7)
        ));
        let provider_queue_key = queue_key(&provider_name);
        let mut connection = gateway.connection_for_test().await.unwrap();
        let queue_exists: bool = connection.exists(&provider_queue_key).await.unwrap();
        assert!(!queue_exists);

        let _: () = connection
            .set(&provider_queue_key, "existing value")
            .await
            .unwrap();
        let storage_error_submission = PreparedSubmission::new(JobDescriptor {
            id: format!("storage-error-{}", uuid::Uuid::new_v4().simple()),
            provider: provider_name.clone(),
            tier: 0,
            permits: 1,
            payload: serde_json::json!({"request": "synthetic payload"}),
            policy_envelope_version: 1,
            trusted_unscoped: true,
            policy_scope: None,
            stream: false,
            enqueue_ms: 1_700_000_000_001,
        })
        .unwrap();
        assert!(matches!(
            gateway.enqueue(&storage_error_submission).await,
            Err(GatewayError::Upstream(_))
        ));
        let stored_value: String = connection.get(&provider_queue_key).await.unwrap();
        assert_eq!(stored_value, "existing value");
        let _: i64 = connection.del(provider_queue_key).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_origin_lease_protocol_waits_for_lifecycle_v1_drain() {
        let provider = format!("origin-drain-{}", uuid::Uuid::new_v4().simple());
        let Some(gateway) = test_gateway(&provider).await else {
            return;
        };
        let old_queue = prior_lifecycle_key(QueueProtocol::LegacyUnscoped, "q", &provider);
        let old_member = format!("old-accepted-{}", uuid::Uuid::new_v4().simple());
        let mut connection = gateway.connection_for_test().await.unwrap();
        let _: () = connection.zadd(&old_queue, &old_member, 0).await.unwrap();
        let prepared = gateway
            .prepare_submission(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"new-protocol"}),
                },
                false,
                None,
            )
            .unwrap();
        assert!(matches!(
            gateway.enqueue(&prepared).await,
            Err(GatewayError::Upstream(message)) if message.contains("migration")
        ));
        assert_eq!(
            connection
                .zscore::<_, _, f64>(&old_queue, &old_member)
                .await
                .unwrap(),
            0.0
        );
        let _: i64 = connection.zrem(&old_queue, &old_member).await.unwrap();
        gateway.enqueue(&prepared).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_scoped_protocol_is_invisible_to_released_workers_and_reapers() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct ReleasedPolicyScope {
            tenant_key: String,
            rpm: Option<u32>,
            tpm: Option<u32>,
        }

        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct ReleasedJobDescriptor {
            id: String,
            provider: String,
            tier: u8,
            permits: u32,
            payload: Value,
            policy_scope: Option<ReleasedPolicyScope>,
            stream: bool,
            enqueue_ms: u64,
        }

        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("mixed-version-{}", uuid::Uuid::new_v4().simple());
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(EchoDispatch),
            unlimited(),
            GatewayConfig::default(),
        )
        .await
        .unwrap();
        let identity = crate::gateway::auth::Identity {
            tenant: format!("raw-tenant-{}", uuid::Uuid::new_v4()),
            tier: 0,
            rpm: Some(10),
            tpm: Some(100),
            budget_usd: Some(1.0),
            budget_window_secs: Some(60),
            budget_allow_unpriced: false,
        };
        let raw_tenant = identity.tenant.clone();
        let prepared = gateway
            .prepare_with_policy(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"model":"synthetic"}),
                },
                false,
                crate::gateway::attempt::TrustedPolicyScope::from_identity(&identity),
            )
            .unwrap();
        assert_eq!(prepared.protocol, QueueProtocol::ScopedV1);
        assert!(!prepared.serialized_payload.contains(&raw_tenant));
        let released: ReleasedJobDescriptor =
            serde_json::from_str(&prepared.serialized_payload).unwrap();
        assert!(released.policy_scope.is_some());

        let mut connection = gateway.connection_for_test().await.unwrap();
        let legacy_queue = released_queue_key(QueueProtocol::LegacyUnscoped, &provider);
        let legacy_processing = released_processing_key(QueueProtocol::LegacyUnscoped, &provider);
        let legacy_leased =
            released_protocol_key(QueueProtocol::LegacyUnscoped, "leased", &provider);
        let scoped_queue = protocol_queue_key(QueueProtocol::ScopedV1, &provider);
        let scoped_processing = protocol_processing_key(QueueProtocol::ScopedV1, &provider);
        let scoped_leased = protocol_leased_key(QueueProtocol::ScopedV1, &provider);
        let scoped_owners = protocol_owners_key(QueueProtocol::ScopedV1, &provider);
        for key in [
            legacy_queue.clone(),
            legacy_processing.clone(),
            legacy_leased.clone(),
            scoped_queue.clone(),
            scoped_processing.clone(),
            scoped_leased.clone(),
            scoped_owners.clone(),
        ] {
            let _: i64 = connection.del(key).await.unwrap();
        }
        gateway.enqueue(&prepared).await.unwrap();
        assert_eq!(connection.zcard::<_, u64>(&legacy_queue).await.unwrap(), 0);
        assert_eq!(connection.zcard::<_, u64>(&scoped_queue).await.unwrap(), 1);

        let released_lease = redis::Script::new(
            r#"
            local top = redis.call('ZPOPMIN', KEYS[1], 1)
            if #top == 0 then return false end
            redis.call('ZADD', KEYS[2], ARGV[1], top[1])
            redis.call('HSET', KEYS[3], top[1], top[2])
            return {top[1], top[2]}
            "#,
        );
        let released_worker_lease: Option<(String, String)> = released_lease
            .key(&legacy_queue)
            .key(&legacy_processing)
            .key(&legacy_leased)
            .arg(1_u64)
            .invoke_async(&mut connection)
            .await
            .unwrap();
        assert!(released_worker_lease.is_none());

        let scoped_lease: Option<(String, String)> = gateway
            .lease
            .key(&scoped_queue)
            .key(&scoped_processing)
            .key(&scoped_leased)
            .key(&scoped_owners)
            .arg(1_u64)
            .arg("scoped-owner")
            .invoke_async(&mut connection)
            .await
            .unwrap();
        let (scoped_member, _) = scoped_lease.expect("scoped job should lease");
        assert_eq!(
            gateway
                .reap_once_protocol(QueueProtocol::LegacyUnscoped, &provider)
                .await,
            0
        );
        assert_eq!(
            connection
                .zcard::<_, u64>(&scoped_processing)
                .await
                .unwrap(),
            1
        );
        let _: usize = connection
            .zadd(&scoped_processing, &scoped_member, 0_u64)
            .await
            .unwrap();
        assert_eq!(
            gateway
                .reap_once_protocol(QueueProtocol::ScopedV1, &provider)
                .await,
            1
        );

        let old_scoped_member = serde_json::json!({
            "id": "released-scoped",
            "provider": provider,
            "tier": 0,
            "permits": 1,
            "payload": {},
            "policy_scope": {"tenant_key":"old-hash","rpm":10,"tpm":100},
            "stream": false,
            "enqueue_ms": 1_700_000_000_000_u64
        })
        .to_string();
        let _: usize = connection
            .zadd(&legacy_queue, &old_scoped_member, 0_u64)
            .await
            .unwrap();
        let newly_leased: Option<(String, String)> = gateway
            .lease
            .key(protocol_queue_key(QueueProtocol::LegacyUnscoped, &provider))
            .key(protocol_processing_key(
                QueueProtocol::LegacyUnscoped,
                &provider,
            ))
            .key(protocol_leased_key(
                QueueProtocol::LegacyUnscoped,
                &provider,
            ))
            .key(protocol_owners_key(
                QueueProtocol::LegacyUnscoped,
                &provider,
            ))
            .arg(1_000_u64)
            .arg("legacy-owner")
            .invoke_async(&mut connection)
            .await
            .unwrap();
        assert!(newly_leased.is_none());
        assert_eq!(connection.zcard::<_, u64>(&legacy_queue).await.unwrap(), 1);
        let released_processing_delivery: Option<(String, String)> = released_lease
            .key(&legacy_queue)
            .key(&legacy_processing)
            .key(&legacy_leased)
            .arg(1_u64)
            .invoke_async(&mut connection)
            .await
            .unwrap();
        assert!(released_processing_delivery.is_some());
        assert_eq!(
            gateway
                .reap_once_protocol(QueueProtocol::LegacyUnscoped, &provider)
                .await,
            0
        );
        assert_eq!(
            connection
                .zcard::<_, u64>(&legacy_processing)
                .await
                .unwrap(),
            1
        );

        for key in [
            legacy_queue,
            legacy_processing,
            legacy_leased,
            scoped_queue,
            scoped_processing,
            scoped_leased,
            scoped_owners,
        ] {
            let _: i64 = connection.del(key).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_unary_round_trip() {
        let provider = format!("itest-unary-{}", uuid::Uuid::new_v4().simple());
        let Some(gw) = test_gateway(&provider).await else {
            return;
        };
        gw.spawn_workers(vec![provider.clone()]);
        let resp = gw
            .submit(GatewayRequest {
                provider,
                tier: 0,
                permits: 1,
                payload: serde_json::json!({ "hello": "world" }),
            })
            .await
            .expect("round trip");
        assert_eq!(resp["echo"]["hello"], "world");
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_stream_round_trip() {
        let provider = format!("itest-stream-{}", uuid::Uuid::new_v4().simple());
        let Some(gw) = test_gateway(&provider).await else {
            return;
        };
        gw.spawn_workers(vec![provider.clone()]);
        let mut rx = gw
            .submit_stream(GatewayRequest {
                provider,
                tier: 0,
                permits: 1,
                payload: serde_json::json!({}),
            })
            .await
            .expect("stream start");
        let mut got = Vec::new();
        while let Some(item) = rx.recv().await {
            got.push(item.expect("chunk"));
        }
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_heartbeat_prevents_healthy_unary_redelivery() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("lease-unary-{}", uuid::Uuid::new_v4().simple());
        let dispatch_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let release_dispatches = Arc::new(Semaphore::new(0));
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(LatchBlockedDispatch {
                dispatch_starts: dispatch_starts.clone(),
                dispatch_started: dispatch_started.clone(),
                release_dispatches: release_dispatches.clone(),
            }),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(300),
                request_timeout: Duration::from_secs(3),
                max_concurrency_per_provider: 2,
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        let worker_handles = gateway.spawn_workers(vec![provider.clone()]);
        let first_dispatch_started = dispatch_started.notified();
        let submission_gateway = gateway.clone();
        let submission_provider = provider.clone();
        let submission = tokio::spawn(async move {
            submission_gateway
                .submit(GatewayRequest {
                    provider: submission_provider,
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"quiet unary"}),
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), first_dispatch_started)
            .await
            .expect("unary dispatch should start");

        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(320)).await;
            assert_eq!(
                gateway
                    .reap_once_protocol(QueueProtocol::LegacyUnscoped, &provider)
                    .await,
                0,
                "a healthy unary delivery must refresh independently"
            );
        }
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 1);

        release_dispatches.add_permits(1);
        let response = tokio::time::timeout(Duration::from_secs(1), submission)
            .await
            .expect("unary submission should finish")
            .unwrap()
            .unwrap();
        assert_eq!(response["request"], "quiet unary");
        for worker_handle in worker_handles {
            worker_handle.abort();
        }
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_terminal_capacity_refuses_before_dispatch() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("terminal-capacity-{}", uuid::Uuid::new_v4().simple());
        let dispatch_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let release_dispatches = Arc::new(Semaphore::new(0));
        let mut gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(LatchBlockedDispatch {
                dispatch_starts: dispatch_starts.clone(),
                dispatch_started,
                release_dispatches,
            }),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(300),
                request_timeout: Duration::from_millis(120),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        Arc::get_mut(&mut gateway).unwrap().lifecycle_limits = lifecycle::Limits {
            max_jobs: 100_000,
            max_base_bytes: 512 * 1024 * 1024,
            max_terminal_bytes: 63,
            unary_terminal_bytes: 64,
            stream_terminal_bytes: 16,
        };
        let workers = gateway.spawn_workers(vec![provider.clone()]);
        let result = gateway
            .submit(GatewayRequest {
                provider,
                tier: 0,
                permits: 1,
                payload: serde_json::json!({"request":"no paid send"}),
            })
            .await;
        assert!(
            matches!(&result, Err(GatewayError::Timeout))
                || matches!(
                    &result,
                    Err(GatewayError::Upstream(message))
                        if message == "distributed origin canceled"
                ),
            "unexpected capacity result: {result:?}"
        );
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 0);
        for worker in workers {
            worker.abort();
        }
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_processing_cancellation_drops_work_without_requeue() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("lifecycle-cancel-{}", uuid::Uuid::new_v4().simple());
        let dispatch_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let dispatch_drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_dropped = Arc::new(tokio::sync::Notify::new());
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(PendingDispatch {
                dispatch_starts: dispatch_starts.clone(),
                dispatch_started: dispatch_started.clone(),
                dispatch_drops: dispatch_drops.clone(),
                dispatch_dropped: dispatch_dropped.clone(),
            }),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(300),
                request_timeout: Duration::from_secs(2),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        let workers = gateway.spawn_workers(vec![provider.clone()]);
        let first_dispatch_started = dispatch_started.notified();
        let dropped = dispatch_dropped.notified();
        let submission_gateway = gateway.clone();
        let submission_provider = provider.clone();
        let submission = tokio::spawn(async move {
            submission_gateway
                .submit(GatewayRequest {
                    provider: submission_provider,
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"cancel active"}),
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), first_dispatch_started)
            .await
            .expect("dispatch should start");
        let mut connection = gateway.connection_for_test().await.unwrap();
        let processing = protocol_processing_key(QueueProtocol::LegacyUnscoped, &provider);
        let members: Vec<String> = connection.zrange(&processing, 0, -1).await.unwrap();
        assert_eq!(members.len(), 1);
        let (id, generation) = members[0].rsplit_once(':').unwrap();
        let origin_token = job_origin_token(&mut connection, id).await;
        assert_eq!(
            lifecycle::cancel(
                &mut connection,
                QueueProtocol::LegacyUnscoped,
                &provider,
                id,
                generation,
                &members[0],
                Duration::from_secs(60),
                &origin_token,
            )
            .await
            .unwrap(),
            2
        );
        tokio::time::timeout(Duration::from_secs(1), dropped)
            .await
            .expect("heartbeat cancellation should drop dispatch");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), submission)
                .await
                .unwrap()
                .unwrap(),
            Err(GatewayError::Upstream(message)) if message == "distributed origin canceled"
        ));
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(dispatch_drops.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            connection
                .zcard::<_, u64>(protocol_queue_key(QueueProtocol::LegacyUnscoped, &provider,))
                .await
                .unwrap(),
            0
        );
        for worker in workers {
            worker.abort();
        }
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_cancel_first_suppresses_chunks_terminals_release_and_dlq() {
        let provider_seed = format!("cancel-races-{}", uuid::Uuid::new_v4().simple());
        let Some(gateway) = test_gateway(&provider_seed).await else {
            return;
        };

        for (suffix, candidate) in [
            (
                "unary",
                BusMessage::Unary(serde_json::json!({"private":"paid"})),
            ),
            (
                "deadline",
                BusMessage::Error("distributed worker deadline exceeded".into()),
            ),
            (
                "provider-error",
                BusMessage::Error("private provider error".into()),
            ),
        ] {
            let provider = format!("{provider_seed}-{suffix}");
            let (ownership, id) = lease_test_job(&gateway, &provider, false, suffix).await;
            let (_, generation) = ownership.id_and_generation().unwrap();
            let mut connection = gateway.connection_for_test().await.unwrap();
            let origin_token = job_origin_token(&mut connection, &id).await;
            assert_eq!(
                lifecycle::cancel(
                    &mut connection,
                    ownership.protocol,
                    &provider,
                    &id,
                    generation,
                    &ownership.member,
                    Duration::from_secs(60),
                    &origin_token,
                )
                .await
                .unwrap(),
                2
            );
            assert!(
                gateway
                    .terminal_owned(&ownership, &id, false, &candidate)
                    .await
            );
            assert!(matches!(
                lifecycle::read_terminal(&mut connection, &id, generation)
                    .await
                    .unwrap(),
                Some(BusMessage::Error(message)) if message == "distributed origin canceled"
            ));
        }

        let stream_provider = format!("{provider_seed}-stream");
        let (stream_ownership, stream_id) =
            lease_test_job(&gateway, &stream_provider, true, "stream-owner").await;
        let (_, stream_generation) = stream_ownership.id_and_generation().unwrap();
        let mut connection = gateway.connection_for_test().await.unwrap();
        let stream_origin_token = job_origin_token(&mut connection, &stream_id).await;
        assert_eq!(
            lifecycle::cancel(
                &mut connection,
                stream_ownership.protocol,
                &stream_provider,
                &stream_id,
                stream_generation,
                &stream_ownership.member,
                Duration::from_secs(60),
                &stream_origin_token,
            )
            .await
            .unwrap(),
            2
        );
        assert!(
            !gateway
                .publish_owned(
                    &stream_ownership,
                    &protocol_response_channel(stream_ownership.protocol, &stream_id),
                    &BusMessage::Chunk("private-ready-chunk".into()),
                )
                .await
        );
        assert!(
            gateway
                .terminal_owned(&stream_ownership, &stream_id, true, &BusMessage::End)
                .await
        );
        assert!(matches!(
            lifecycle::read_terminal(&mut connection, &stream_id, stream_generation)
                .await
                .unwrap(),
            Some(BusMessage::Error(message)) if message == "distributed origin canceled"
        ));

        let release_provider = format!("{provider_seed}-release");
        let (release_ownership, release_id) =
            lease_test_job(&gateway, &release_provider, false, "release-owner").await;
        let (_, release_generation) = release_ownership.id_and_generation().unwrap();
        let release_origin_token = job_origin_token(&mut connection, &release_id).await;
        assert_eq!(
            lifecycle::cancel(
                &mut connection,
                release_ownership.protocol,
                &release_provider,
                &release_id,
                release_generation,
                &release_ownership.member,
                Duration::from_secs(60),
                &release_origin_token,
            )
            .await
            .unwrap(),
            2
        );
        assert!(!gateway.release_owned(&release_ownership).await);
        assert_eq!(
            connection
                .zcard::<_, u64>(protocol_queue_key(
                    release_ownership.protocol,
                    &release_provider,
                ))
                .await
                .unwrap(),
            0
        );
        assert!(
            gateway
                .terminal_owned(
                    &release_ownership,
                    &release_id,
                    false,
                    &BusMessage::Error("rate release".into()),
                )
                .await
        );

        let dlq_provider = format!("{provider_seed}-dlq");
        let (dlq_ownership, dlq_id) =
            lease_test_job(&gateway, &dlq_provider, false, "dlq-owner").await;
        let (_, dlq_generation) = dlq_ownership.id_and_generation().unwrap();
        let dlq_origin_token = job_origin_token(&mut connection, &dlq_id).await;
        assert_eq!(
            lifecycle::cancel(
                &mut connection,
                dlq_ownership.protocol,
                &dlq_provider,
                &dlq_id,
                dlq_generation,
                &dlq_ownership.member,
                Duration::from_secs(60),
                &dlq_origin_token,
            )
            .await
            .unwrap(),
            2
        );
        assert!(gateway.dead_letter_owned(&dlq_ownership).await);
        assert_eq!(gateway.dead_letter_len(&dlq_provider).await, 0);
        assert!(matches!(
            lifecycle::read_terminal(&mut connection, &dlq_id, dlq_generation)
                .await
                .unwrap(),
            Some(BusMessage::Error(message)) if message == "distributed origin canceled"
        ));

        let reap_provider = format!("{provider_seed}-reap");
        let (reap_ownership, reap_id) =
            lease_test_job(&gateway, &reap_provider, false, "reap-owner").await;
        let (_, reap_generation) = reap_ownership.id_and_generation().unwrap();
        let reap_origin_token = job_origin_token(&mut connection, &reap_id).await;
        assert_eq!(
            lifecycle::cancel(
                &mut connection,
                reap_ownership.protocol,
                &reap_provider,
                &reap_id,
                reap_generation,
                &reap_ownership.member,
                Duration::from_secs(60),
                &reap_origin_token,
            )
            .await
            .unwrap(),
            2
        );
        let _: () = connection
            .zadd(
                protocol_processing_key(reap_ownership.protocol, &reap_provider),
                &reap_ownership.member,
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            gateway
                .reap_once_protocol(reap_ownership.protocol, &reap_provider)
                .await,
            1
        );
        assert_eq!(
            connection
                .zcard::<_, u64>(protocol_queue_key(reap_ownership.protocol, &reap_provider))
                .await
                .unwrap(),
            0
        );
        assert!(matches!(
            lifecycle::read_terminal(&mut connection, &reap_id, reap_generation)
                .await
                .unwrap(),
            Some(BusMessage::Error(message)) if message == "distributed origin canceled"
        ));

        let first_provider = format!("{provider_seed}-terminal-first");
        let (first_ownership, first_id) =
            lease_test_job(&gateway, &first_provider, false, "terminal-first-owner").await;
        let (_, first_generation) = first_ownership.id_and_generation().unwrap();
        let first_origin_token = job_origin_token(&mut connection, &first_id).await;
        assert!(
            gateway
                .terminal_owned(
                    &first_ownership,
                    &first_id,
                    false,
                    &BusMessage::Unary(serde_json::json!({"winner":"terminal"})),
                )
                .await
        );
        assert_eq!(
            lifecycle::cancel(
                &mut connection,
                first_ownership.protocol,
                &first_provider,
                &first_id,
                first_generation,
                &first_ownership.member,
                Duration::from_secs(60),
                &first_origin_token,
            )
            .await
            .unwrap(),
            3
        );
        assert!(matches!(
            lifecycle::read_terminal(&mut connection, &first_id, first_generation)
                .await
                .unwrap(),
            Some(BusMessage::Unary(value)) if value == serde_json::json!({"winner":"terminal"})
        ));
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_origin_crash_expiry_cancels_waiting_job_after_restart() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("origin-crash-{}", uuid::Uuid::new_v4().simple());
        let mut gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(EchoDispatch),
            unlimited(),
            GatewayConfig {
                request_timeout: Duration::from_secs(2),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        Arc::get_mut(&mut gateway).unwrap().origin_lease_timeout = Duration::from_millis(40);
        let prepared = gateway
            .prepare_submission(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"crash"}),
                },
                false,
                None,
            )
            .unwrap();
        let id = prepared.descriptor.id.clone();
        let mut accepted = gateway.accept_prepared(prepared, None).await.unwrap();
        let member = accepted.prepared.member.clone();
        let generation = member.rsplit_once(':').unwrap().1.to_string();
        let mut connection = gateway.connection_for_test().await.unwrap();
        assert_eq!(
            lifecycle::heartbeat_origin(
                &mut connection,
                &id,
                &generation,
                &member,
                "stale-origin-token",
                Duration::from_millis(40),
            )
            .await
            .unwrap(),
            0
        );
        accepted.origin_guard.disarm();
        drop(accepted);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            gateway
                .redis_operation(|mut connection| async move {
                    lifecycle::expire_origins(&mut connection, 16, Duration::from_secs(60)).await
                })
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .zcard::<_, u64>(protocol_queue_key(QueueProtocol::LegacyUnscoped, &provider,))
                .await
                .unwrap(),
            0
        );
        assert!(matches!(
            lifecycle::read_terminal(&mut connection, &id, &generation)
                .await
                .unwrap(),
            Some(BusMessage::Error(message)) if message == "distributed origin canceled"
        ));
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_origin_crash_expiry_drops_processing_dispatch() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("origin-processing-crash-{}", uuid::Uuid::new_v4().simple());
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let dispatch_dropped = Arc::new(tokio::sync::Notify::new());
        let mut gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(PendingDispatch {
                dispatch_starts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                dispatch_started: dispatch_started.clone(),
                dispatch_drops: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                dispatch_dropped: dispatch_dropped.clone(),
            }),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(300),
                request_timeout: Duration::from_secs(2),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        Arc::get_mut(&mut gateway).unwrap().origin_lease_timeout = Duration::from_millis(40);
        let mut prepared = gateway
            .prepare_submission(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"processing-crash"}),
                },
                false,
                None,
            )
            .unwrap();
        let id = prepared.descriptor.id.clone();
        gateway
            .enqueue_prepared(
                &mut prepared,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        let generation = prepared.member.rsplit_once(':').unwrap().1.to_string();
        let worker = tokio::spawn(gateway.clone().worker(
            provider,
            QueueProtocol::LegacyUnscoped,
            Arc::new(Semaphore::new(1)),
        ));
        let started = dispatch_started.notified();
        tokio::time::timeout(Duration::from_secs(1), started)
            .await
            .expect("processing dispatch should start");
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            gateway
                .redis_operation(|mut connection| async move {
                    lifecycle::expire_origins(&mut connection, 16, Duration::from_secs(60)).await
                })
                .await
                .unwrap(),
            1
        );
        tokio::time::timeout(Duration::from_secs(1), dispatch_dropped.notified())
            .await
            .expect("origin expiry should drop active provider work");
        let mut connection = gateway.connection_for_test().await.unwrap();
        assert!(matches!(
            lifecycle::read_terminal(&mut connection, &id, &generation)
                .await
                .unwrap(),
            Some(BusMessage::Error(message)) if message == "distributed origin canceled"
        ));
        worker.abort();
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_quiet_stream_receiver_drop_cancels_active_delivery() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("origin-body-drop-{}", uuid::Uuid::new_v4().simple());
        let dispatch_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let mut gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(QuietStreamDispatch {
                dispatch_starts: dispatch_starts.clone(),
                dispatch_started: dispatch_started.clone(),
                release_streams: Arc::new(Semaphore::new(0)),
            }),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(90),
                request_timeout: Duration::from_secs(2),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        Arc::get_mut(&mut gateway).unwrap().origin_lease_timeout = Duration::from_secs(1);
        let workers = gateway.spawn_workers(vec![provider.clone()]);
        let first_dispatch = dispatch_started.notified();
        let receiver = gateway
            .submit_stream(GatewayRequest {
                provider: provider.clone(),
                tier: 0,
                permits: 1,
                payload: serde_json::json!({"request":"quiet"}),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), first_dispatch)
            .await
            .expect("quiet dispatch should start");
        let mut connection = gateway.connection_for_test().await.unwrap();
        let processing = protocol_processing_key(QueueProtocol::LegacyUnscoped, &provider);
        let members: Vec<String> = connection.zrange(&processing, 0, -1).await.unwrap();
        assert_eq!(members.len(), 1);
        let (id, generation) = members[0].rsplit_once(':').unwrap();
        let id = id.to_string();
        let generation = generation.to_string();
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if connection.zcard::<_, u64>(&processing).await.unwrap() == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("receiver drop should terminate the active delivery");
        assert!(matches!(
            lifecycle::read_terminal(&mut connection, &id, &generation)
                .await
                .unwrap(),
            Some(BusMessage::Error(message)) if message == "distributed origin canceled"
        ));
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        for worker in workers {
            worker.abort();
        }
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_unary_submit_future_drop_uses_bounded_cancel_pump() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("origin-unary-drop-{}", uuid::Uuid::new_v4().simple());
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let dispatch_dropped = Arc::new(tokio::sync::Notify::new());
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(PendingDispatch {
                dispatch_starts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                dispatch_started: dispatch_started.clone(),
                dispatch_drops: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                dispatch_dropped: dispatch_dropped.clone(),
            }),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(90),
                request_timeout: Duration::from_secs(2),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        let workers = gateway.spawn_workers(vec![provider.clone()]);
        let started = dispatch_started.notified();
        let submit_gateway = gateway.clone();
        let submit_provider = provider.clone();
        let submission = tokio::spawn(async move {
            submit_gateway
                .submit(GatewayRequest {
                    provider: submit_provider,
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"drop"}),
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), started)
            .await
            .expect("unary dispatch should start");
        let mut connection = gateway.connection_for_test().await.unwrap();
        let processing = protocol_processing_key(QueueProtocol::LegacyUnscoped, &provider);
        let members: Vec<String> = connection.zrange(&processing, 0, -1).await.unwrap();
        let (id, generation) = members[0].rsplit_once(':').unwrap();
        let id = id.to_string();
        let generation = generation.to_string();
        let dropped = dispatch_dropped.notified();
        submission.abort();
        let _ = submission.await;
        tokio::time::timeout(Duration::from_secs(1), dropped)
            .await
            .expect("origin drop should cancel provider work");
        assert!(matches!(
            lifecycle::read_terminal(&mut connection, &id, &generation)
                .await
                .unwrap(),
            Some(BusMessage::Error(message)) if message == "distributed origin canceled"
        ));
        for worker in workers {
            worker.abort();
        }
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_stream_progress_cannot_extend_total_request_timeout() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("origin-total-deadline-{}", uuid::Uuid::new_v4().simple());
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(ProgressStreamDispatch),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(90),
                request_timeout: Duration::from_millis(120),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        let workers = gateway.spawn_workers(vec![provider.clone()]);
        let started = tokio::time::Instant::now();
        let mut receiver = gateway
            .submit_stream(GatewayRequest {
                provider,
                tier: 0,
                permits: 1,
                payload: serde_json::json!({"request":"progress"}),
            })
            .await
            .unwrap();
        let mut progress_chunks = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
            {
                Some(Ok(chunk)) => {
                    assert_eq!(chunk, "progress");
                    progress_chunks += 1;
                }
                Some(Err(GatewayError::Timeout)) => break,
                other => panic!("unexpected stream result: {other:?}"),
            }
        }
        assert!(progress_chunks >= 3);
        assert!(started.elapsed() < Duration::from_millis(500));
        for worker in workers {
            worker.abort();
        }
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_heartbeat_prevents_quiet_stream_redelivery() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("lease-stream-{}", uuid::Uuid::new_v4().simple());
        let dispatch_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let release_streams = Arc::new(Semaphore::new(0));
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(QuietStreamDispatch {
                dispatch_starts: dispatch_starts.clone(),
                dispatch_started: dispatch_started.clone(),
                release_streams: release_streams.clone(),
            }),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(300),
                request_timeout: Duration::from_secs(3),
                max_concurrency_per_provider: 2,
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        let worker_handles = gateway.spawn_workers(vec![provider.clone()]);
        let first_dispatch_started = dispatch_started.notified();
        let mut receiver = gateway
            .submit_stream(GatewayRequest {
                provider: provider.clone(),
                tier: 0,
                permits: 1,
                payload: serde_json::json!({"request":"quiet stream"}),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), first_dispatch_started)
            .await
            .expect("stream dispatch should start");

        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(320)).await;
            assert_eq!(
                gateway
                    .reap_once_protocol(QueueProtocol::LegacyUnscoped, &provider)
                    .await,
                0,
                "a healthy quiet stream must refresh independently"
            );
        }
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 1);

        release_streams.add_permits(1);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .expect("stream should emit")
                .expect("stream should remain open")
                .expect("stream item should succeed"),
            "terminal"
        );
        for worker_handle in worker_handles {
            worker_handle.abort();
        }
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_worker_deadline_drops_dispatch_and_completes_only_owned_delivery() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("lease-deadline-{}", uuid::Uuid::new_v4().simple());
        let dispatch_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let dispatch_drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_dropped = Arc::new(tokio::sync::Notify::new());
        let mut gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(PendingDispatch {
                dispatch_starts: dispatch_starts.clone(),
                dispatch_started: dispatch_started.clone(),
                dispatch_drops: dispatch_drops.clone(),
                dispatch_dropped: dispatch_dropped.clone(),
            }),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(300),
                request_timeout: Duration::from_secs(2),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        let mutable_gateway = Arc::get_mut(&mut gateway).unwrap();
        mutable_gateway.worker_job_timeout = Duration::from_millis(290);
        mutable_gateway.terminal_operation_delay = Duration::from_millis(70);
        mutable_gateway.origin_lease_timeout = Duration::from_secs(2);
        let preparation_capacity = Arc::new(Semaphore::new(1));
        let worker = tokio::spawn(gateway.clone().worker(
            provider.clone(),
            QueueProtocol::LegacyUnscoped,
            preparation_capacity,
        ));
        let first_dispatch_started = dispatch_started.notified();
        let dropped = dispatch_dropped.notified();
        let submission_gateway = gateway.clone();
        let submission_provider = provider.clone();
        let submission = tokio::spawn(async move {
            submission_gateway
                .submit(GatewayRequest {
                    provider: submission_provider,
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"deadline"}),
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), first_dispatch_started)
            .await
            .expect("dispatch should start");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut connection = gateway.connection_for_test().await.unwrap();
        let processing = protocol_processing_key(QueueProtocol::LegacyUnscoped, &provider);
        let processing_members: Vec<String> = connection.zrange(&processing, 0, -1).await.unwrap();
        assert_eq!(processing_members.len(), 1);
        let processing_id = processing_members[0].rsplit_once(':').unwrap().0;
        let origin_expiry: u64 = connection
            .hget(lifecycle::meta_key(processing_id), "origin_expires_at_ms")
            .await
            .unwrap();
        assert!(
            origin_expiry
                > redis_server_time_ms(&mut connection)
                    .await
                    .saturating_add(1_000)
        );
        let near_expiry = redis_server_time_ms(&mut connection)
            .await
            .saturating_add(120);
        let _: () = connection
            .zadd(&processing, &processing_members[0], near_expiry)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(160)).await;
        assert_eq!(
            gateway
                .reap_once_protocol(QueueProtocol::LegacyUnscoped, &provider)
                .await,
            0,
            "deadline cleanup heartbeat must refresh a near-expiry delivery"
        );
        tokio::time::timeout(Duration::from_secs(1), dropped)
            .await
            .expect("worker deadline should drop dispatch");
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(dispatch_drops.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), submission)
                .await
                .expect("origin should receive deadline")
                .unwrap(),
            Err(GatewayError::Upstream(message)) if message == "distributed worker deadline exceeded"
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if connection.zcard::<_, u64>(&processing).await.unwrap() == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned deadline completion should clear processing");
        worker.abort();
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_heartbeat_ownership_loss_drops_work_without_stale_cleanup() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("lease-loss-{}", uuid::Uuid::new_v4().simple());
        let dispatch_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let dispatch_drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_dropped = Arc::new(tokio::sync::Notify::new());
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(PendingDispatch {
                dispatch_starts: dispatch_starts.clone(),
                dispatch_started: dispatch_started.clone(),
                dispatch_drops: dispatch_drops.clone(),
                dispatch_dropped: dispatch_dropped.clone(),
            }),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(300),
                request_timeout: Duration::from_secs(2),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        let preparation_capacity = Arc::new(Semaphore::new(1));
        let worker = tokio::spawn(gateway.clone().worker(
            provider.clone(),
            QueueProtocol::LegacyUnscoped,
            preparation_capacity,
        ));
        let first_dispatch_started = dispatch_started.notified();
        let dropped = dispatch_dropped.notified();
        let submission_gateway = gateway.clone();
        let submission_provider = provider.clone();
        let submission = tokio::spawn(async move {
            submission_gateway
                .submit(GatewayRequest {
                    provider: submission_provider,
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"ownership loss"}),
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), first_dispatch_started)
            .await
            .expect("dispatch should start");

        let processing = protocol_processing_key(QueueProtocol::LegacyUnscoped, &provider);
        let owners = protocol_owners_key(QueueProtocol::LegacyUnscoped, &provider);
        let mut connection = gateway.connection_for_test().await.unwrap();
        let members: Vec<String> = connection.zrange(&processing, 0, -1).await.unwrap();
        assert_eq!(members.len(), 1);
        let member = &members[0];
        let job_id = member.rsplit_once(':').unwrap().0.to_string();
        let _: () = connection
            .hset(&owners, member, "replacement-owner")
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), dropped)
            .await
            .expect("failed heartbeat should drop dispatch");
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(dispatch_drops.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(connection
            .zscore::<_, _, Option<f64>>(&processing, member)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            connection
                .hget::<_, _, String>(&owners, member)
                .await
                .unwrap(),
            "replacement-owner"
        );
        assert_eq!(
            connection
                .zcard::<_, u64>(protocol_queue_key(QueueProtocol::LegacyUnscoped, &provider,))
                .await
                .unwrap(),
            0
        );
        assert!(!connection
            .exists::<_, bool>(protocol_done_key(QueueProtocol::LegacyUnscoped, &job_id,))
            .await
            .unwrap());
        submission.abort();
        worker.abort();
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_worker_task_cancellation_drops_dispatch_and_leaves_owned_lease() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("lease-cancel-{}", uuid::Uuid::new_v4().simple());
        let dispatch_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let dispatch_drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_dropped = Arc::new(tokio::sync::Notify::new());
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(PendingDispatch {
                dispatch_starts: dispatch_starts.clone(),
                dispatch_started: dispatch_started.clone(),
                dispatch_drops: dispatch_drops.clone(),
                dispatch_dropped: dispatch_dropped.clone(),
            }),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_secs(2),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        let prepared = gateway
            .prepare_submission(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"cancel worker"}),
                },
                false,
                None,
            )
            .unwrap();
        let job_id = prepared.descriptor.id.clone();
        gateway.enqueue(&prepared).await.unwrap();
        let protocol = QueueProtocol::LegacyUnscoped;
        let queue = protocol_queue_key(protocol, &provider);
        let processing = protocol_processing_key(protocol, &provider);
        let leased = protocol_leased_key(protocol, &provider);
        let owners = protocol_owners_key(protocol, &provider);
        let owner_token = "cancel-owner";
        let mut connection = gateway.connection_for_test().await.unwrap();
        let leased_delivery: Option<(String, String, String)> = gateway
            .lease
            .key(&queue)
            .key(&processing)
            .key(&leased)
            .key(&owners)
            .key(lifecycle::counters())
            .key(lifecycle::terminal_reservations())
            .key(lifecycle::reservation_states())
            .key(lifecycle::expiry())
            .arg(2_000_u64)
            .arg(owner_token)
            .arg(gateway.lifecycle_limits.max_terminal_bytes)
            .arg(gateway.lifecycle_limits.unary_terminal_bytes)
            .arg(gateway.lifecycle_limits.stream_terminal_bytes)
            .arg(lifecycle::job_prefix())
            .arg(7 * 60 * 60 * 1_000_u64)
            .arg(6 * 60 * 60 * 1_000_u64)
            .invoke_async(&mut connection)
            .await
            .unwrap();
        let (member, original_score, serialized_payload) =
            leased_delivery.expect("job should lease");
        let ownership = LeaseOwnership {
            protocol,
            provider: provider.clone(),
            member: member.clone(),
            original_score,
            owner_token: owner_token.into(),
            serialized_payload,
        };
        let first_dispatch_started = dispatch_started.notified();
        let dropped = dispatch_dropped.notified();
        let task_gateway = gateway.clone();
        let task = tokio::spawn(async move {
            task_gateway
                .execute_leased_job(ownership, RateKey::provider(provider))
                .await;
        });
        tokio::time::timeout(Duration::from_secs(1), first_dispatch_started)
            .await
            .expect("dispatch should start");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), dropped)
            .await
            .expect("task cancellation should drop dispatch and heartbeat");
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(dispatch_drops.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(connection
            .zscore::<_, _, Option<f64>>(&processing, &member)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            connection
                .hget::<_, _, String>(&owners, &member)
                .await
                .unwrap(),
            owner_token
        );
        assert!(!connection
            .exists::<_, bool>(protocol_done_key(protocol, &job_id))
            .await
            .unwrap());
        assert_eq!(connection.zcard::<_, u64>(&queue).await.unwrap(), 0);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_worker_leases_only_available_preparation_capacity() {
        let Some(redis_url) = std::env::var("LLMSHIM_REDIS_URL").ok() else {
            return;
        };
        let provider = format!(
            "itest-preparation-capacity-{}",
            uuid::Uuid::new_v4().simple()
        );
        let client = redis::Client::open(redis_url.clone()).unwrap();
        let mut connection = ConnectionManager::new(client).await.unwrap();
        for key in [
            queue_key(&provider),
            processing_key(&provider),
            leased_key(&provider),
            protocol_owners_key(QueueProtocol::LegacyUnscoped, &provider),
        ] {
            let _: i64 = connection.del(key).await.unwrap();
        }

        let dispatch_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let first_dispatch_started = dispatch_started.notified();
        let release_dispatches = Arc::new(Semaphore::new(0));
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(LatchBlockedDispatch {
                dispatch_starts: dispatch_starts.clone(),
                dispatch_started: dispatch_started.clone(),
                release_dispatches: release_dispatches.clone(),
            }),
            unlimited(),
            GatewayConfig {
                max_concurrency_per_provider: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let mut submissions = Vec::new();
        for id in 0..3 {
            let gateway = gateway.clone();
            let provider = provider.clone();
            submissions.push(tokio::spawn(async move {
                gateway
                    .submit(GatewayRequest {
                        provider,
                        tier: 0,
                        permits: 1,
                        payload: serde_json::json!({"id": id}),
                    })
                    .await
            }));
        }
        for _ in 0..1_000 {
            let queued: u64 = connection.zcard(queue_key(&provider)).await.unwrap();
            if queued == 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            connection
                .zcard::<_, u64>(queue_key(&provider))
                .await
                .unwrap(),
            3
        );

        let worker_handles = gateway.spawn_workers(vec![provider.clone()]);
        tokio::time::timeout(Duration::from_secs(2), first_dispatch_started)
            .await
            .expect("first leased dispatch should start");
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            connection
                .zcard::<_, u64>(processing_key(&provider))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .hlen::<_, u64>(leased_key(&provider))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .zcard::<_, u64>(queue_key(&provider))
                .await
                .unwrap(),
            2
        );

        release_dispatches.add_permits(3);
        for submission in submissions {
            tokio::time::timeout(Duration::from_secs(2), submission)
                .await
                .expect("bounded dispatch should complete")
                .unwrap()
                .unwrap();
        }
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 3);
        for worker_handle in worker_handles {
            worker_handle.abort();
        }
        for key in [
            queue_key(&provider),
            processing_key(&provider),
            leased_key(&provider),
            protocol_owners_key(QueueProtocol::LegacyUnscoped, &provider),
        ] {
            let _: i64 = connection.del(key).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_scoped_and_legacy_workers_share_one_provider_capacity() {
        let Some(redis_url) = std::env::var("LLMSHIM_REDIS_URL").ok() else {
            return;
        };
        let provider = format!("mixed-capacity-{}", uuid::Uuid::new_v4().simple());
        let dispatch_starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_started = Arc::new(tokio::sync::Notify::new());
        let first_dispatch_started = dispatch_started.notified();
        let release_dispatches = Arc::new(Semaphore::new(0));
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(LatchBlockedDispatch {
                dispatch_starts: dispatch_starts.clone(),
                dispatch_started: dispatch_started.clone(),
                release_dispatches: release_dispatches.clone(),
            }),
            unlimited(),
            GatewayConfig {
                max_concurrency_per_provider: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let legacy = gateway
            .prepare_submission(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"kind":"legacy"}),
                },
                false,
                None,
            )
            .unwrap();
        let scoped = gateway
            .prepare_with_policy(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"kind":"scoped"}),
                },
                false,
                crate::gateway::attempt::TrustedPolicyScope::from_identity(
                    &crate::gateway::auth::Identity {
                        tenant: "synthetic-scoped".into(),
                        tier: 0,
                        rpm: None,
                        tpm: None,
                        budget_usd: None,
                        budget_window_secs: None,
                        budget_allow_unpriced: false,
                    },
                ),
            )
            .unwrap();
        gateway.enqueue(&legacy).await.unwrap();
        gateway.enqueue(&scoped).await.unwrap();

        let preparation_capacity = Arc::new(Semaphore::new(1));
        let legacy_worker = tokio::spawn(gateway.clone().worker(
            provider.clone(),
            QueueProtocol::LegacyUnscoped,
            preparation_capacity.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(2), first_dispatch_started)
            .await
            .expect("legacy dispatch should hold the shared capacity");
        let scoped_worker = tokio::spawn(gateway.clone().worker(
            provider.clone(),
            QueueProtocol::ScopedV1,
            preparation_capacity,
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut connection = gateway.connection_for_test().await.unwrap();
        assert_eq!(dispatch_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            connection
                .zcard::<_, u64>(protocol_queue_key(QueueProtocol::ScopedV1, &provider))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .zcard::<_, u64>(protocol_processing_key(QueueProtocol::ScopedV1, &provider))
                .await
                .unwrap(),
            0
        );

        legacy_worker.abort();
        scoped_worker.abort();
        release_dispatches.add_permits(1);
        for protocol in [QueueProtocol::LegacyUnscoped, QueueProtocol::ScopedV1] {
            for key in [
                protocol_queue_key(protocol, &provider),
                protocol_processing_key(protocol, &provider),
                protocol_leased_key(protocol, &provider),
                protocol_owners_key(protocol, &provider),
            ] {
                let _: i64 = connection.del(key).await.unwrap();
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_reaper_redelivers_expired_lease() {
        let provider = format!("itest-reap-{}", uuid::Uuid::new_v4().simple());
        let Some(gw) = test_gateway(&provider).await else {
            return;
        };
        let mut conn = gw.connection_for_test().await.unwrap();
        let (ownership, _) = lease_test_job(&gw, &provider, false, "crashed-owner").await;
        let member = ownership.member;
        let orig_score: f64 = ownership.original_score.parse().unwrap();
        let _: () = conn
            .zadd(processing_key(&provider), &member, 1_u64)
            .await
            .unwrap();

        let reaped = gw.reap_once(&provider).await;
        assert_eq!(reaped, 1, "expired lease should be redelivered");
        // It's back on the queue with its original score, and gone from processing.
        let qlen: u64 = conn.zcard(queue_key(&provider)).await.unwrap();
        let plen: u64 = conn.zcard(processing_key(&provider)).await.unwrap();
        assert_eq!(qlen, 1);
        assert_eq!(plen, 0);
        let score: f64 = conn.zscore(queue_key(&provider), member).await.unwrap();
        assert_eq!(score, orig_score);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_visibility_uses_coordinator_time_despite_client_clock_skew() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("lease-time-{}", uuid::Uuid::new_v4().simple());
        let gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(EchoDispatch),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(500),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        let prepared = gateway
            .prepare_submission(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"coordinator time"}),
                },
                false,
                None,
            )
            .unwrap();
        gateway.enqueue(&prepared).await.unwrap();

        let protocol = QueueProtocol::LegacyUnscoped;
        let queue = protocol_queue_key(protocol, &provider);
        let processing = protocol_processing_key(protocol, &provider);
        let leased = protocol_leased_key(protocol, &provider);
        let owners = protocol_owners_key(protocol, &provider);
        let owner_token = "coordinator-time-owner";
        let mut connection = gateway.connection_for_test().await.unwrap();
        let server_before_lease = redis_server_time_ms(&mut connection).await;
        let leased_delivery: Option<(String, String, String)> = gateway
            .lease
            .key(&queue)
            .key(&processing)
            .key(&leased)
            .key(&owners)
            .key(lifecycle::counters())
            .key(lifecycle::terminal_reservations())
            .key(lifecycle::reservation_states())
            .key(lifecycle::expiry())
            .arg(500_u64)
            .arg(owner_token)
            .arg(gateway.lifecycle_limits.max_terminal_bytes)
            .arg(gateway.lifecycle_limits.unary_terminal_bytes)
            .arg(gateway.lifecycle_limits.stream_terminal_bytes)
            .arg(lifecycle::job_prefix())
            .arg(7 * 60 * 60 * 1_000_u64)
            .arg(6 * 60 * 60 * 1_000_u64)
            .invoke_async(&mut connection)
            .await
            .unwrap();
        let (member, original_score, serialized_payload) =
            leased_delivery.expect("job should lease");
        let server_after_lease = redis_server_time_ms(&mut connection).await;
        let lease_deadline = connection
            .zscore::<_, _, f64>(&processing, &member)
            .await
            .unwrap() as u64;
        assert!(lease_deadline >= server_before_lease.saturating_add(500));
        assert!(lease_deadline <= server_after_lease.saturating_add(500));

        let ownership = LeaseOwnership {
            protocol,
            provider: provider.clone(),
            member: member.clone(),
            original_score,
            owner_token: owner_token.into(),
            serialized_payload,
        };
        let simulated_behind_client = server_before_lease.saturating_sub(86_400_000);
        let simulated_ahead_client = server_after_lease.saturating_add(86_400_000);
        assert!(simulated_behind_client < lease_deadline);
        assert!(simulated_ahead_client > lease_deadline);

        tokio::time::sleep(Duration::from_millis(25)).await;
        let server_before_refresh = redis_server_time_ms(&mut connection).await;
        assert_eq!(gateway.refresh_owned(&ownership).await, 1);
        let server_after_refresh = redis_server_time_ms(&mut connection).await;
        let refreshed_deadline = connection
            .zscore::<_, _, f64>(&processing, &member)
            .await
            .unwrap() as u64;
        assert!(refreshed_deadline >= server_before_refresh.saturating_add(500));
        assert!(refreshed_deadline <= server_after_refresh.saturating_add(500));
        assert_eq!(gateway.reap_once_protocol(protocol, &provider).await, 0);

        tokio::time::sleep(Duration::from_millis(525)).await;
        assert_eq!(gateway.reap_once_protocol(protocol, &provider).await, 1);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_stale_owner_cannot_mutate_or_publish_for_replacement_delivery() {
        let redis_url = std::env::var("LLMSHIM_REDIS_URL").expect("owned Redis fixture required");
        let provider = format!("lease-fence-{}", uuid::Uuid::new_v4().simple());
        let mut gateway = DistributedGateway::connect(
            &redis_url,
            Arc::new(EchoDispatch),
            unlimited(),
            GatewayConfig {
                lease_timeout: Duration::from_millis(400),
                ..GatewayConfig::default()
            },
        )
        .await
        .unwrap();
        Arc::get_mut(&mut gateway).unwrap().origin_lease_timeout = Duration::from_secs(2);
        let prepared = gateway
            .prepare_submission(
                GatewayRequest {
                    provider: provider.clone(),
                    tier: 0,
                    permits: 1,
                    payload: serde_json::json!({"request":"fenced"}),
                },
                false,
                None,
            )
            .unwrap();
        let job_id = prepared.descriptor.id.clone();
        gateway.enqueue(&prepared).await.unwrap();

        let protocol = QueueProtocol::LegacyUnscoped;
        let queue = protocol_queue_key(protocol, &provider);
        let processing = protocol_processing_key(protocol, &provider);
        let leased = protocol_leased_key(protocol, &provider);
        let owners = protocol_owners_key(protocol, &provider);
        let mut connection = gateway.connection_for_test().await.unwrap();
        let first_token = "first-owner";
        let first_lease: Option<(String, String, String)> = gateway
            .lease
            .key(&queue)
            .key(&processing)
            .key(&leased)
            .key(&owners)
            .key(lifecycle::counters())
            .key(lifecycle::terminal_reservations())
            .key(lifecycle::reservation_states())
            .key(lifecycle::expiry())
            .arg(400_u64)
            .arg(first_token)
            .arg(gateway.lifecycle_limits.max_terminal_bytes)
            .arg(gateway.lifecycle_limits.unary_terminal_bytes)
            .arg(gateway.lifecycle_limits.stream_terminal_bytes)
            .arg(lifecycle::job_prefix())
            .arg(7 * 60 * 60 * 1_000_u64)
            .arg(6 * 60 * 60 * 1_000_u64)
            .invoke_async(&mut connection)
            .await
            .unwrap();
        let (member, original_score, serialized_payload) =
            first_lease.expect("first delivery should lease");
        let stale_ownership = LeaseOwnership {
            protocol,
            provider: provider.clone(),
            member: member.clone(),
            original_score: original_score.clone(),
            owner_token: first_token.into(),
            serialized_payload,
        };

        tokio::time::sleep(Duration::from_millis(450)).await;
        assert_eq!(gateway.reap_once_protocol(protocol, &provider).await, 1);
        let replacement_token = "replacement-owner";
        let replacement_lease: Option<(String, String, String)> = gateway
            .lease
            .key(&queue)
            .key(&processing)
            .key(&leased)
            .key(&owners)
            .key(lifecycle::counters())
            .key(lifecycle::terminal_reservations())
            .key(lifecycle::reservation_states())
            .key(lifecycle::expiry())
            .arg(1_000_u64)
            .arg(replacement_token)
            .arg(gateway.lifecycle_limits.max_terminal_bytes)
            .arg(gateway.lifecycle_limits.unary_terminal_bytes)
            .arg(gateway.lifecycle_limits.stream_terminal_bytes)
            .arg(lifecycle::job_prefix())
            .arg(7 * 60 * 60 * 1_000_u64)
            .arg(6 * 60 * 60 * 1_000_u64)
            .invoke_async(&mut connection)
            .await
            .unwrap();
        assert!(replacement_lease.is_some());

        let response_channel = protocol_response_channel(protocol, &job_id);
        assert_eq!(gateway.refresh_owned(&stale_ownership).await, 0);
        assert!(
            !gateway
                .publish_owned(
                    &stale_ownership,
                    &response_channel,
                    &BusMessage::Unary(serde_json::json!({"stale":true})),
                )
                .await
        );
        assert!(!gateway.complete_owned(&stale_ownership, &job_id).await);
        assert!(!gateway.dead_letter_owned(&stale_ownership).await);
        assert!(!gateway.release_owned(&stale_ownership).await);
        assert!(!gateway.ack_owned(&stale_ownership).await);

        assert!(connection
            .zscore::<_, _, Option<f64>>(&processing, &member)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            connection
                .hget::<_, _, String>(&owners, &member)
                .await
                .unwrap(),
            replacement_token
        );
        assert!(!connection
            .exists::<_, bool>(protocol_done_key(protocol, &job_id))
            .await
            .unwrap());
        assert_eq!(
            connection
                .llen::<_, u64>(protocol_dlq_key(protocol, &provider))
                .await
                .unwrap(),
            0
        );
        assert_eq!(connection.zcard::<_, u64>(&queue).await.unwrap(), 0);

        let replacement_ownership = LeaseOwnership {
            protocol,
            provider: provider.clone(),
            member,
            original_score,
            owner_token: replacement_token.into(),
            serialized_payload: "{}".into(),
        };
        assert_eq!(gateway.refresh_owned(&replacement_ownership).await, 1);
        assert!(gateway.ack_owned(&replacement_ownership).await);
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_dedup_and_dead_letter_markers() {
        let provider = format!("itest-dedup-{}", uuid::Uuid::new_v4().simple());
        let Some(gw) = test_gateway(&provider).await else {
            return;
        };
        let mut conn = gw.connection_for_test().await.unwrap();
        for k in [
            protocol_done_key(QueueProtocol::LegacyUnscoped, "job-x"),
            protocol_attempts_key(QueueProtocol::LegacyUnscoped, "job-y"),
            protocol_dlq_key(QueueProtocol::LegacyUnscoped, &provider),
            released_dlq_key(QueueProtocol::LegacyUnscoped, &provider),
            released_dlq_key(QueueProtocol::ScopedV1, &provider),
            protocol_processing_key(QueueProtocol::LegacyUnscoped, &provider),
            protocol_leased_key(QueueProtocol::LegacyUnscoped, &provider),
            protocol_owners_key(QueueProtocol::LegacyUnscoped, &provider),
        ] {
            let _: Result<i64, _> = conn.del(k).await;
        }

        // Idempotency marker.
        assert_eq!(
            gw.is_done(QueueProtocol::LegacyUnscoped, "job-x").await,
            Some(false)
        );
        let completed_member = "completed-member";
        let completed_ownership = LeaseOwnership {
            protocol: QueueProtocol::LegacyUnscoped,
            provider: provider.clone(),
            member: completed_member.into(),
            original_score: "0".into(),
            owner_token: "completed-owner".into(),
            serialized_payload: "{}".into(),
        };
        let _: () = conn
            .zadd(
                protocol_processing_key(QueueProtocol::LegacyUnscoped, &provider),
                completed_member,
                now_ms().saturating_add(1_000),
            )
            .await
            .unwrap();
        let _: () = conn
            .hset(
                protocol_leased_key(QueueProtocol::LegacyUnscoped, &provider),
                completed_member,
                0,
            )
            .await
            .unwrap();
        let _: () = conn
            .hset(
                protocol_owners_key(QueueProtocol::LegacyUnscoped, &provider),
                completed_member,
                "completed-owner",
            )
            .await
            .unwrap();
        assert!(gw.complete_owned(&completed_ownership, "job-x").await);
        assert_eq!(
            gw.is_done(QueueProtocol::LegacyUnscoped, "job-x").await,
            Some(true)
        );

        // Attempt counter increments per delivery.
        assert_eq!(
            gw.bump_attempts(QueueProtocol::LegacyUnscoped, "job-y")
                .await,
            Some(1)
        );
        assert_eq!(
            gw.bump_attempts(QueueProtocol::LegacyUnscoped, "job-y")
                .await,
            Some(2)
        );

        // Dead-letter transition records authoritative cleanup ownership.
        assert_eq!(gw.dead_letter_len(&provider).await, 0);
        let baseline_jobs = conn
            .hget::<_, _, i64>(lifecycle::counters(), "jobs")
            .await
            .unwrap_or_default();
        let baseline_base = conn
            .hget::<_, _, i64>(lifecycle::counters(), "base_bytes")
            .await
            .unwrap_or_default();
        let (poison_ownership, poison_id) =
            lease_test_job(&gw, &provider, false, "poison-owner").await;
        assert!(gw.dead_letter_owned(&poison_ownership).await);
        assert_eq!(gw.dead_letter_len(&provider).await, 1);
        assert_eq!(
            conn.hget::<_, _, String>(lifecycle::reservation_states(), &poison_ownership.member,)
                .await
                .unwrap(),
            "dlq"
        );
        assert!(
            conn.hget::<_, _, u64>(lifecycle::dlq_reservations(), &poison_ownership.member)
                .await
                .unwrap()
                > 0
        );
        let _: i64 = conn
            .del((
                lifecycle::meta_key(&poison_id),
                lifecycle::payload_key(&poison_id),
                lifecycle::terminal_key(&poison_id),
            ))
            .await
            .unwrap();
        let _: () = conn
            .zadd(
                lifecycle::expiry(),
                &poison_ownership.member,
                -9_000_000_000_000_000_i64,
            )
            .await
            .unwrap();
        assert_eq!(lifecycle::cleanup_expired(&mut conn, 1).await.unwrap(), 1);
        assert_eq!(gw.dead_letter_len(&provider).await, 0);
        assert_eq!(
            conn.hget::<_, _, i64>(lifecycle::counters(), "jobs")
                .await
                .unwrap_or_default(),
            baseline_jobs
        );
        assert_eq!(
            conn.hget::<_, _, i64>(lifecycle::counters(), "base_bytes")
                .await
                .unwrap_or_default(),
            baseline_base
        );
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_client_idempotency_binds_owner_and_request() {
        let provider = "itest-client-idempotency";
        let Some(gateway) = test_gateway(provider).await else {
            return;
        };
        let original_context = crate::gateway::idempotency::IdempotencyContext::new(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            "client-key",
            &serde_json::json!({"prompt": "one"}),
        );
        let changed_request_context = crate::gateway::idempotency::IdempotencyContext::new(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            "client-key",
            &serde_json::json!({"prompt": "two"}),
        );
        let other_credential_context = crate::gateway::idempotency::IdempotencyContext::new(
            "tenant-a",
            "credential-b",
            "/v1/chat",
            "client-key",
            &serde_json::json!({"prompt": "one"}),
        );
        let reassigned_tenant_context = crate::gateway::idempotency::IdempotencyContext::new(
            "tenant-b",
            "credential-a",
            "/v1/chat",
            "client-key",
            &serde_json::json!({"prompt": "one"}),
        );
        let mut connection = gateway.connection_for_test().await.unwrap();
        let scoped_storage_key = scoped_idempotency_key(&original_context);
        let unsafe_legacy_key = "llmshim:gw:idem:client-key";
        let _: Result<i64, _> = connection.del(&scoped_storage_key).await;
        let _: Result<i64, _> = connection.del(unsafe_legacy_key).await;

        assert_eq!(
            gateway.scoped_idem_lookup(&original_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );
        let lifecycle_reference = LifecycleReference {
            job_id: format!("idem-pointer-{}", uuid::Uuid::new_v4().simple()),
            generation: "1".into(),
        };
        let _: () = connection
            .set(
                lifecycle::terminal_key(&lifecycle_reference.job_id),
                serde_json::to_vec(&BusMessage::Unary(serde_json::json!({
                    "private": "response"
                })))
                .unwrap(),
            )
            .await
            .unwrap();
        let _: () = redis::cmd("HSET")
            .arg(lifecycle::meta_key(&lifecycle_reference.job_id))
            .arg("generation")
            .arg(&lifecycle_reference.generation)
            .arg("state")
            .arg("terminal")
            .query_async(&mut connection)
            .await
            .unwrap();
        gateway
            .scoped_idem_store(&original_context, &lifecycle_reference, 60)
            .await;
        assert_eq!(
            gateway.scoped_idem_lookup(&original_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Replay(
                serde_json::json!({"private": "response"})
            )
        );
        assert_eq!(
            gateway.scoped_idem_lookup(&changed_request_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Conflict
        );
        assert_eq!(
            gateway.scoped_idem_lookup(&other_credential_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );
        assert_eq!(
            gateway.scoped_idem_lookup(&reassigned_tenant_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );

        let _: () = connection
            .set_ex(
                unsafe_legacy_key,
                serde_json::json!({"private": "legacy-response"}).to_string(),
                60,
            )
            .await
            .unwrap();
        let _: i64 = connection.del(&scoped_storage_key).await.unwrap();
        let _: i64 = connection
            .del(lifecycle::terminal_key(&lifecycle_reference.job_id))
            .await
            .unwrap();
        assert_eq!(
            gateway.scoped_idem_lookup(&original_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );

        let _: () = connection
            .set_ex(&scoped_storage_key, "not-json", 60)
            .await
            .unwrap();
        assert_eq!(
            gateway.scoped_idem_lookup(&original_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );

        let unknown_version = serde_json::json!({
            "version": 2,
            "request_fingerprint": "unused",
            "response": {"private": "unknown-version"}
        });
        let _: () = connection
            .set_ex(&scoped_storage_key, unknown_version.to_string(), 60)
            .await
            .unwrap();
        assert_eq!(
            gateway.scoped_idem_lookup(&original_context).await,
            crate::gateway::idempotency::IdempotencyLookup::Miss
        );
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    #[allow(deprecated)]
    async fn redis_generic_idempotency_api_uses_an_isolated_namespace() {
        let provider = "itest-generic-idempotency";
        let Some(gateway) = test_gateway(provider).await else {
            return;
        };
        let client_key = format!("generic-{}", uuid::Uuid::new_v4());
        let mut connection = gateway.connection_for_test().await.unwrap();
        let generic_key = generic_idempotency_key(&client_key);
        let unsafe_legacy_key = format!("llmshim:gw:idem:{client_key}");
        let _: Result<i64, _> = connection.del(&generic_key).await;
        let _: Result<i64, _> = connection.del(&unsafe_legacy_key).await;

        gateway
            .idem_put(&client_key, &serde_json::json!({"generic": true}), 60)
            .await;
        assert_eq!(
            gateway.idem_get(&client_key).await,
            Some(serde_json::json!({"generic": true}))
        );
        assert!(!connection.exists::<_, bool>(&generic_key).await.unwrap());
        assert!(!connection
            .exists::<_, bool>(&unsafe_legacy_key)
            .await
            .unwrap());
    }

    #[tokio::test]
    #[ignore = "requires LLMSHIM_REDIS_URL"]
    async fn redis_zpopmin_orders_by_tier_then_age() {
        let provider = "itest-order";
        let Some(gw) = test_gateway(provider).await else {
            return;
        };
        let mut conn = gw.connection_for_test().await.unwrap();
        let key = queue_key(provider);
        // tier 1 (older, newer), tier 3.
        for (member, tier, ms) in [("a", 1u8, 1000u64), ("b", 1, 2000), ("c", 3, 3000)] {
            let _: () = conn
                .zadd(&key, member, deadline_score(tier, ms, STEP))
                .await
                .unwrap();
        }
        let mut got = Vec::new();
        for _ in 0..3 {
            let popped: Vec<(String, f64)> = conn.zpopmin(&key, 1).await.unwrap();
            got.push(popped[0].0.clone());
        }
        // tier 3 first, then tier 1 in FIFO (a before b).
        assert_eq!(got, vec!["c", "a", "b"]);
    }
}

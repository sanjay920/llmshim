//! Request-bound idempotency for completed unary gateway responses.

use std::collections::{BTreeSet, HashMap};
use std::mem::size_of;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

const CACHE_ENTRY_VERSION: u8 = 1;
const DEFAULT_MAX_ENTRIES: usize = 100_000;
const DEFAULT_MAX_ENTRY_RETAINED_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_RETAINED_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_ENTRY_RETAINED_NODES: usize = 16_384;
const DEFAULT_MAX_RETAINED_NODES: usize = 1_000_000;
const JSON_OBJECT_MEMBER_OVERHEAD_BYTES: usize = 64;
const EXPIRY_INDEX_MEMBER_OVERHEAD_BYTES: usize = 64;

/// Credential, route, client key, and canonical request binding for one replay.
pub(crate) struct IdempotencyContext {
    storage_key: String,
    request_fingerprint: String,
}

impl IdempotencyContext {
    pub fn new(
        tenant_scope: &str,
        credential_scope: &str,
        route_scope: &str,
        client_key: &str,
        canonical_request: &Value,
    ) -> Self {
        Self {
            storage_key: digest_parts(
                b"llmshim-gateway-idempotency-owner-v1",
                &[
                    tenant_scope.as_bytes(),
                    credential_scope.as_bytes(),
                    route_scope.as_bytes(),
                    client_key.as_bytes(),
                ],
            ),
            request_fingerprint: digest_parts(
                b"llmshim-gateway-idempotency-request-v1",
                &[
                    route_scope.as_bytes(),
                    canonical_request.to_string().as_bytes(),
                ],
            ),
        }
    }
    pub fn storage_key(&self) -> &str {
        &self.storage_key
    }
}

pub(crate) fn generic_storage_key(client_key: &str) -> String {
    digest_parts(
        b"llmshim-gateway-idempotency-generic-v1",
        &[client_key.as_bytes()],
    )
}

fn digest_parts(domain: &[u8], parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

/// Result of looking up an idempotency key for a specific request.
#[derive(Debug, PartialEq)]
pub(crate) enum IdempotencyLookup {
    Miss,
    Replay(Value),
    Conflict,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CachedResponse {
    version: u8,
    request_fingerprint: String,
    response: Value,
}

impl CachedResponse {
    pub fn new(context: &IdempotencyContext, response: Value) -> Self {
        Self {
            version: CACHE_ENTRY_VERSION,
            request_fingerprint: context.request_fingerprint.clone(),
            response,
        }
    }
    pub fn lookup(&self, context: &IdempotencyContext) -> IdempotencyLookup {
        if self.version != CACHE_ENTRY_VERSION {
            return IdempotencyLookup::Miss;
        }
        if self.request_fingerprint != context.request_fingerprint {
            return IdempotencyLookup::Conflict;
        }
        IdempotencyLookup::Replay(self.response.clone())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RetainedFootprint {
    bytes: usize,
    nodes: usize,
}

impl RetainedFootprint {
    fn add_bytes(&mut self, bytes: usize, limit: usize) -> bool {
        match self.bytes.checked_add(bytes) {
            Some(total) if total <= limit => {
                self.bytes = total;
                true
            }
            _ => false,
        }
    }
    fn add_node(&mut self, limit: usize) -> bool {
        match self.nodes.checked_add(1) {
            Some(total) if total <= limit => {
                self.nodes = total;
                true
            }
            _ => false,
        }
    }
    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            bytes: self.bytes.checked_add(other.bytes)?,
            nodes: self.nodes.checked_add(other.nodes)?,
        })
    }
    fn without(self, removed: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_sub(removed.bytes),
            nodes: self.nodes.saturating_sub(removed.nodes),
        }
    }
    fn fits_within(self, limits: CacheLimits) -> bool {
        self.bytes <= limits.max_retained_bytes && self.nodes <= limits.max_retained_nodes
    }
}

/// Conservatively estimates a JSON value without serializing or cloning it.
fn estimate_value_footprint(
    value: &Value,
    max_bytes: usize,
    max_nodes: usize,
) -> Option<RetainedFootprint> {
    let mut footprint = RetainedFootprint::default();
    let mut pending = vec![value];
    while let Some(current) = pending.pop() {
        if !footprint.add_node(max_nodes) || !footprint.add_bytes(size_of::<Value>(), max_bytes) {
            return None;
        }
        match current {
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
            Value::String(text) => {
                if !footprint.add_bytes(text.capacity(), max_bytes) {
                    return None;
                }
            }
            Value::Array(values) => {
                let bytes = values
                    .capacity()
                    .checked_mul(size_of::<Value>())?
                    .checked_add(size_of::<Vec<Value>>())?;
                if !footprint.add_bytes(bytes, max_bytes) {
                    return None;
                }
                if values.len() > max_nodes.saturating_sub(pending.len()) {
                    return None;
                }
                pending.extend(values);
            }
            Value::Object(values) => {
                if !footprint.add_bytes(
                    values
                        .len()
                        .checked_mul(JSON_OBJECT_MEMBER_OVERHEAD_BYTES)?,
                    max_bytes,
                ) {
                    return None;
                }
                if values.len() > max_nodes.saturating_sub(pending.len()) {
                    return None;
                }
                for (key, child) in values {
                    if !footprint.add_bytes(key.capacity(), max_bytes) {
                        return None;
                    }
                    pending.push(child);
                }
            }
        }
    }
    Some(footprint)
}

#[derive(Clone, Copy)]
struct CacheLimits {
    max_entries: usize,
    max_entry_retained_bytes: usize,
    max_retained_bytes: usize,
    max_entry_retained_nodes: usize,
    max_retained_nodes: usize,
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_entry_retained_bytes: DEFAULT_MAX_ENTRY_RETAINED_BYTES,
            max_retained_bytes: DEFAULT_MAX_RETAINED_BYTES,
            max_entry_retained_nodes: DEFAULT_MAX_ENTRY_RETAINED_NODES,
            max_retained_nodes: DEFAULT_MAX_RETAINED_NODES,
        }
    }
}

struct CacheEntry<T> {
    value: T,
    expires_at: Instant,
    footprint: RetainedFootprint,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum CacheNamespace {
    Scoped,
    Generic,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct ExpiryRecord {
    expires_at: Instant,
    namespace: CacheNamespace,
    storage_key: String,
}

#[derive(Default)]
struct IdempotencyCacheState {
    scoped_entries: HashMap<String, CacheEntry<CachedResponse>>,
    generic_entries: HashMap<String, CacheEntry<Value>>,
    expiry_index: BTreeSet<ExpiryRecord>,
    retained_footprint: RetainedFootprint,
}

impl IdempotencyCacheState {
    fn purge_expired(&mut self, now: Instant) {
        while self
            .expiry_index
            .first()
            .is_some_and(|record| record.expires_at <= now)
        {
            let record = self
                .expiry_index
                .pop_first()
                .expect("the expiry index was checked under the cache lock");
            match record.namespace {
                CacheNamespace::Scoped => self.remove_scoped(&record.storage_key),
                CacheNamespace::Generic => self.remove_generic(&record.storage_key),
            }
        }
    }
    fn entry_count(&self) -> usize {
        self.scoped_entries.len() + self.generic_entries.len()
    }
    fn can_retain(
        &self,
        candidate: RetainedFootprint,
        replaced: Option<RetainedFootprint>,
        limits: CacheLimits,
    ) -> bool {
        self.retained_footprint
            .without(replaced.unwrap_or_default())
            .checked_add(candidate)
            .is_some_and(|total| total.fits_within(limits))
    }
    fn remove_scoped(&mut self, key: &str) {
        if let Some((storage_key, entry)) = self.scoped_entries.remove_entry(key) {
            self.expiry_index.remove(&ExpiryRecord {
                expires_at: entry.expires_at,
                namespace: CacheNamespace::Scoped,
                storage_key,
            });
            self.retained_footprint = self.retained_footprint.without(entry.footprint);
        }
    }
    fn remove_generic(&mut self, key: &str) {
        if let Some((storage_key, entry)) = self.generic_entries.remove_entry(key) {
            self.expiry_index.remove(&ExpiryRecord {
                expires_at: entry.expires_at,
                namespace: CacheNamespace::Generic,
                storage_key,
            });
            self.retained_footprint = self.retained_footprint.without(entry.footprint);
        }
    }
}

fn scoped_entry_footprint(
    context: &IdempotencyContext,
    response: &Value,
    limits: CacheLimits,
) -> Option<RetainedFootprint> {
    let mut footprint = estimate_value_footprint(
        response,
        limits.max_entry_retained_bytes,
        limits.max_entry_retained_nodes,
    )?;
    for bytes in [
        size_of::<CacheEntry<CachedResponse>>(),
        size_of::<String>(),
        size_of::<ExpiryRecord>(),
        EXPIRY_INDEX_MEMBER_OVERHEAD_BYTES,
        context.storage_key.capacity(),
        context.storage_key.capacity(),
        context.request_fingerprint.capacity(),
    ] {
        if !footprint.add_bytes(bytes, limits.max_entry_retained_bytes) {
            return None;
        }
    }
    Some(footprint)
}

fn generic_entry_footprint(
    key: &String,
    response: &Value,
    limits: CacheLimits,
) -> Option<RetainedFootprint> {
    let mut footprint = estimate_value_footprint(
        response,
        limits.max_entry_retained_bytes,
        limits.max_entry_retained_nodes,
    )?;
    for retained_bytes in [
        size_of::<CacheEntry<Value>>(),
        size_of::<String>(),
        size_of::<ExpiryRecord>(),
        EXPIRY_INDEX_MEMBER_OVERHEAD_BYTES,
        key.capacity(),
        key.capacity(),
    ] {
        if !footprint.add_bytes(retained_bytes, limits.max_entry_retained_bytes) {
            return None;
        }
    }
    Some(footprint)
}

/// A process-local, bounded, best-effort cache of completed request-bound responses.
pub struct IdempotencyCache {
    state: Mutex<IdempotencyCacheState>,
    ttl: Duration,
    limits: CacheLimits,
}

impl IdempotencyCache {
    pub fn new(ttl: Duration) -> Self {
        Self::with_limits(ttl, CacheLimits::default())
    }
    fn with_limits(ttl: Duration, limits: CacheLimits) -> Self {
        Self {
            state: Mutex::new(IdempotencyCacheState::default()),
            ttl,
            limits,
        }
    }
    pub(crate) fn lookup(&self, context: &IdempotencyContext) -> IdempotencyLookup {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        match state.scoped_entries.get(context.storage_key()) {
            Some(entry) if entry.expires_at <= now => {
                state.remove_scoped(context.storage_key());
                IdempotencyLookup::Miss
            }
            Some(entry) => entry.value.lookup(context),
            None => IdempotencyLookup::Miss,
        }
    }
    pub(crate) fn store(&self, context: &IdempotencyContext, response: Value) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        state.purge_expired(now);
        let Some(footprint) = scoped_entry_footprint(context, &response, self.limits) else {
            return;
        };
        let replaced = state
            .scoped_entries
            .get(context.storage_key())
            .map(|entry| entry.footprint);
        if (replaced.is_none() && state.entry_count() >= self.limits.max_entries)
            || !state.can_retain(footprint, replaced, self.limits)
        {
            return;
        }
        state.remove_scoped(context.storage_key());
        state.retained_footprint = state
            .retained_footprint
            .checked_add(footprint)
            .expect("idempotency cache footprint was checked before insertion");
        let storage_key = context.storage_key().to_string();
        let expires_at = now + self.ttl;
        state.expiry_index.insert(ExpiryRecord {
            expires_at,
            namespace: CacheNamespace::Scoped,
            storage_key: storage_key.clone(),
        });
        state.scoped_entries.insert(
            storage_key,
            CacheEntry {
                value: CachedResponse::new(context, response),
                expires_at,
                footprint,
            },
        );
    }
    /// Generic process-local cache lookup retained for API compatibility.
    #[deprecated(note = "gateway HTTP replay uses credential-scoped idempotency")]
    pub fn get(&self, key: &str) -> Option<Value> {
        let storage_key = generic_storage_key(key);
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        match state.generic_entries.get(&storage_key) {
            Some(entry) if entry.expires_at <= now => {
                state.remove_generic(&storage_key);
                None
            }
            Some(entry) => Some(entry.value.clone()),
            None => None,
        }
    }
    /// Generic process-local cache insertion retained for API compatibility.
    #[deprecated(note = "gateway HTTP replay uses credential-scoped idempotency")]
    pub fn put(&self, key: &str, value: Value) {
        let storage_key = generic_storage_key(key);
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        state.purge_expired(now);
        let Some(footprint) = generic_entry_footprint(&storage_key, &value, self.limits) else {
            return;
        };
        let replaced = state
            .generic_entries
            .get(&storage_key)
            .map(|entry| entry.footprint);
        if (replaced.is_none() && state.entry_count() >= self.limits.max_entries)
            || !state.can_retain(footprint, replaced, self.limits)
        {
            return;
        }
        state.remove_generic(&storage_key);
        state.retained_footprint = state
            .retained_footprint
            .checked_add(footprint)
            .expect("idempotency cache footprint was checked before insertion");
        let expires_at = now + self.ttl;
        state.expiry_index.insert(ExpiryRecord {
            expires_at,
            namespace: CacheNamespace::Generic,
            storage_key: storage_key.clone(),
        });
        state.generic_entries.insert(
            storage_key,
            CacheEntry {
                value,
                expires_at,
                footprint,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context(tenant: &str, credential: &str, route: &str, request: Value) -> IdempotencyContext {
        IdempotencyContext::new(tenant, credential, route, "client-key", &request)
    }
    fn small_limits(max_entries: usize) -> CacheLimits {
        CacheLimits {
            max_entries,
            max_entry_retained_bytes: 1_024,
            max_retained_bytes: 1_024,
            max_entry_retained_nodes: 64,
            max_retained_nodes: 64,
        }
    }
    fn stats(cache: &IdempotencyCache) -> (usize, usize, RetainedFootprint) {
        let state = cache.state.lock().unwrap();
        assert_eq!(state.expiry_index.len(), state.entry_count());
        (
            state.scoped_entries.len(),
            state.generic_entries.len(),
            state.retained_footprint,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn matching_request_replays_then_expires() {
        let cache = IdempotencyCache::new(Duration::from_secs(60));
        let request = context(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "one"}),
        );
        assert_eq!(cache.lookup(&request), IdempotencyLookup::Miss);
        cache.store(&request, json!({"response": 1}));
        assert_eq!(
            cache.lookup(&request),
            IdempotencyLookup::Replay(json!({"response": 1}))
        );
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(cache.lookup(&request), IdempotencyLookup::Miss);
    }

    #[test]
    fn same_key_isolated_by_credential_route_and_request() {
        let cache = IdempotencyCache::new(Duration::from_secs(60));
        let original = context(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "one"}),
        );
        cache.store(&original, json!({"private": "response"}));
        let other_tenant = context(
            "tenant-b",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "one"}),
        );
        let other_credential = context(
            "tenant-a",
            "credential-b",
            "/v1/chat",
            json!({"prompt": "one"}),
        );
        let other_route = context(
            "tenant-a",
            "credential-a",
            "/v1/chat/completions",
            json!({"prompt": "one"}),
        );
        let changed_request = context(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "two"}),
        );
        assert_eq!(cache.lookup(&other_tenant), IdempotencyLookup::Miss);
        assert_eq!(cache.lookup(&other_credential), IdempotencyLookup::Miss);
        assert_eq!(cache.lookup(&other_route), IdempotencyLookup::Miss);
        assert_eq!(cache.lookup(&changed_request), IdempotencyLookup::Conflict);
    }

    #[tokio::test(start_paused = true)]
    #[allow(deprecated)]
    async fn generic_public_cache_api_remains_separate_and_expires() {
        let cache = IdempotencyCache::new(Duration::from_secs(60));
        cache.put("client-key", json!({"generic": true}));
        assert_eq!(cache.get("client-key"), Some(json!({"generic": true})));
        let scoped = context(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "one"}),
        );
        assert_eq!(cache.lookup(&scoped), IdempotencyLookup::Miss);
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(cache.get("client-key"), None);
    }

    #[test]
    fn unknown_cached_response_version_fails_closed() {
        let context = context(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "one"}),
        );
        let cached = CachedResponse {
            version: CACHE_ENTRY_VERSION + 1,
            request_fingerprint: context.request_fingerprint.clone(),
            response: json!({"private": "response"}),
        };
        assert_eq!(cached.lookup(&context), IdempotencyLookup::Miss);
    }

    #[test]
    #[allow(deprecated)]
    fn all_live_entries_stop_at_shared_count_limit_and_namespaces_remain_separate() {
        let cache = IdempotencyCache::with_limits(Duration::from_secs(60), small_limits(2));
        let first = context(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "one"}),
        );
        let second = context(
            "tenant-b",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "two"}),
        );
        cache.store(&first, json!({"response": 1}));
        cache.put("generic", json!({"response": 2}));
        cache.store(&second, json!({"response": 3}));
        assert_eq!(stats(&cache).0 + stats(&cache).1, 2);
        assert_eq!(
            cache.lookup(&first),
            IdempotencyLookup::Replay(json!({"response": 1}))
        );
        assert_eq!(cache.lookup(&second), IdempotencyLookup::Miss);
        assert_eq!(cache.get("generic"), Some(json!({"response": 2})));
    }

    #[test]
    #[allow(deprecated)]
    fn replacement_releases_old_retained_footprint() {
        let cache = IdempotencyCache::with_limits(Duration::from_secs(60), small_limits(2));
        cache.put("generic", json!({"text": "a"}));
        let before = stats(&cache).2;
        cache.put("generic", json!({"text": "b"}));
        assert_eq!(stats(&cache).0 + stats(&cache).1, 1);
        assert_eq!(stats(&cache).2, before);
        assert_eq!(cache.get("generic"), Some(json!({"text": "b"})));
    }

    #[tokio::test(start_paused = true)]
    #[allow(deprecated)]
    async fn expiry_index_keeps_one_record_per_current_entry() {
        let cache = IdempotencyCache::with_limits(Duration::from_secs(5), small_limits(2));
        let scoped_context = context(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "one"}),
        );
        cache.put("refreshed", json!(0));
        tokio::time::advance(Duration::from_secs(1)).await;
        cache.store(&scoped_context, json!(2));
        tokio::time::advance(Duration::from_secs(1)).await;
        for response_value in 0..8 {
            cache.put("refreshed", json!(response_value));
            assert_eq!(stats(&cache).0 + stats(&cache).1, 2);
        }

        tokio::time::advance(Duration::from_secs(3)).await;
        cache.put("full-cache", json!(3));
        assert_eq!(cache.get("full-cache"), None);
        assert_eq!(cache.get("refreshed"), Some(json!(7)));
        assert_eq!(
            cache.lookup(&scoped_context),
            IdempotencyLookup::Replay(json!(2))
        );
        assert_eq!(stats(&cache).0 + stats(&cache).1, 2);

        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(cache.lookup(&scoped_context), IdempotencyLookup::Miss);
        assert_eq!(stats(&cache).0 + stats(&cache).1, 1);

        tokio::time::advance(Duration::from_secs(1)).await;
        cache.put("after-expiry", json!(4));
        assert_eq!(cache.get("refreshed"), None);
        assert_eq!(cache.get("after-expiry"), Some(json!(4)));
        assert_eq!(stats(&cache).0 + stats(&cache).1, 1);
    }

    #[tokio::test(start_paused = true)]
    #[allow(deprecated)]
    async fn expiry_frees_shared_capacity_and_footprint() {
        let cache = IdempotencyCache::with_limits(Duration::from_secs(5), small_limits(1));
        cache.put("generic", json!({"response": 1}));
        assert!(stats(&cache).2.bytes > 0);
        tokio::time::advance(Duration::from_secs(6)).await;
        let scoped = context(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "one"}),
        );
        cache.store(&scoped, json!({"response": 2}));
        assert_eq!(stats(&cache).0 + stats(&cache).1, 1);
        assert_eq!(
            cache.lookup(&scoped),
            IdempotencyLookup::Replay(json!({"response": 2}))
        );
    }

    #[test]
    #[allow(deprecated)]
    fn entry_and_aggregate_payload_limits_skip_retention() {
        let short_response = json!({"text": "short"});
        let short_response_footprint = generic_entry_footprint(
            &generic_storage_key("first"),
            &short_response,
            small_limits(4),
        )
        .unwrap();
        let cache_limits = CacheLimits {
            max_entry_retained_bytes: short_response_footprint.bytes + 1,
            max_retained_bytes: short_response_footprint.bytes * 2,
            ..small_limits(4)
        };
        let cache = IdempotencyCache::with_limits(Duration::from_secs(60), cache_limits);
        cache.put(
            "too-large",
            json!({"text": "this entry is intentionally over the tiny test limit"}),
        );
        assert_eq!(cache.get("too-large"), None);
        cache.put("first", short_response.clone());
        cache.put("second", short_response.clone());
        cache.put("third", short_response);
        assert_eq!(stats(&cache).1, 2);
        assert!(stats(&cache).2.bytes <= cache_limits.max_retained_bytes);
        assert!(stats(&cache).2.nodes <= cache_limits.max_retained_nodes);
    }

    #[test]
    fn deep_values_stop_at_complexity_limit_without_serialization_or_cloning() {
        let mut deep_value = json!(null);
        for _ in 0..32 {
            deep_value = Value::Array(vec![deep_value]);
        }
        assert!(estimate_value_footprint(&deep_value, 1_024, 8).is_none());
        let mut limits = small_limits(1);
        limits.max_entry_retained_nodes = 8;
        let cache = IdempotencyCache::with_limits(Duration::from_secs(60), limits);
        let scoped = context(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "one"}),
        );
        cache.store(&scoped, deep_value);
        assert_eq!(cache.lookup(&scoped), IdempotencyLookup::Miss);
    }

    #[test]
    fn wide_values_stop_before_building_an_unbounded_traversal_stack() {
        let wide_value = Value::Array((0..32).map(|_| json!(null)).collect());
        assert!(estimate_value_footprint(&wide_value, 1_024, 8).is_none());
    }

    #[test]
    fn zero_limits_do_not_panic_or_retain_entries() {
        let cache = IdempotencyCache::with_limits(
            Duration::from_secs(60),
            CacheLimits {
                max_entries: 0,
                max_entry_retained_bytes: 0,
                max_retained_bytes: 0,
                max_entry_retained_nodes: 0,
                max_retained_nodes: 0,
            },
        );
        let scoped = context(
            "tenant-a",
            "credential-a",
            "/v1/chat",
            json!({"prompt": "one"}),
        );
        cache.store(&scoped, json!({"response": 1}));
        assert_eq!(cache.lookup(&scoped), IdempotencyLookup::Miss);
    }
}

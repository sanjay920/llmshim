//! Request-bound idempotency for completed unary gateway responses.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::time::Instant;

const CACHE_ENTRY_VERSION: u8 = 1;

/// Credential, route, client key, and canonical request binding for one replay.
pub(crate) struct IdempotencyContext {
    storage_key: String,
    request_fingerprint: String,
}

impl IdempotencyContext {
    pub fn new(
        credential_scope: &str,
        route_scope: &str,
        client_key: &str,
        canonical_request: &Value,
    ) -> Self {
        Self {
            storage_key: digest_parts(
                b"llmshim-gateway-idempotency-owner-v1",
                &[
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

/// A process-local TTL cache of completed, request-bound responses.
pub struct IdempotencyCache {
    entries: Mutex<HashMap<String, (CachedResponse, Instant)>>,
    ttl: Duration,
    max_entries: usize,
}

impl IdempotencyCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            max_entries: 100_000,
        }
    }

    pub(crate) fn lookup(&self, context: &IdempotencyContext) -> IdempotencyLookup {
        let mut entries = self.entries.lock().unwrap();
        let now = Instant::now();
        match entries.get(context.storage_key()) {
            Some((_, expires_at)) if *expires_at <= now => {
                entries.remove(context.storage_key());
                IdempotencyLookup::Miss
            }
            Some((cached_response, _)) => cached_response.lookup(context),
            None => IdempotencyLookup::Miss,
        }
    }

    pub(crate) fn store(&self, context: &IdempotencyContext, response: Value) {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.max_entries {
            entries.retain(|_, (_, expires_at)| *expires_at > now);
        }
        entries.insert(
            context.storage_key().to_string(),
            (CachedResponse::new(context, response), now + self.ttl),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context(owner: &str, route: &str, request: Value) -> IdempotencyContext {
        IdempotencyContext::new(owner, route, "client-key", &request)
    }

    #[tokio::test(start_paused = true)]
    async fn matching_request_replays_then_expires() {
        let cache = IdempotencyCache::new(Duration::from_secs(60));
        let request = context("credential-a", "/v1/chat", json!({"prompt": "one"}));
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
        let original = context("credential-a", "/v1/chat", json!({"prompt": "one"}));
        cache.store(&original, json!({"private": "response"}));

        let other_credential = context("credential-b", "/v1/chat", json!({"prompt": "one"}));
        let other_route = context(
            "credential-a",
            "/v1/chat/completions",
            json!({"prompt": "one"}),
        );
        let changed_request = context("credential-a", "/v1/chat", json!({"prompt": "two"}));

        assert_eq!(cache.lookup(&other_credential), IdempotencyLookup::Miss);
        assert_eq!(cache.lookup(&other_route), IdempotencyLookup::Miss);
        assert_eq!(cache.lookup(&changed_request), IdempotencyLookup::Conflict);
    }
}

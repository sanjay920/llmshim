//! Captured payloads on the data plane (design note 0052, slice 5b).
//!
//! A client that opted in with `capture = "payload"` has the request and the answer of each attempt sealed into the
//! `[media_store]`: AES-256-GCM under a fresh per-object key, wrapped under the deployment's `capture.payload_key`,
//! under a name that says nothing. The usage record carries only an opaque reference. The control plane stores the
//! reference and can open nothing with it: opening goes through a gateway's admin listener, which holds the key.
//!
//! Capture is best-effort and never on a request's critical path: sealing is a few microseconds, the write is queued
//! (bounded by bytes as well as by count) and written by several writers at once, and a full queue or a failing store
//! drops the capture and counts it.

use crate::gateway::usage::{PayloadWriter, StoredPayload};
use crate::serve::blobstore::{self, BlobError, Handle, WrapKey};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Writes waiting for the store, by count and by bytes: a full queue drops the capture. The bytes bound is the one
/// that matters, since a document can be megabytes.
const QUEUE: usize = 1024;
const QUEUED_BYTES: usize = 64 << 20;
/// Writes in flight at once: one store round trip is tens of milliseconds, and captures must not wait in line
/// behind it.
const WRITERS: usize = 8;

/// Captured payloads by outcome: `queued`, `stored`, `failed` (the store refused or did not answer),
/// `dropped_full` (the queue was full) and `too_large` (the store would refuse the object).
pub const PAYLOAD_CAPTURE: &str = "llmshim_payload_capture_total";

fn count(outcome: &'static str) {
    crate::gateway::metrics::incr(PAYLOAD_CAPTURE, &[("outcome", outcome)]);
}

pub struct PayloadStore {
    handle: Arc<Handle>,
    wrap: WrapKey,
    ttl: Duration,
    queue: tokio::sync::mpsc::Sender<(String, Vec<u8>)>,
    /// Ciphertext waiting to be written.
    queued_bytes: Arc<AtomicUsize>,
    queued_limit: usize,
}

impl PayloadStore {
    /// `secret` is the configured `capture.payload_key`'s bytes. The writers run on the current runtime, or on a
    /// thread of their own when there is none (a test, a check).
    pub fn new(handle: Arc<Handle>, secret: &[u8], ttl: Duration) -> Arc<Self> {
        Self::with_limit(handle, secret, ttl, QUEUED_BYTES)
    }

    fn with_limit(
        handle: Arc<Handle>,
        secret: &[u8],
        ttl: Duration,
        queued_limit: usize,
    ) -> Arc<Self> {
        let (queue, mut receiver) = tokio::sync::mpsc::channel::<(String, Vec<u8>)>(QUEUE);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let store = handle.store.clone();
        let pending = queued_bytes.clone();
        let writers = Arc::new(tokio::sync::Semaphore::new(WRITERS));
        let write = async move {
            while let Some((name, ciphertext)) = receiver.recv().await {
                let Ok(permit) = writers.clone().acquire_owned().await else {
                    break;
                };
                let (store, pending) = (store.clone(), pending.clone());
                tokio::spawn(async move {
                    let size = ciphertext.len();
                    match store.put(&name, ciphertext).await {
                        Ok(()) => count("stored"),
                        Err(_) => count("failed"),
                    }
                    pending.fetch_sub(size, Ordering::AcqRel);
                    drop(permit);
                });
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(write);
            }
            Err(_) => {
                std::thread::spawn(move || {
                    if let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .build()
                    {
                        runtime.block_on(write);
                    }
                });
            }
        }
        Arc::new(Self {
            handle,
            wrap: WrapKey::derive(secret),
            ttl,
            queue,
            queued_bytes,
            queued_limit,
        })
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn object_store(&self) -> Arc<dyn blobstore::BlobStore> {
        self.handle.store.clone()
    }

    /// The document a reference names, opened. The reference is never logged or echoed.
    pub async fn open(&self, reference: &str) -> Result<Value, BlobError> {
        let max = self.handle.store.max_object_bytes();
        let bytes =
            blobstore::open_payload(self.handle.store.as_ref(), reference, &self.wrap, max).await?;
        serde_json::from_slice(&bytes).map_err(|_| BlobError::Corrupt)
    }
}

impl PayloadWriter for PayloadStore {
    fn store(&self, document: Vec<u8>) -> Option<StoredPayload> {
        if document.len() > self.handle.store.max_object_bytes() {
            count("too_large");
            return None;
        }
        // The expiry is fixed here and sealed into the reference: past it the payload cannot be opened, whether or
        // not the sweeper has deleted the object yet.
        let expires = chrono::Utc::now() + chrono::Duration::from_std(self.ttl).unwrap_or_default();
        let sealed = blobstore::seal_payload(
            blobstore::PAYLOAD_PREFIX,
            &document,
            &self.wrap,
            expires.timestamp_millis(),
        );
        let size = sealed.ciphertext.len();
        if self.queued_bytes.fetch_add(size, Ordering::AcqRel) + size > self.queued_limit
            || self
                .queue
                .try_send((sealed.name, sealed.ciphertext))
                .is_err()
        {
            self.queued_bytes.fetch_sub(size, Ordering::AcqRel);
            count("dropped_full");
            return None;
        }
        count("queued");
        Some(StoredPayload {
            reference: sealed.reference,
            bytes: document.len() as u64,
            expires_at: expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        })
    }
}

/// What the admin endpoint answers for a reference that does not open: the class of the problem and nothing the
/// store or the reference said.
pub fn refusal(error: BlobError) -> (axum::http::StatusCode, &'static str) {
    use axum::http::StatusCode;
    match error {
        BlobError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
        BlobError::Invalid | BlobError::Corrupt => {
            (StatusCode::UNPROCESSABLE_ENTITY, "bad_reference")
        }
        BlobError::TooLarge => (StatusCode::UNPROCESSABLE_ENTITY, "too_large"),
        BlobError::Denied | BlobError::Unavailable => {
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::blobstore::tests::Memory;

    fn handle(store: Arc<dyn blobstore::BlobStore>) -> Arc<Handle> {
        Arc::new(Handle {
            store,
            ttl: Duration::from_secs(3600),
        })
    }

    const SECRET: &[u8] = &[7u8; 32];

    #[tokio::test]
    async fn a_stored_payload_opens_with_its_reference_and_is_gone_past_its_expiry() {
        let memory = Arc::new(Memory::default());
        let store = PayloadStore::new(handle(memory.clone()), SECRET, Duration::from_secs(3600));
        let stored = store
            .store(br#"{"v":1,"request":{"text":"hello"}}"#.to_vec())
            .expect("queued");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let opened = loop {
            match store.open(&stored.reference).await {
                Ok(document) => break document,
                Err(BlobError::NotFound) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("{error:?}"),
            }
        };
        assert_eq!(opened["request"]["text"], "hello");
        // A payload whose retention is over cannot be opened, though the object is still in the store.
        let short = PayloadStore::new(handle(memory.clone()), SECRET, Duration::from_millis(1));
        let gone = short.store(br#"{"v":1}"#.to_vec()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            short.open(&gone.reference).await.unwrap_err(),
            BlobError::NotFound
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while memory.objects.lock().unwrap().len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the second object was never written"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A store whose writes wait until it is told to go.
    struct Held {
        inner: Memory,
        go: tokio::sync::Notify,
        in_flight: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl blobstore::BlobStore for Held {
        async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<(), BlobError> {
            self.in_flight.fetch_add(1, Ordering::SeqCst);
            self.go.notified().await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.inner.put(name, bytes).await
        }
        async fn get(&self, name: &str, max: usize) -> Result<Vec<u8>, BlobError> {
            self.inner.get(name, max).await
        }
        async fn delete(&self, name: &str) -> Result<(), BlobError> {
            self.inner.delete(name).await
        }
        async fn list(&self) -> Result<Vec<blobstore::Listed>, BlobError> {
            self.inner.list().await
        }
        fn max_object_bytes(&self) -> usize {
            1 << 20
        }
    }

    #[tokio::test]
    async fn waiting_captures_are_bounded_by_bytes_and_writes_overlap() {
        let held = Arc::new(Held {
            inner: Memory::default(),
            go: tokio::sync::Notify::new(),
            in_flight: AtomicUsize::new(0),
        });
        // Room for about three 10 KB documents.
        let store = PayloadStore::with_limit(
            handle(held.clone()),
            SECRET,
            Duration::from_secs(3600),
            35_000,
        );
        let document = vec![b'x'; 10_000];
        let kept = (0..10)
            .filter(|_| store.store(document.clone()).is_some())
            .count();
        assert_eq!(kept, 3, "the bytes waiting are bounded, not just the count");
        // The three writes are in flight together (the store is slow), not one after another.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while held.in_flight.load(Ordering::SeqCst) < 3 {
            assert!(
                std::time::Instant::now() < deadline,
                "writes did not overlap"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Once they finish the room is back.
        for _ in 0..3 {
            held.go.notify_one();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while store.queued_bytes.load(Ordering::SeqCst) != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the budget was not returned"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(store.store(document).is_some());
    }

    #[test]
    fn a_secret_must_be_random_bytes_in_base64_not_a_phrase() {
        assert!(
            blobstore::secret_bytes(&"A".repeat(43)).is_ok(),
            "32 bytes, unpadded"
        );
        assert!(
            blobstore::secret_bytes("MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=").is_ok(),
            "32 bytes, padded"
        );
        for bad in [
            "a-human-memorable-passphrase-of-some-length",
            "",
            "short",
            &"A".repeat(30),
            "not base64 at all!!",
        ] {
            assert!(blobstore::secret_bytes(bad).is_err(), "{bad}");
        }
    }
}

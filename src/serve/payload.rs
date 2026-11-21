//! Captured payloads on the data plane (design note 0052, slice 5b).
//!
//! A client that opted in with `capture = "payload"` has the request and the answer of each attempt sealed into the
//! `[media_store]`: AES-256-GCM under a fresh per-object key, wrapped under the deployment's `capture.payload_key`,
//! under a name that says nothing. The usage record carries only an opaque reference. The control plane stores the
//! reference and can open nothing with it: opening goes through a gateway's admin listener, which holds the key.
//!
//! Capture is best-effort and never on a request's critical path: sealing is a few microseconds, the write is queued
//! to a bounded channel, and a full queue or a failing store drops the capture and counts it.

use crate::gateway::usage::{PayloadWriter, StoredPayload};
use crate::serve::blobstore::{self, BlobError, Handle, WrapKey};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Writes waiting for the store; a full queue drops the capture.
const QUEUE: usize = 256;

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
}

impl PayloadStore {
    /// `secret` is the configured `capture.payload_key`. The writer runs on the current runtime, or on a thread of its
    /// own when there is none (a test, a check).
    pub fn new(handle: Arc<Handle>, secret: &str, ttl: Duration) -> Arc<Self> {
        let (queue, mut receiver) = tokio::sync::mpsc::channel::<(String, Vec<u8>)>(QUEUE);
        let store = handle.store.clone();
        let write = async move {
            while let Some((name, ciphertext)) = receiver.recv().await {
                match store.put(&name, ciphertext).await {
                    Ok(()) => count("stored"),
                    Err(_) => count("failed"),
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(write);
            }
            Err(_) => {
                std::thread::spawn(move || {
                    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
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
        })
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub fn store(&self) -> Arc<dyn blobstore::BlobStore> {
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
        let sealed = blobstore::seal_payload(blobstore::PAYLOAD_PREFIX, &document, &self.wrap);
        if self
            .queue
            .try_send((sealed.name, sealed.ciphertext))
            .is_err()
        {
            count("dropped_full");
            return None;
        }
        count("queued");
        let expires = chrono::Utc::now() + chrono::Duration::from_std(self.ttl).unwrap_or_default();
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

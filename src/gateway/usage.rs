//! Per-attempt usage records, for showback and chargeback.
//!
//! The gateway already meters every provider attempt to enforce spend caps. This
//! module lets an embedder watch that metering: when an attempt ends, one
//! [`UsageRecord`] goes to the installed [`UsageSink`] with the tenant, the
//! provider and model, how the attempt ended, and the tokens and cost the
//! provider reported.
//!
//! Records are per **attempt**, not per request. A retry, a repair and a fallback
//! are each a separate charge, and a request that fails over appears once for
//! every provider it touched. Summing `cost_usd` per tenant gives what the
//! tenant cost; a `null` cost means unknown, never free, so a sum that must be
//! exact has to treat nulls as a gap rather than as zero.
//!
//! The recorder holds no more than the latest usage snapshot of each attempt
//! still in flight, and drops it when the attempt finishes or is abandoned. An
//! attempt is recorded exactly once, and only if it was admitted: a request the
//! gateway refused before sending costs nothing and produces no record.

use crate::policy::{AttemptIdentity, AttemptKind, AttemptOutcome};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// What one provider attempt cost.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UsageRecord {
    /// Always `"usage"`, so records can share a stream with other log lines.
    pub event: &'static str,
    /// UTC, when the attempt ended.
    pub ts: String,
    /// The attempt's id, unique per provider call.
    pub attempt_id: String,
    /// The caller's tenant. `None` when this replica's configuration does not
    /// name the tenant the job belongs to.
    pub tenant: Option<String>,
    pub provider: String,
    pub model: String,
    /// `"completion"` or `"stream"`.
    pub kind: &'static str,
    /// `completed`, `http_failure`, `transport_failure`, `invalid_response`,
    /// `stream_failure` or `abandoned`.
    pub outcome: &'static str,
    /// The upstream HTTP status, for an `http_failure`.
    pub http_status: Option<u16>,
    pub duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// USD for this attempt. A provider floor is a lower bound; `None` means
    /// unknown and never means free.
    pub cost_usd: Option<f64>,
    /// `"provider"` (an exact terminal bill), `"provider_floor"` (the highest
    /// partial bill seen, a lower bound) or `"catalog"` (an estimate).
    pub cost_source: Option<String>,
    /// Whether the provider reported any usage for this attempt.
    pub usage_reported: bool,
}

/// Receives a record when an attempt ends. Called inline on the response path:
/// return promptly, and never fail the request.
pub trait UsageSink: Send + Sync {
    fn record(&self, record: &UsageRecord);
}

/// A sink and the tenants it can name.
///
/// The policy scope that travels with a job (through Redis, in a fleet) holds
/// only a hash of the tenant, on purpose: tenant names are not put in shared
/// storage. A replica names the tenant by hashing each of its own configured
/// tenants and looking the job's hash up. Replicas of one fleet share a
/// configuration, so they agree; a tenant a replica does not know is `None`.
pub struct UsageSetup {
    pub sink: Arc<dyn UsageSink>,
    pub tenants: Vec<String>,
}

impl UsageSetup {
    /// The hash that identifies a tenant in a policy scope.
    pub(crate) fn tenant_key(tenant: &str) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(tenant.as_bytes()))
    }
}

struct Open {
    tenant: Option<String>,
    provider: String,
    model: String,
    kind: &'static str,
    started: Instant,
    usage: Option<Value>,
}

/// Tracks attempts in flight and emits one record when each ends.
pub(crate) struct UsageRecorder {
    sink: Arc<dyn UsageSink>,
    /// Tenant name by the hash a policy scope carries.
    tenants: HashMap<String, String>,
    open: Mutex<HashMap<uuid::Uuid, Open>>,
}

impl UsageRecorder {
    pub(crate) fn new(setup: UsageSetup) -> Self {
        Self {
            sink: setup.sink,
            tenants: setup
                .tenants
                .into_iter()
                .map(|tenant| (UsageSetup::tenant_key(&tenant), tenant))
                .collect(),
            open: Mutex::new(HashMap::new()),
        }
    }

    /// An attempt was admitted and is about to be sent. `tenant_key` is the hash
    /// from the policy scope.
    pub(crate) fn begin(&self, identity: &AttemptIdentity, tenant_key: &str) {
        let open = Open {
            tenant: self.tenants.get(tenant_key).cloned(),
            provider: identity.provider_name().to_owned(),
            model: identity.native_model().to_owned(),
            kind: kind_name(identity.kind()),
            started: Instant::now(),
            usage: None,
        };
        if let Ok(mut attempts) = self.open.lock() {
            attempts.insert(identity.id(), open);
        }
    }

    /// The attempt's cumulative usage snapshot; the latest one wins.
    pub(crate) fn observe(&self, id: uuid::Uuid, usage: &Value) {
        if let Ok(mut attempts) = self.open.lock() {
            if let Some(open) = attempts.get_mut(&id) {
                open.usage = Some(usage.clone());
            }
        }
    }

    /// The attempt ended. Exactly one record per admitted attempt: a second call
    /// for the same id (a finish followed by a drop notification) finds nothing.
    pub(crate) fn finish(&self, id: uuid::Uuid, outcome: AttemptOutcome) {
        let Some(open) = self.open.lock().ok().and_then(|mut a| a.remove(&id)) else {
            return;
        };
        let record = build(id, open, outcome);
        self.sink.record(&record);
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.open.lock().unwrap().len()
    }
}

fn kind_name(kind: AttemptKind) -> &'static str {
    match kind {
        AttemptKind::Completion => "completion",
        AttemptKind::Stream => "stream",
    }
}

fn outcome_parts(outcome: AttemptOutcome) -> (&'static str, Option<u16>) {
    match outcome {
        AttemptOutcome::Completed { .. } => ("completed", None),
        AttemptOutcome::HttpFailure { status, .. } => ("http_failure", Some(status)),
        AttemptOutcome::TransportFailure { .. } => ("transport_failure", None),
        AttemptOutcome::InvalidResponse { .. } => ("invalid_response", None),
        AttemptOutcome::StreamFailure { .. } => ("stream_failure", None),
        AttemptOutcome::Abandoned { .. } => ("abandoned", None),
    }
}

fn build(id: uuid::Uuid, open: Open, outcome: AttemptOutcome) -> UsageRecord {
    let (outcome, http_status) = outcome_parts(outcome);
    let usage = open.usage.as_ref();
    let count = |key: &str| -> u64 {
        usage
            .and_then(|usage| usage.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    UsageRecord {
        event: "usage",
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        attempt_id: id.to_string(),
        tenant: open.tenant,
        provider: open.provider,
        model: open.model,
        kind: open.kind,
        outcome,
        http_status,
        duration_ms: open.started.elapsed().as_millis() as u64,
        input_tokens: count("prompt_tokens"),
        output_tokens: count("completion_tokens"),
        reasoning_tokens: count("reasoning_tokens"),
        cache_read_tokens: count("cache_read_tokens"),
        cache_write_tokens: count("cache_write_tokens"),
        cost_usd: usage
            .and_then(|usage| usage.get("cost_usd"))
            .and_then(Value::as_f64),
        cost_source: usage
            .and_then(|usage| usage.get("cost_source"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        usage_reported: usage.is_some(),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Collects records in memory.
    #[derive(Default)]
    pub(crate) struct Collected(pub(crate) Mutex<Vec<UsageRecord>>);

    impl UsageSink for Collected {
        fn record(&self, record: &UsageRecord) {
            self.0.lock().unwrap().push(record.clone());
        }
    }
}

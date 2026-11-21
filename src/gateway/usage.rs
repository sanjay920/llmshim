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
//! still in flight (plus three cost figures), and drops them when the attempt
//! finishes or is abandoned. An attempt is recorded exactly once, and only if it
//! was admitted: a request the gateway refused before sending costs nothing and
//! produces no record. An attempt that was admitted and then dropped before its
//! request left (a deadline, a cancelled client) is recorded as `abandoned` with
//! no usage.
//!
//! The cost of a record is settled by the same rule as the spend ledger, so the
//! two cannot disagree: a provider's exact terminal bill wins; otherwise the
//! terminal catalog estimate, but never below the highest charge the provider
//! reported along the way (`provider_floor`). A later, lower catalog estimate
//! cannot erase a charge the provider already reported.

// The module is the seam `serve` plugs its usage log into; without that feature
// nothing installs a sink, and the setup types go unused.
#![cfg_attr(not(feature = "serve"), allow(dead_code))]

use crate::gateway::budget::{may_finalize, SpendAuthority, SpendObservation};
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
    /// The name (`keys[].name`) of the caller key the request authenticated
    /// with. `None` when this replica's configuration does not name the
    /// credential the job carries, or the caller had none.
    pub key: Option<String>,
    /// The key's operator-chosen labels, names sorted (note 0029); `{}` for a named
    /// key with none, `null` exactly when `key` is `null`. Labels only: nothing
    /// admits, limits or routes on them.
    pub tags: Option<std::collections::BTreeMap<String, String>>,
    /// The request's id: the caller's `x-request-id` when it sent a well-formed
    /// one, otherwise the id the gateway minted. It is the `request_id` of the
    /// request's access-log line, so every attempt of a request (a retry, a
    /// fallback) joins to it and to the caller's own logs. `None` for a job that
    /// carries none.
    pub request_id: Option<String>,
    /// The deployment the caller addressed (`deployment/<name>`), `null` otherwise; `provider` and
    /// `model` are the target that served the attempt (note 0031).
    pub deployment: Option<String>,
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
    /// The provider's prompt count. Providers disagree on whether it already
    /// includes cache reads (Anthropic's does not, OpenAI's does); use
    /// `uncached_input_tokens` with the two cache fields for chargeback.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// `None` when the provider reported no reasoning count, which is not the
    /// same as none being used.
    pub reasoning_tokens: Option<u64>,
    /// Prompt tokens billed at the full input rate; `None` when not reported.
    pub uncached_input_tokens: Option<u64>,
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
    /// `"fleet"` for an attempt admitted the ordinary way, `"outage"` for one
    /// admitted on a replica-owned lease while Redis was unreachable (0027).
    pub admission: &'static str,
    /// The prompt cut into keyed-hash segments, for the cache debugger (note 0052): present only for a
    /// client whose configuration turns structure capture on, and for a request body the walker understood.
    /// Hashes, kinds and sizes: none of the prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structure: Option<Vec<crate::gateway::structure::Segment>>,
    /// A sealed copy of the request and the answer, for a client that opts in to payload capture (note 0052,
    /// slice 5b): an opaque reference only. The text is in the object store, sealed; the reference opens nothing
    /// without the deployment's capture key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<StoredPayload>,
}

/// Where a captured payload went, as the record carries it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StoredPayload {
    /// Opaque: the object's name, its length and its key wrapped under the deployment's capture key.
    pub reference: String,
    /// The size of the stored document, before sealing.
    pub bytes: u64,
    /// When the object is due to be deleted (RFC 3339).
    pub expires_at: String,
}

/// Seals a payload document and queues its write (note 0052, slice 5b). Called once, when an attempt ends, with
/// the whole document; `None` when it was dropped (a full queue, a document the store would refuse), counted.
pub trait PayloadWriter: Send + Sync {
    fn store(&self, document: Vec<u8>) -> Option<StoredPayload>;
}

/// A bounded copy of a text: the first `max` bytes of it, and whether there was more.
struct Capped {
    text: Vec<u8>,
    seen: u64,
    max: usize,
}

impl Capped {
    fn new(max: usize) -> Self {
        Self {
            text: Vec::new(),
            seen: 0,
            max,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.seen += bytes.len() as u64;
        let room = self.max.saturating_sub(self.text.len());
        self.text.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    fn json(&self) -> Value {
        serde_json::json!({
            "text": String::from_utf8_lossy(&self.text),
            "bytes": self.seen,
            "truncated": self.seen > self.text.len() as u64,
        })
    }
}

impl std::io::Write for Capped {
    /// Past the cap the write is refused, which stops the serializer feeding it: a huge body costs the time of its
    /// first `max` bytes, not of all of them. The size reported is then the cap and more, never the true size.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.text.len() >= self.max {
            self.seen += 1;
            return Err(std::io::Error::other("capped"));
        }
        self.push(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The request and the answer of one attempt, collected while it runs.
struct PendingPayload {
    wire: crate::reasoning::WireFormat,
    request: Capped,
    response: Capped,
    writer: Arc<dyn PayloadWriter>,
}

/// Receives a record when an attempt ends. Called inline on the response path:
/// return promptly, and never fail the request.
pub trait UsageSink: Send + Sync {
    fn record(&self, record: &UsageRecord);
}

/// A sink, and the tenants and keys it can name.
///
/// The policy scope that travels with a job (through Redis, in a fleet) holds
/// only a hash of the tenant, on purpose: tenant names are not put in shared
/// storage. A replica names the tenant by hashing each of its own configured
/// tenants and looking the job's hash up. Replicas of one fleet share a
/// configuration, so they agree; a tenant a replica does not know is `None`.
#[derive(Clone)]
pub struct UsageSetup {
    pub sink: Arc<dyn UsageSink>,
    pub tenants: TenantNames,
    pub keys: KeyNames,
    /// Prompt-structure capture (note 0052); `None` records no structure for anyone.
    pub capture: Option<StructureCapture>,
    /// Starts a trace span for each admitted attempt, when something exports
    /// traces. Driven from the same two points as the records.
    pub spans: Option<Arc<dyn AttemptSpans>>,
}

/// Which clients have structure capture on, and the secret that keys the fingerprints (note 0052).
///
/// Clones share one table, so a reload that changes a client's `capture` takes effect for every holder at once;
/// unlike the names tables it **replaces** the set, because turning capture off must stop it. The secret is fixed
/// for the process: rotating it is a restart, after which earlier and later fingerprints are not comparable.
#[derive(Clone)]
pub struct StructureCapture {
    key: Arc<Vec<u8>>,
    clients: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    payload: Option<PayloadCapture>,
}

/// Payload capture (note 0052, slice 5b): which clients opted in, where the document goes, and the most of the
/// request and of the answer that is kept.
#[derive(Clone)]
pub struct PayloadCapture {
    clients: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    writer: Arc<dyn PayloadWriter>,
    max_bytes: usize,
}

impl StructureCapture {
    pub fn new(key: Vec<u8>, clients: impl IntoIterator<Item = String>) -> Self {
        let capture = Self {
            key: Arc::new(key),
            clients: Default::default(),
            payload: None,
        };
        capture.set_clients(clients);
        capture
    }

    /// Also capture payloads, for `clients`, through `writer`, keeping at most `max_bytes` of the request and of the
    /// answer each.
    pub fn with_payload(
        mut self,
        writer: Arc<dyn PayloadWriter>,
        max_bytes: usize,
        clients: impl IntoIterator<Item = String>,
    ) -> Self {
        self.payload = Some(PayloadCapture {
            clients: Default::default(),
            writer,
            max_bytes,
        });
        self.set_payload_clients(clients);
        self
    }

    /// The credentials that capture payloads; everyone else does not. A reload replaces the set.
    pub fn set_payload_clients(&self, clients: impl IntoIterator<Item = String>) {
        if let Some(payload) = &self.payload {
            let mut set = payload.clients.write().unwrap_or_else(|p| p.into_inner());
            *set = clients
                .into_iter()
                .map(|c| credential_prefix(&c).to_owned())
                .collect();
        }
    }

    /// The pending payload of an attempt of `credential`, when it opted in.
    fn pending_payload(
        &self,
        credential: Option<&str>,
        wire: crate::reasoning::WireFormat,
        body: &Value,
    ) -> Option<PendingPayload> {
        let payload = self.payload.as_ref()?;
        let credential = credential?;
        if !payload
            .clients
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .contains(credential_prefix(credential))
        {
            return None;
        }
        let mut request = Capped::new(payload.max_bytes);
        // A body is written into the bounded copy as it is serialized, and the serializer is stopped when the copy is
        // full: a huge body costs the time of its first `max_bytes`, and its memory is the cap.
        let _ = serde_json::to_writer(&mut request, body);
        Some(PendingPayload {
            wire,
            request,
            response: Capped::new(payload.max_bytes),
            writer: payload.writer.clone(),
        })
    }

    /// The credentials (whole scope digests or the prefix a policy scope carries) that capture; everyone else
    /// does not.
    pub fn set_clients(&self, clients: impl IntoIterator<Item = String>) {
        let mut set = self.clients.write().unwrap_or_else(|p| p.into_inner());
        *set = clients
            .into_iter()
            .map(|c| credential_prefix(&c).to_owned())
            .collect();
    }

    /// The segments of `body` when the credential captures, `None` when it does not (or has none).
    pub(crate) fn segments(
        &self,
        credential: Option<&str>,
        wire: crate::reasoning::WireFormat,
        body: &Value,
    ) -> Option<Vec<crate::gateway::structure::Segment>> {
        let credential = credential?;
        if !self
            .clients
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .contains(credential_prefix(credential))
        {
            return None;
        }
        let segments = crate::gateway::structure::segments(wire, body, &self.key);
        #[cfg(feature = "serve")]
        crate::gateway::metrics::incr(
            STRUCTURE_CAPTURE,
            &[(
                "outcome",
                match &segments {
                    Ok(_) => "captured",
                    Err(crate::gateway::structure::Skipped::Unrecognized) => "unrecognized",
                    Err(crate::gateway::structure::Skipped::TooLarge) => "too_large",
                },
            )],
        );
        segments.ok()
    }
}

/// Structure captures by outcome: `captured`, `unrecognized` (a body the walker does not understand) or `too_large`
/// (past the work budget).
#[cfg(feature = "serve")]
pub const STRUCTURE_CAPTURE: &str = "llmshim_structure_capture_total";

/// What is known when an attempt is admitted.
#[cfg_attr(not(feature = "serve-otel"), allow(dead_code))]
pub struct AttemptBegun<'a> {
    pub request_id: Option<&'a str>,
    pub provider: &'a str,
    pub model: &'a str,
    pub kind: &'static str,
}

/// Starts the trace span of an attempt. One span per admitted attempt, none for
/// a refusal, as for the record.
pub trait AttemptSpans: Send + Sync {
    fn start(&self, begun: &AttemptBegun<'_>) -> Box<dyn AttemptSpan>;
}

/// The span of one attempt, ended with the record the usage log would carry.
pub trait AttemptSpan: Send {
    fn end(self: Box<Self>, record: &UsageRecord);
}

/// Tenant names by the hash a policy scope carries.
///
/// Clones share one table, so a configuration reload that adds a tenant makes
/// its name resolvable for every holder at once. The table only grows: a name
/// is a label on a record, not a permission, and an attempt that was admitted
/// under a tenant a later reload removed (or one queued on another replica that
/// has not reloaded yet) is still better named than left blank.
#[derive(Clone, Default)]
pub struct TenantNames(Arc<std::sync::RwLock<HashMap<String, String>>>);

impl TenantNames {
    pub fn new(tenants: impl IntoIterator<Item = String>) -> Self {
        let names = Self::default();
        names.add(tenants);
        names
    }

    /// Make more tenants resolvable. Names already known stay known.
    pub fn add(&self, tenants: impl IntoIterator<Item = String>) {
        let mut table = self.0.write().unwrap_or_else(|p| p.into_inner());
        for tenant in tenants {
            table.insert(UsageSetup::tenant_key(&tenant), tenant);
        }
    }

    pub(crate) fn name(&self, key: &str) -> Option<String> {
        self.0
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(key)
            .cloned()
    }
}

/// How many hex digits of a credential's scope digest a policy scope carries.
/// 16 (64 bits) tells keys apart, which is all a log line needs, and is not the
/// digest an operator may hold in a `sha256` field, so it cannot stand in for the
/// token's verifier.
pub(crate) const CREDENTIAL_PREFIX_LEN: usize = 16;

/// The prefix of a credential scope that identifies a key in a policy scope and
/// in [`KeyNames`].
pub(crate) fn credential_prefix(credential_scope: &str) -> &str {
    credential_scope
        .get(..CREDENTIAL_PREFIX_LEN)
        .unwrap_or(credential_scope)
}

/// Caller-key names by credential.
///
/// A job's policy scope carries the first 16 hex digits of the digest that scopes
/// the credential (the SHA-256 of its token, or of `mtls:<name>` for a
/// certificate workload), never the key's name; a replica names the key from its
/// own configuration, as it names the tenant. Like [`TenantNames`], clones share
/// one table, it only grows, and a name is a label, not a permission. A key that
/// is renamed keeps resolving, to its newest name.
///
/// Callers pass a whole scope digest or the prefix a policy scope carries; the
/// table is keyed by the prefix either way.
#[derive(Clone, Default)]
pub struct KeyNames {
    names: Arc<std::sync::RwLock<HashMap<String, String>>>,
    /// Operator-chosen labels per credential (design note 0029): copied onto usage
    /// records, never used to decide anything. A credential with a name and no entry
    /// here has no tags.
    tags: Arc<std::sync::RwLock<HashMap<String, std::collections::BTreeMap<String, String>>>>,
}

impl KeyNames {
    /// `(credential scope, key name)` pairs.
    pub fn new(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        let names = Self::default();
        names.add(pairs);
        names
    }

    /// Make more keys resolvable. A credential already known keeps resolving; a
    /// renamed one resolves to its newest name.
    pub fn add(&self, pairs: impl IntoIterator<Item = (String, String)>) {
        let mut table = self.names.write().unwrap_or_else(|p| p.into_inner());
        table.extend(
            pairs
                .into_iter()
                .map(|(credential, name)| (credential_prefix(&credential).to_owned(), name)),
        );
    }

    /// Set the tags of credentials (replacing what each had: a key whose tags were all
    /// removed resolves to none).
    pub fn add_tags(
        &self,
        pairs: impl IntoIterator<Item = (String, std::collections::BTreeMap<String, String>)>,
    ) {
        let mut table = self.tags.write().unwrap_or_else(|p| p.into_inner());
        table.extend(
            pairs
                .into_iter()
                .map(|(credential, tags)| (credential_prefix(&credential).to_owned(), tags)),
        );
    }

    /// The tags of a credential this replica can name: `{}` when it has none, `None`
    /// exactly when [`Self::name`] is.
    pub(crate) fn tags(
        &self,
        credential: &str,
    ) -> Option<std::collections::BTreeMap<String, String>> {
        self.name(credential)?;
        Some(
            self.tags
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .get(credential_prefix(credential))
                .cloned()
                .unwrap_or_default(),
        )
    }

    pub(crate) fn name(&self, credential: &str) -> Option<String> {
        self.names
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(credential_prefix(credential))
            .cloned()
    }
}

impl UsageSetup {
    /// The hash that identifies a tenant in a policy scope.
    pub(crate) fn tenant_key(tenant: &str) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(tenant.as_bytes()))
    }
}

struct Open {
    span: Option<Box<dyn AttemptSpan>>,
    tenant: Option<String>,
    key: Option<String>,
    tags: Option<std::collections::BTreeMap<String, String>>,
    request_id: Option<String>,
    deployment: Option<String>,
    provider: String,
    model: String,
    kind: &'static str,
    started: Instant,
    /// The latest usage snapshot: counters are cumulative, so the last wins.
    usage: Option<Value>,
    /// The three figures the ledger settles from, in USD. The highest charge
    /// the provider reported, its exact terminal bill, and the largest terminal
    /// catalog estimate.
    max_provider_usd: Option<f64>,
    terminal_provider_usd: Option<f64>,
    terminal_catalog_usd: Option<f64>,
    structure: Option<Vec<crate::gateway::structure::Segment>>,
    payload: Option<PendingPayload>,
}

/// Tracks attempts in flight and emits one record when each ends.
pub(crate) struct UsageRecorder {
    spans: Option<Arc<dyn AttemptSpans>>,
    sink: Arc<dyn UsageSink>,
    tenants: TenantNames,
    keys: KeyNames,
    open: Mutex<HashMap<uuid::Uuid, Open>>,
    admission: &'static str,
    capture: Option<StructureCapture>,
}

impl UsageRecorder {
    pub(crate) fn new(setup: UsageSetup) -> Self {
        Self {
            capture: setup.capture,
            spans: setup.spans,
            sink: setup.sink,
            tenants: setup.tenants,
            keys: setup.keys,
            open: Mutex::new(HashMap::new()),
            admission: "fleet",
        }
    }

    /// The admission every record of this recorder carries (`"outage"` for
    /// the local outage path, note 0027).
    #[cfg(feature = "redis-coordination")]
    pub(crate) fn with_admission(mut self, admission: &'static str) -> Self {
        self.admission = admission;
        self
    }

    /// An attempt was admitted and is about to be sent. `tenant_key`,
    /// `credential` and `request_id` come from the policy scope; `native_body` is the request as the
    /// provider will receive it, read only to fingerprint its structure for a client that opted in.
    pub(crate) fn begin(
        &self,
        identity: &AttemptIdentity,
        native_body: &Value,
        tenant_key: &str,
        credential: Option<&str>,
        request_id: Option<&str>,
        deployment: Option<&str>,
    ) {
        let structure = self
            .capture
            .as_ref()
            .and_then(|capture| capture.segments(credential, identity.wire(), native_body));
        let payload = self
            .capture
            .as_ref()
            .and_then(|capture| capture.pending_payload(credential, identity.wire(), native_body));
        let span = self.spans.as_ref().map(|spans| {
            spans.start(&AttemptBegun {
                request_id,
                provider: identity.instance(),
                model: identity.native_model(),
                kind: kind_name(identity.kind()),
            })
        });
        let open = Open {
            span,
            tenant: self.tenants.name(tenant_key),
            key: credential.and_then(|credential| self.keys.name(credential)),
            // Read with the name, at admission, so the two always agree on one record.
            tags: credential.and_then(|credential| self.keys.tags(credential)),
            request_id: request_id.map(str::to_owned),
            deployment: deployment.map(str::to_owned),
            provider: identity.instance().to_owned(),
            model: identity.native_model().to_owned(),
            kind: kind_name(identity.kind()),
            started: Instant::now(),
            usage: None,
            max_provider_usd: None,
            terminal_provider_usd: None,
            terminal_catalog_usd: None,
            structure,
            payload,
        };
        if let Ok(mut attempts) = self.open.lock() {
            attempts.insert(identity.id(), open);
        }
    }

    /// The attempt's cumulative usage snapshot. Counters take the latest; the
    /// cost figures are kept as the ledger keeps them (see the module docs).
    /// `observation` is the same value the ledger is given for this snapshot.
    pub(crate) fn observe(&self, id: uuid::Uuid, usage: &Value, observation: &SpendObservation) {
        let usd = crate::cost::stamped(usage)
            .or_else(|| (observation.amount_nanos == Some(0)).then_some(0.0))
            .filter(|usd| usd.is_finite() && *usd >= 0.0);
        if let Ok(mut attempts) = self.open.lock() {
            if let Some(open) = attempts.get_mut(&id) {
                open.usage = Some(usage.clone());
                let Some(usd) = usd else { return };
                if observation.authority == SpendAuthority::Provider {
                    open.max_provider_usd = Some(open.max_provider_usd.map_or(usd, |m| m.max(usd)));
                }
                if observation.terminal_authoritative {
                    match observation.authority {
                        SpendAuthority::Provider => open.terminal_provider_usd = Some(usd),
                        SpendAuthority::Catalog => {
                            open.terminal_catalog_usd =
                                Some(open.terminal_catalog_usd.map_or(usd, |c| c.max(usd)));
                        }
                    }
                }
            }
        }
    }

    /// One frame of the attempt's answer as the provider sent it, for a client that captures payloads: a stream's
    /// events are kept in arrival order up to the cap, a non-streaming body as it is.
    pub(crate) fn observe_native(&self, id: uuid::Uuid, frame: crate::policy::NativeFrame<'_>) {
        // Every frame of every stream comes through here: with payload capture off there is nothing to keep, and the
        // lock is not taken.
        if !self
            .capture
            .as_ref()
            .is_some_and(|capture| capture.payload.is_some())
        {
            return;
        }
        let Ok(mut attempts) = self.open.lock() else {
            return;
        };
        let Some(pending) = attempts.get_mut(&id).and_then(|open| open.payload.as_mut()) else {
            return;
        };
        match frame {
            crate::policy::NativeFrame::StreamData { data } => {
                pending.response.push(data.as_bytes());
                pending.response.push(b"\n");
            }
            crate::policy::NativeFrame::ResponseBody { body } => {
                let _ = serde_json::to_writer(&mut pending.response, body);
            }
        }
    }

    /// The attempt ended. Exactly one record per admitted attempt: a second call
    /// for the same id (a finish followed by a drop notification) finds nothing.
    pub(crate) fn finish(&self, id: uuid::Uuid, outcome: AttemptOutcome) {
        let Some(mut open) = self.open.lock().ok().and_then(|mut a| a.remove(&id)) else {
            return;
        };
        let span = open.span.take();
        let pending = open.payload.take();
        let mut record = build(id, open, outcome, self.admission);
        if let Some(pending) = pending {
            // The document is the request and the answer as the provider saw them; the writer seals it and
            // queues the write, and the record carries only the reference.
            let document = serde_json::json!({
                "v": 1,
                "attempt_id": record.attempt_id,
                "ts": record.ts,
                "wire": crate::gateway::structure::wire_name(pending.wire),
                "provider": record.provider,
                "model": record.model,
                "outcome": record.outcome,
                "request": pending.request.json(),
                "response": pending.response.json(),
            });
            if let Ok(bytes) = serde_json::to_vec(&document) {
                record.payload = pending.writer.store(bytes);
            }
        }
        #[cfg(feature = "serve")]
        count_spend(&record);
        if let Some(span) = span {
            span.end(&record);
        }
        self.sink.record(&record);
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.open.lock().unwrap().len()
    }
}

/// Feed the spend and token counters (note 0034). Labels are the configured provider name and the
/// closed `cost_source` set, so the series count is bounded by the file.
#[cfg(feature = "serve")]
pub(crate) fn count_spend(record: &UsageRecord) {
    use super::metrics;
    let provider = record.provider.as_str();
    match (record.cost_usd, record.cost_source.as_deref()) {
        (Some(usd), source) if usd.is_finite() && usd > 0.0 => {
            let nanos = (usd * 1_000_000_000.0).round() as u64;
            metrics::add(
                metrics::COST_NANOUSD,
                &[
                    ("provider", provider),
                    ("source", source.unwrap_or("catalog")),
                ],
                nanos,
            );
        }
        (Some(_), _) => {}
        // An attempt that never reached the model (a refusal, a transport failure, an abandoned
        // call that saw no usage) cost nothing: it is not an under-report.
        (None, _) if record.usage_reported || record.outcome == "completed" => {
            metrics::incr(metrics::COST_UNKNOWN, &[("provider", provider)])
        }
        (None, _) => {}
    }
    for (kind, n) in [
        ("input", record.input_tokens),
        ("output", record.output_tokens),
        ("cache_read", record.cache_read_tokens),
        ("cache_write", record.cache_write_tokens),
    ] {
        if n > 0 {
            metrics::add(
                metrics::TOKENS,
                &[("provider", provider), ("kind", kind)],
                n,
            );
        }
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

/// The cost and its source, settled as the spend ledger settles an attempt.
fn settle(open: &Open, finalizes: bool) -> (Option<f64>, Option<String>) {
    let source = |usd: f64, name: &str| (Some(usd), Some(name.to_owned()));
    if finalizes {
        if let Some(bill) = open.terminal_provider_usd {
            return source(bill, "provider");
        }
        if let Some(estimate) = open.terminal_catalog_usd {
            return match open.max_provider_usd {
                Some(floor) if floor > estimate => source(floor, "provider_floor"),
                _ => source(estimate, "catalog"),
            };
        }
    }
    // Not finalized (the attempt failed or was abandoned, or reported nothing
    // terminal): what is known is a lower bound. A provider's reported charge
    // is that floor; failing that, the latest snapshot as it was stamped.
    if let Some(floor) = open.max_provider_usd {
        return source(floor, "provider_floor");
    }
    let usage = open.usage.as_ref();
    (
        usage.and_then(crate::cost::stamped),
        usage
            .and_then(|usage| usage.get("cost_source"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    )
}

fn build(
    id: uuid::Uuid,
    open: Open,
    outcome: AttemptOutcome,
    admission: &'static str,
) -> UsageRecord {
    let (cost_usd, cost_source) = settle(&open, may_finalize(outcome));
    let (outcome, http_status) = outcome_parts(outcome);
    let usage = open.usage.as_ref();
    let optional = |value: Option<&Value>| value.and_then(Value::as_u64);
    let count =
        |key: &str| -> u64 { optional(usage.and_then(|usage| usage.get(key))).unwrap_or(0) };
    // Chat-wire providers keep the reasoning count in the details object.
    let reasoning = optional(usage.and_then(|usage| usage.get("reasoning_tokens"))).or_else(|| {
        optional(
            usage.and_then(|usage| usage.pointer("/completion_tokens_details/reasoning_tokens")),
        )
    });
    UsageRecord {
        event: "usage",
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        attempt_id: id.to_string(),
        tenant: open.tenant,
        key: open.key,
        tags: open.tags,
        request_id: open.request_id,
        deployment: open.deployment,
        provider: open.provider,
        model: open.model,
        kind: open.kind,
        outcome,
        http_status,
        duration_ms: open.started.elapsed().as_millis() as u64,
        input_tokens: count("prompt_tokens"),
        output_tokens: count("completion_tokens"),
        reasoning_tokens: reasoning,
        uncached_input_tokens: optional(usage.and_then(|usage| usage.get("uncached_input_tokens"))),
        cache_read_tokens: count("cache_read_tokens"),
        cache_write_tokens: count("cache_write_tokens"),
        cost_usd,
        cost_source,
        usage_reported: usage.is_some(),
        admission,
        structure: open.structure,
        payload: None,
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::reasoning::WireFormat;
    use serde_json::json;

    fn body() -> Value {
        json!({"messages": [{"role": "user", "content": "hello"}]})
    }

    #[test]
    fn only_the_listed_credentials_capture_and_a_new_list_replaces_the_old() {
        let capture = StructureCapture::new(b"k".repeat(32), ["a".repeat(64)]);
        // A whole scope digest and the prefix a policy scope carries name the same credential.
        assert!(capture
            .segments(Some(&"a".repeat(64)), WireFormat::OpenAiChat, &body())
            .is_some());
        assert!(capture
            .segments(Some(&"a".repeat(16)), WireFormat::OpenAiChat, &body())
            .is_some());
        assert!(capture
            .segments(Some(&"b".repeat(64)), WireFormat::OpenAiChat, &body())
            .is_none());
        assert!(
            capture
                .segments(None, WireFormat::OpenAiChat, &body())
                .is_none(),
            "no key, no capture"
        );
        // Replacing the set stops the old client at once, for every clone of the handle.
        let clone = capture.clone();
        clone.set_clients(["b".repeat(64)]);
        assert!(capture
            .segments(Some(&"a".repeat(64)), WireFormat::OpenAiChat, &body())
            .is_none());
        assert!(capture
            .segments(Some(&"b".repeat(64)), WireFormat::OpenAiChat, &body())
            .is_some());
        clone.set_clients(Vec::<String>::new());
        assert!(capture
            .segments(Some(&"b".repeat(64)), WireFormat::OpenAiChat, &body())
            .is_none());
    }

    #[test]
    fn a_body_the_walker_cannot_read_records_no_structure() {
        let capture = StructureCapture::new(b"k".repeat(32), ["a".repeat(64)]);
        assert!(capture
            .segments(
                Some(&"a".repeat(64)),
                WireFormat::OpenAiChat,
                &json!({"prompt": "x"})
            )
            .is_none());
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

#[cfg(test)]
mod tenant_name_tests {
    use super::*;

    #[test]
    fn tenant_names_are_shared_grow_only_and_hash_keyed() {
        let names = TenantNames::new(["acme".to_string()]);
        let clone = names.clone();
        assert_eq!(
            names.name(&UsageSetup::tenant_key("acme")).as_deref(),
            Some("acme")
        );
        assert!(names.name(&UsageSetup::tenant_key("globex")).is_none());
        // A name added through one handle resolves through the other.
        clone.add(["globex".to_string()]);
        assert_eq!(
            names.name(&UsageSetup::tenant_key("globex")).as_deref(),
            Some("globex")
        );
        // Adding more never forgets: an attempt admitted before a reload removed
        // its tenant is still named when it ends.
        clone.add(["initech".to_string()]);
        assert!(names.name(&UsageSetup::tenant_key("acme")).is_some());
        // The table is keyed by the hash, never by the name.
        assert!(names.name("acme").is_none());
    }
}

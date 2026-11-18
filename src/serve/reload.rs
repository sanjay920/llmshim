//! Replacing what a running server reloads: the caller keys (0012), and the
//! providers, routes and provider rate limits (0022).
//!
//! Only those can change while the server runs. Listeners, TLS paths, fleet
//! settings and every other table are fixed at start, because they are held by
//! objects that cannot be swapped safely. A reload is therefore
//! all-or-nothing:
//!
//! 1. the new file must pass everything `--check` applies (shape, references,
//!    caller-key rules);
//! 2. everything outside `[[keys]]` must be exactly what the server started with,
//!    compared as parsed documents, and each difference is refused **by path**;
//! 3. only then is the new key set swapped in, in one step, for every holder of
//!    the key store (the gateway, the access log and the client-certificate
//!    binding), and new tenant and key names are made resolvable for the logs.
//!    (Names are added for any valid file, even one that changes no key: a rename
//!    is not a change to who may call.)
//!
//! A rejected reload changes nothing: the previous keys keep serving, so a typo
//! in a file never locks callers out and never silently keeps a revoked key
//! either, because the outcome is recorded where an operator looks.
//!
//! Rate buckets, spend windows and idempotency state are not touched: a reload
//! is not a limit reset.

use super::config::{Format, Issue, ServeConfig};
use super::keys::build_key_store;
use crate::gateway::auth::LiveKeys;
use crate::gateway::usage::{KeyNames, TenantNames};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// The parts of a running server that a reload replaces. Created while the
/// application is assembled; the application and the [`Reloader`] share them.
#[derive(Clone)]
pub struct ReloadHandles {
    pub(crate) keys: LiveKeys,
    pub(crate) tenants: TenantNames,
    pub(crate) key_names: KeyNames,
    pub(crate) allow: super::allowlist::AllowTable,
    /// The gateway's router (0022): providers and routes are replaced as one.
    pub(crate) router: crate::router::RouterHandle,
    /// Retunes every holder of the provider limits (0022). Called before the
    /// router is replaced, so a lowered limit takes hold no later than the
    /// provider that needs it.
    pub(crate) limits: Arc<dyn Fn(crate::proxy::ratelimit::RateLimitConfig) + Send + Sync>,
    /// A fleet replica's workers follow the provider set (0022): called after the
    /// router is replaced, so workers start for an added provider and a removed
    /// provider's stop. `None` for a single instance, whose lanes are made on demand.
    pub(crate) workers: Option<Arc<dyn Fn() + Send + Sync>>,
    /// How old the snapshot in force is, with `[control_plane]` (0024).
    pub(crate) clock: Option<super::snapshot::SnapshotClock>,
    pub(crate) status: StatusHandle,
    /// The audit log, when `[logging] audit_log` is set (note 0047).
    pub(crate) audit: Option<Arc<super::audit::AuditLog>>,
}

impl ReloadHandles {
    // One handle per thing a reload replaces: the list grows with what reloads.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        keys: LiveKeys,
        tenants: TenantNames,
        key_names: KeyNames,
        allow: super::allowlist::AllowTable,
        router: crate::router::RouterHandle,
        limits: Arc<dyn Fn(crate::proxy::ratelimit::RateLimitConfig) + Send + Sync>,
        workers: Option<Arc<dyn Fn() + Send + Sync>>,
        clock: Option<super::snapshot::SnapshotClock>,
    ) -> Self {
        Self {
            keys,
            tenants,
            key_names,
            allow,
            router,
            limits,
            workers,
            clock,
            status: StatusHandle::default(),
            audit: None,
        }
    }

    /// Record what this server changes in `audit` (note 0047).
    pub(crate) fn with_audit(mut self, audit: Option<Arc<super::audit::AuditLog>>) -> Self {
        self.audit = audit;
        self
    }

    /// The age and pull state of the snapshot in force, with `[control_plane]`.
    pub fn snapshot_clock(&self) -> Option<super::snapshot::SnapshotClock> {
        self.clock.clone()
    }

    /// What the admin listener reports.
    pub fn status(&self) -> ReloadStatus {
        let mut status = self.status.snapshot();
        status.snapshot = self.clock.as_ref().map(|clock| clock.status());
        status.audit = self.audit.as_ref().map(|audit| audit.head());
        status
    }

    pub(crate) fn status_handle(&self) -> StatusHandle {
        self.status.clone()
    }
}

/// One problem, as reported on the admin listener.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IssueView {
    pub path: String,
    pub message: String,
}

impl From<&Issue> for IssueView {
    fn from(issue: &Issue) -> Self {
        Self {
            path: issue.path.clone(),
            message: issue.message.clone(),
        }
    }
}

/// How the last attempt to reload ended.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReloadAttempt {
    pub at_epoch_secs: u64,
    /// `"applied"` or `"rejected"`.
    pub outcome: &'static str,
    /// Why it was rejected. Paths and messages only, never a value.
    pub issues: Vec<IssueView>,
    /// What an applied reload changed in the providers and routes, by name:
    /// `providers.added: x`, `providers.removed: x`, `providers.changed: x`,
    /// `routes`. Never a value.
    pub changes: Vec<String>,
}

/// What an operator needs to know that a reload took effect (or did not).
#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
pub struct ReloadStatus {
    /// 0 for the configuration the process started with; each applied reload
    /// adds one.
    pub generation: u64,
    /// SHA-256 of the configuration file in force.
    pub config_sha256: String,
    /// Caller credentials in force.
    pub credentials: usize,
    pub loaded_at_epoch_secs: u64,
    pub last_check_epoch_secs: Option<u64>,
    /// Whether the running keys are what the file says, as of the last check.
    /// False after a rejected file (the old keys keep serving) and while a
    /// valid change waits to be seen twice; true again once the file is
    /// accepted, applied, or put back to what is running. Alert on false.
    pub in_sync: bool,
    /// The latest applied or rejected reload. A no-change check is not recorded
    /// here.
    pub last_reload: Option<ReloadAttempt>,
    /// 0 until a reload replaces the providers or routes; each such reload adds one.
    pub router_generation: u64,
    /// 0 until a reload changes the rate limits; each such reload adds one. The
    /// limits are separate objects from the router (0022), so each has its own.
    pub limits_generation: u64,
    /// Providers whose `api_key` is an `{ env = ... }` reference. A process's
    /// environment cannot change while it runs, so a reload cannot rotate these:
    /// only a `{ file }` secret can be rotated (0022).
    pub env_secret_providers: Vec<String>,
    /// The policy snapshot in force, with `[control_plane]` (0024); `null` otherwise.
    pub snapshot: Option<super::snapshot::SnapshotStatus>,
    /// The audit log's head with `[logging] audit_log` (note 0047): how many records it holds and
    /// the hash the next carries. Anchor it somewhere the log's writer cannot reach: the file alone
    /// cannot show that records were removed from its end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit: Option<super::audit::Head>,
}

/// Shared, so the admin listener can report what the reloader did.
#[derive(Clone, Default)]
pub(crate) struct StatusHandle(Arc<Mutex<ReloadStatus>>);

impl StatusHandle {
    pub(crate) fn snapshot(&self) -> ReloadStatus {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn update(&self, change: impl FnOnce(&mut ReloadStatus)) {
        change(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

/// What one reload did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The new keys are in force.
    Applied { generation: u64, credentials: usize },
    /// The file, and every secret it references, resolve to what is in force.
    Unchanged,
    /// Nothing changed. `repeated` is true when the same file failed the same
    /// way last time, so a poller can stay quiet about it.
    Rejected { issues: Vec<Issue>, repeated: bool },
    /// A poll found a change that is valid but has not yet been seen twice. A
    /// file caught mid-write can parse as a smaller, valid set of keys (cut at a
    /// `[[keys]]` boundary), and applying it would revoke callers or drop a
    /// key's limits until the next check. A change is applied once two polls in
    /// a row see exactly the same one. A signal never defers.
    Deferred,
}

struct Active {
    /// The configuration document without `[[keys]]`, `[providers.*]` and
    /// `[routes.*]`, which a reload replaces.
    document: Value,
    /// What the providers and routes in force were built from.
    router: RouterPrint,
    /// What the rate limits in force were built from.
    limits: String,
    fingerprint: String,
    enforced: bool,
    /// A digest of the last rejection, to recognize a repeat.
    last_rejection: Option<String>,
    /// The change a poll saw last time and did not apply yet.
    pending: Option<String>,
    /// The snapshot this process applied (0024): a reload never goes behind it,
    /// whatever the floor file on disk says.
    applied: Option<super::snapshot::Floor>,
    /// The key names in force, to say which a reload added or removed (note 0047).
    key_names: std::collections::BTreeSet<String>,
}

/// Reloads `[[keys]]` from the configuration file the server started from.
pub struct Reloader {
    path: PathBuf,
    handles: ReloadHandles,
    active: Mutex<Active>,
}

impl Reloader {
    /// `text` is what `config` was parsed from, so later changes are measured
    /// against the start, not against a second read.
    pub fn new(
        path: &Path,
        text: &str,
        format: Format,
        config: &ServeConfig,
        env: &dyn Fn(&str) -> Option<String>,
        snapshot: Option<&super::snapshot::Snapshot>,
        handles: ReloadHandles,
    ) -> Result<Self, Vec<Issue>> {
        let file_issue = |message| {
            vec![Issue {
                path: "(file)".into(),
                message,
            }]
        };
        let document = document_without_reloadable(text, format).map_err(file_issue)?;
        let (providers_doc, routes_doc) = reloadable_parts(text, format).map_err(file_issue)?;
        let router = RouterPrint::of(config, &providers_doc, &routes_doc, env, None);
        let limits = limits_print(config);
        let current = handles.keys.current();
        let active = Active {
            document,
            fingerprint: format!(
                "{}:{}:{}:{}",
                current.fingerprint(),
                handles.allow.fingerprint(),
                router.fingerprint(),
                limits
            ),
            router,
            limits,
            enforced: current.is_enforced(),
            last_rejection: None,
            pending: None,
            key_names: config.keys.iter().map(|key| key.name.clone()).collect(),
            applied: snapshot.map(|snapshot| super::snapshot::Floor {
                org: snapshot.org.clone(),
                generation: snapshot.generation,
                payload_sha256: snapshot.payload_sha256.clone(),
                applied_at_epoch_secs: super::snapshot::epoch_secs(),
            }),
        };
        if let (Some(clock), Some(snapshot)) = (&handles.clock, snapshot) {
            // The floor of an earlier run says when this same snapshot first applied:
            // a restart does not make an old snapshot fresh (0024).
            let known = config
                .state
                .policy_dir
                .as_ref()
                .and_then(|dir| super::snapshot::read_floor(dir).ok().flatten())
                .filter(|floor| {
                    floor.org == snapshot.org
                        && floor.generation == snapshot.generation
                        && floor.payload_sha256 == snapshot.payload_sha256
                })
                .map(|floor| floor.applied_at_epoch_secs);
            clock.applied(snapshot.generation, &snapshot.signer, known);
        }
        let credentials = current.credential_count();
        let config_sha256 = sha256_hex(text.as_bytes());
        handles.status.update(|status| {
            *status = ReloadStatus {
                generation: 0,
                config_sha256,
                credentials,
                loaded_at_epoch_secs: epoch_secs(),
                last_check_epoch_secs: None,
                in_sync: true,
                last_reload: None,
                router_generation: 0,
                limits_generation: 0,
                env_secret_providers: active.router.env_form(),
                snapshot: None,
                audit: None,
            };
        });
        Ok(Self {
            path: path.to_path_buf(),
            handles,
            active: Mutex::new(active),
        })
    }

    /// What the admin listener reports.
    pub fn status(&self) -> ReloadStatus {
        self.handles.status()
    }

    /// Read the file again and, if all of it is acceptable, replace the keys at
    /// once. For an operator's signal. Blocking: it reads files, so call it from
    /// a blocking task.
    pub fn reload(&self, env: &dyn Fn(&str) -> Option<String>) -> Outcome {
        self.run(env, false)
    }

    /// [`Self::reload`] for a timer: a change is applied only after two polls in
    /// a row have seen exactly the same one ([`Outcome::Deferred`] until then).
    pub fn poll(&self, env: &dyn Fn(&str) -> Option<String>) -> Outcome {
        self.run(env, true)
    }

    fn run(&self, env: &dyn Fn(&str) -> Option<String>, debounce: bool) -> Outcome {
        // One reload at a time; a second caller waits and then sees the first's
        // result as its starting point.
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        let now = epoch_secs();
        let step = self.attempt(&mut active, env, debounce);
        self.handles.status.update(|status| {
            status.last_check_epoch_secs = Some(now);
            match &step {
                Step::Applied {
                    generation,
                    credentials,
                    config_sha256,
                    changes,
                    env_form,
                    ..
                } => {
                    if changes.iter().any(|change| change != "limits") {
                        status.router_generation += 1;
                    }
                    if changes.iter().any(|change| change == "limits") {
                        status.limits_generation += 1;
                    }
                    status.env_secret_providers.clone_from(env_form);
                    status.generation = *generation;
                    status.credentials = *credentials;
                    status.in_sync = true;
                    status.config_sha256.clone_from(config_sha256);
                    status.last_reload = Some(ReloadAttempt {
                        at_epoch_secs: now,
                        outcome: "applied",
                        issues: Vec::new(),
                        changes: changes.clone(),
                    });
                }
                Step::Unchanged { config_sha256 } => {
                    status.config_sha256.clone_from(config_sha256);
                    // Whatever was rejected earlier has been put right: the file
                    // is what is running again.
                    status.in_sync = true;
                }
                Step::Deferred => status.in_sync = false,
                Step::Rejected { issues, repeated } => {
                    status.in_sync = false;
                    // A repeat keeps the time it first failed.
                    if !repeated {
                        status.last_reload = Some(ReloadAttempt {
                            at_epoch_secs: now,
                            outcome: "rejected",
                            issues: issues.iter().map(IssueView::from).collect(),
                            changes: Vec::new(),
                        });
                    }
                }
            }
        });
        self.audit_step(&step, debounce);
        match step {
            Step::Applied {
                generation,
                credentials,
                ..
            } => Outcome::Applied {
                generation,
                credentials,
            },
            Step::Unchanged { .. } => Outcome::Unchanged,
            Step::Deferred => Outcome::Deferred,
            Step::Rejected { issues, repeated } => Outcome::Rejected { issues, repeated },
        }
    }

    /// Record what a reload did in the audit log (note 0047): an applied reload with what changed
    /// and which keys were added or removed by name, and a refused one (the first time) with the
    /// paths of the settings at fault, never their values or the issues' text. Unchanged and
    /// deferred checks change nothing and are not recorded. A failed write is counted and
    /// reported; the log then takes no more records.
    fn audit_step(&self, step: &Step, debounce: bool) {
        let Some(audit) = &self.handles.audit else {
            return;
        };
        let actor = if debounce { "poller" } else { "operator" };
        let written = match step {
            Step::Applied {
                generation,
                credentials,
                config_sha256,
                changes,
                keys_added,
                keys_removed,
                snapshot,
                ..
            } => audit.record(
                "config.reloaded",
                actor,
                "applied",
                json!({
                    "generation": generation,
                    "credentials": credentials,
                    "config_sha256": config_sha256,
                    "changes": changes,
                    "keys_added": keys_added,
                    "keys_removed": keys_removed,
                    "snapshot": snapshot,
                }),
            ),
            Step::Rejected {
                issues,
                repeated: false,
            } => audit.record(
                "config.reloaded",
                actor,
                "rejected",
                json!({
                    "paths": issues.iter().map(|issue| issue.path.clone()).collect::<Vec<_>>(),
                }),
            ),
            _ => return,
        };
        if let Err(message) = written {
            crate::gateway::metrics::incr(crate::gateway::metrics::AUDIT_WRITE_FAILURES, &[]);
            eprintln!("error: the audit log could not record a reload: {message}");
        }
    }

    /// The snapshot just recorded is the one in force: its age restarts if it is a new
    /// generation.
    fn note_applied(&self, loaded: &super::config::Loaded) {
        if let (Some(clock), Some(snapshot)) = (&self.handles.clock, &loaded.snapshot) {
            clock.applied(snapshot.generation, &snapshot.signer, None);
            self.mark_signer(Some(&loaded.config));
        }
    }

    /// Whether the signer of the snapshot in force is still trusted by `config`.
    fn mark_signer(&self, config: Option<&ServeConfig>) {
        let (Some(clock), Some(control_plane)) = (
            &self.handles.clock,
            config.and_then(|config| config.control_plane.as_ref()),
        ) else {
            return;
        };
        let signer = clock.status().signer;
        clock.set_signer_untrusted(
            !signer.is_empty() && !control_plane.verify_keys.contains(&signer),
        );
    }

    fn attempt(
        &self,
        active: &mut Active,
        env: &dyn Fn(&str) -> Option<String>,
        debounce: bool,
    ) -> Step {
        let loaded = match ServeConfig::load_with_text(&self.path) {
            Ok(loaded) => {
                self.mark_signer(Some(&loaded.config));
                loaded
            }
            Err(issues) => {
                // The file may name fewer trusted keys than the signer of what is
                // serving: say so even though the reload is refused.
                self.mark_signer(
                    ServeConfig::load_with_text_resolving(&self.path, false)
                        .ok()
                        .as_ref()
                        .map(|loaded| &loaded.config),
                );
                return reject(active, "", issues);
            }
        };
        let config_sha256 = sha256_hex(loaded.text.as_bytes());
        let reject_now = |active: &mut Active, issues| reject(active, &config_sha256, issues);
        if let (Some(snapshot), Some(applied)) = (&loaded.snapshot, &active.applied) {
            if let Err(issue) = super::snapshot::against_applied(snapshot, applied) {
                return reject_now(active, vec![issue]);
            }
        }

        // Everything outside the keys must be what the server started with.
        let parsed =
            document_without_reloadable(&loaded.text, loaded.format).and_then(|document| {
                reloadable_parts(&loaded.text, loaded.format)
                    .map(|(providers, routes)| (document, providers, routes))
            });
        let (document, providers_doc, routes_doc) = match parsed {
            Ok(parts) => parts,
            Err(message) => {
                return reject_now(
                    active,
                    vec![Issue {
                        path: "(file)".into(),
                        message,
                    }],
                )
            }
        };
        let mut issues: Vec<Issue> = Vec::new();
        let mut changed = Vec::new();
        changed_paths(&active.document, &document, "", &mut changed);
        issues.extend(changed.into_iter().map(|path| Issue {
            path,
            message: "cannot change while the server is running; restart to apply".into(),
        }));
        issues.extend(loaded.config.feature_issues());
        // The rules about caller keys, and about their values against the
        // upstream keys. A provider, admin or Redis secret that cannot be read
        // right now must not stop a revocation from applying: an unreadable
        // secret of a provider whose table is unchanged counts as unchanged
        // (0022), and the admin and Redis secrets are outside the reload.
        issues.extend(loaded.config.check_same_values(env));
        if !issues.is_empty() {
            return reject_now(active, issues);
        }

        let store = match build_key_store(&loaded.config, env) {
            Ok(store) => store,
            Err(issues) => return reject_now(active, issues),
        };
        // Unreachable through a file, since `server.allow_unauthenticated` is
        // outside the keys; kept so that no future change to the rules can turn
        // authentication off or on by reload.
        if store.is_enforced() != active.enforced {
            return reject_now(
                active,
                vec![Issue {
                    path: "keys".into(),
                    message: "caller authentication cannot be turned on or off while the \
                              server is running; restart to apply"
                        .into(),
                }],
            );
        }

        // Providers and routes (0022): what they were built from, by digest.
        let router = RouterPrint::of(
            &loaded.config,
            &providers_doc,
            &routes_doc,
            env,
            Some(&active.router),
        );
        let mut changes = active.router.changes_to(&router);
        let limits = limits_print(&loaded.config);
        let limits_changed = limits != active.limits;
        let mut next_router = None;
        if !changes.is_empty() {
            // Built whole before anything is replaced; a provider that cannot be
            // built rejects the reload by path and the old router keeps serving.
            let mut found = Vec::new();
            let base = self.handles.router.current();
            let built = super::build::build_router_from(&loaded.config, env, &mut found, &base);
            // The credential files of the providers this reload adds or changes: a
            // file that is missing, empty or malformed rejects it (a transport that
            // reads nothing until a request would otherwise fail every one).
            let mut files = loaded.config.check_secrets_present(env);
            files.extend(loaded.config.check_credential_files());
            for issue in files {
                if router.touches(&changes, &issue.path)
                    && !found.iter().any(|seen| seen.path == issue.path)
                {
                    found.push(issue);
                }
            }
            if !found.is_empty() {
                return reject_now(active, found);
            }
            next_router = Some(built);
        }

        active.last_rejection = None;
        // Labels first, and whether or not anything else changed: a request
        // admitted under new keys must find its tenant's and its key's name
        // already resolvable, and a rename is not a change to who may call (the
        // fingerprint ignores names) yet the logs should say the new name.
        // Adding is idempotent, and a name is a label, never a permission.
        self.handles
            .tenants
            .add(loaded.config.keys.iter().map(|key| key.tenant.clone()));
        self.handles
            .key_names
            .add(super::keys::key_names(&loaded.config, env));
        self.handles
            .key_names
            .add_tags(super::keys::key_tags(&loaded.config, env));
        // Who may call and from where change together or not at all: one
        // fingerprint, one debounce.
        let allowlists = super::keys::allowlists(&loaded.config, env);
        let fingerprint = format!(
            "{}:{}:{}:{}",
            store.fingerprint(),
            super::allowlist::AllowTable::fingerprint_of(&allowlists),
            router.fingerprint(),
            limits
        );
        if limits_changed {
            changes.push("limits".to_string());
        }
        if fingerprint == active.fingerprint {
            active.pending = None;
            // The same keys, but possibly a higher generation: the floor follows.
            if let Err(issue) = record_snapshot(&loaded, &mut active.applied) {
                return reject_now(active, vec![issue]);
            }
            self.note_applied(&loaded);
            return Step::Unchanged { config_sha256 };
        }
        if debounce {
            let candidate = format!("{config_sha256}:{fingerprint}");
            if active.pending.as_deref() != Some(candidate.as_str()) {
                active.pending = Some(candidate);
                return Step::Deferred;
            }
        }
        active.pending = None;
        if let Err(issue) = record_snapshot(&loaded, &mut active.applied) {
            return reject_now(active, vec![issue]);
        }
        self.note_applied(&loaded);
        let credentials = store.credential_count();
        // The allowlists first, and keyed by credential rather than by asking the
        // store (see `allowlist::enforce`): a key that is new in this reload and
        // restricted is already bound when the store starts to know it.
        self.handles.allow.replace(allowlists);
        self.handles.keys.replace(store);
        // The limits first, then the router (0022): a lowered limit holds no later
        // than the provider that needs it.
        if limits_changed {
            (self.handles.limits)(super::build::rate_limit_config(&loaded.config));
            active.limits = limits;
        }
        if let Some(next) = next_router {
            self.handles.router.replace(next);
            // Then the workers (a fleet): a job for an added provider is leased by
            // this replica from here on.
            if let Some(workers) = &self.handles.workers {
                workers();
            }
        }
        let env_form = router.env_form();
        active.router = router;
        active.fingerprint = fingerprint;
        let generation = self.handles.status.snapshot().generation + 1;
        // Which keys this reload added or removed, by name (never a secret or a digest).
        let names: std::collections::BTreeSet<String> = loaded
            .config
            .keys
            .iter()
            .map(|key| key.name.clone())
            .collect();
        let keys_added = names.difference(&active.key_names).cloned().collect();
        let keys_removed = active.key_names.difference(&names).cloned().collect();
        active.key_names = names;
        let snapshot = loaded.snapshot.as_ref().map(|snapshot| {
            json!({"org": snapshot.org, "generation": snapshot.generation, "signer": snapshot.signer})
        });
        Step::Applied {
            generation,
            credentials,
            config_sha256,
            changes,
            env_form,
            keys_added,
            keys_removed,
            snapshot,
        }
    }
}

enum Step {
    Applied {
        generation: u64,
        credentials: usize,
        config_sha256: String,
        changes: Vec<String>,
        env_form: Vec<String>,
        keys_added: Vec<String>,
        keys_removed: Vec<String>,
        snapshot: Option<Value>,
    },
    Unchanged {
        config_sha256: String,
    },
    Deferred,
    Rejected {
        issues: Vec<Issue>,
        repeated: bool,
    },
}

/// Record the snapshot a load verified as the one in force (0024): a replica that
/// restarts must not accept an older one.
fn record_snapshot(
    loaded: &super::config::Loaded,
    applied: &mut Option<super::snapshot::Floor>,
) -> Result<(), Issue> {
    if let (Some(snapshot), Some(dir)) = (&loaded.snapshot, loaded.config.state.policy_dir.as_ref())
    {
        let floor = super::snapshot::Floor {
            org: snapshot.org.clone(),
            generation: snapshot.generation,
            payload_sha256: snapshot.payload_sha256.clone(),
            applied_at_epoch_secs: super::snapshot::epoch_secs(),
        };
        super::snapshot::write_floor(dir, &floor)?;
        *applied = Some(floor);
    }
    Ok(())
}

fn reject(active: &mut Active, config_sha256: &str, issues: Vec<Issue>) -> Step {
    let digest = sha256_hex(
        format!(
            "{config_sha256}\n{}",
            issues
                .iter()
                .map(Issue::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        )
        .as_bytes(),
    );
    let repeated = active.last_rejection.as_deref() == Some(digest.as_str());
    active.last_rejection = Some(digest);
    active.pending = None;
    Step::Rejected { issues, repeated }
}

fn parse_document(text: &str, format: Format) -> Result<Value, String> {
    match format {
        Format::Toml => toml::from_str(text).map_err(|error| error.message().to_string()),
        Format::Json => serde_json::from_str(text).map_err(|error| error.to_string()),
    }
}

/// The file as a generic document without what a reload replaces: the `keys`
/// array, `[tenants.*]`, `[providers.*]` and `[routes.*]`.
fn document_without_reloadable(text: &str, format: Format) -> Result<Value, String> {
    let mut document = parse_document(text, format)?;
    if let Value::Object(table) = &mut document {
        // `tenants` (budgets, note 0030) reload with the keys: a tenant's cap is part of
        // the identity of every key under it.
        for name in ["keys", "tenants", "providers", "routes", "deployments"] {
            table.remove(name);
        }
        // A verify key can be added or removed without a restart (0024): the next
        // load verifies against the new list. The rest of `[control_plane]` is fixed.
        if let Some(Value::Object(control_plane)) = table.get_mut("control_plane") {
            control_plane.remove("verify_keys");
        }
        // The rate limits reload too; the rest of `[limits]` does not. An emptied
        // table is dropped so that a file with only limits and one with no
        // `[limits]` table at all compare equal.
        if let Some(Value::Object(limits)) = table.get_mut("limits") {
            for name in ["rpm", "tpm", "providers"] {
                limits.remove(name);
            }
            if limits.is_empty() {
                table.remove("limits");
            }
        }
    }
    Ok(document)
}

/// The `providers` and `routes` tables as written.
fn reloadable_parts(text: &str, format: Format) -> Result<(Value, Value), String> {
    let document = parse_document(text, format)?;
    let part = |name: &str| document.get(name).cloned().unwrap_or(Value::Null);
    // Deployments rebuild the router like routes do, so they travel with them (note 0031).
    Ok((
        part("providers"),
        serde_json::json!({"routes": part("routes"), "deployments": part("deployments")}),
    ))
}

/// What the provider rate limits are, as text to compare: the values only.
fn limits_print(config: &ServeConfig) -> String {
    format!(
        "{:?}:{:?}:{:?}",
        config.limits.rpm, config.limits.tpm, config.limits.providers
    )
}

/// What one provider was built from: its table as written, and digests of the
/// secret and of the credential files it reads. Digests only: nothing here is
/// a secret, and nothing here is ever reported but by provider name.
#[derive(Clone, PartialEq, Eq)]
struct ProviderPrint {
    table: String,
    secret: String,
    files: String,
}

/// What the providers and routes in force were built from (0022).
#[derive(Clone, Default)]
struct RouterPrint {
    providers: BTreeMap<String, ProviderPrint>,
    routes: String,
    env_form: Vec<String>,
}

impl RouterPrint {
    fn of(
        config: &ServeConfig,
        providers_doc: &Value,
        routes_doc: &Value,
        env: &dyn Fn(&str) -> Option<String>,
        previous: Option<&RouterPrint>,
    ) -> Self {
        let mut providers = BTreeMap::new();
        let mut env_form = Vec::new();
        for (name, provider) in &config.providers {
            let table = providers_doc
                .get(name)
                .map(Value::to_string)
                .unwrap_or_default();
            let before = previous.and_then(|print| print.providers.get(name));
            let secret = match &provider.api_key {
                None => "-".to_string(),
                Some(reference) => {
                    if reference.env.is_some() {
                        env_form.push(name.clone());
                    }
                    match super::config::read_secret(reference, env) {
                        Ok(value) => sha256_hex(value.as_bytes()),
                        // Unreadable now: a provider whose table is unchanged keeps
                        // what it had (an unrelated edit is not blocked by it); a
                        // changed one is distinct, so the rebuild reports the path.
                        Err(_) => match before {
                            Some(before) if before.table == table => before.secret.clone(),
                            _ => "unreadable".to_string(),
                        },
                    }
                }
            };
            let digest_of = |path: &Option<PathBuf>| -> String {
                path.as_ref()
                    .map(|path| match std::fs::read(path) {
                        Ok(bytes) => sha256_hex(&bytes),
                        Err(_) => "unreadable".to_string(),
                    })
                    .unwrap_or_else(|| "-".to_string())
            };
            let files = format!(
                "{}:{}:{}",
                digest_of(&provider.service_account_file),
                digest_of(&provider.config_file),
                digest_of(&provider.credentials_file)
            );
            providers.insert(
                name.clone(),
                ProviderPrint {
                    table,
                    secret,
                    files,
                },
            );
        }
        Self {
            providers,
            routes: routes_doc.to_string(),
            env_form,
        }
    }

    fn fingerprint(&self) -> String {
        let mut text = self.routes.clone();
        for (name, print) in &self.providers {
            text.push_str(&format!(
                "\n{name}|{}|{}|{}",
                print.table, print.secret, print.files
            ));
        }
        sha256_hex(text.as_bytes())
    }

    fn env_form(&self) -> Vec<String> {
        self.env_form.clone()
    }

    /// What changed from `self` to `next`, by name.
    fn changes_to(&self, next: &RouterPrint) -> Vec<String> {
        let mut changes = Vec::new();
        for (name, print) in &next.providers {
            match self.providers.get(name) {
                None => changes.push(format!("providers.added: {name}")),
                Some(before) if before != print => {
                    changes.push(format!("providers.changed: {name}"))
                }
                Some(_) => {}
            }
        }
        for name in self.providers.keys() {
            if !next.providers.contains_key(name) {
                changes.push(format!("providers.removed: {name}"));
            }
        }
        if self.routes != next.routes {
            changes.push("routes".to_string());
        }
        changes
    }

    /// Whether `path` (`providers.<name>.<field>`) belongs to a provider the
    /// changes name as added or changed.
    fn touches(&self, changes: &[String], path: &str) -> bool {
        changes.iter().any(|change| {
            change
                .strip_prefix("providers.added: ")
                .or_else(|| change.strip_prefix("providers.changed: "))
                .is_some_and(|name| path.starts_with(&format!("providers.{name}.")))
        })
    }
}

/// Dotted paths at which `old` and `new` differ. Values are never reported.
fn changed_paths(old: &Value, new: &Value, at: &str, out: &mut Vec<String>) {
    match (old, new) {
        (Value::Object(old), Value::Object(new)) => {
            let names: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
            for name in names {
                let path = if at.is_empty() {
                    name.clone()
                } else {
                    format!("{at}.{name}")
                };
                match (old.get(name), new.get(name)) {
                    (Some(old), Some(new)) => changed_paths(old, new, &path, out),
                    _ => out.push(path),
                }
            }
        }
        _ if old != new => out.push(if at.is_empty() {
            "(file)".into()
        } else {
            at.into()
        }),
        _ => {}
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::auth::KeyStore;
    use axum::http::HeaderMap;
    use std::io::Write;

    const OLD: &str = "sk-old-0123456789abcdef";
    const NEW: &str = "sk-new-0123456789abcdef";

    fn digest(token: &str) -> String {
        sha256_hex(token.as_bytes())
    }

    fn env(name: &str) -> Option<String> {
        (name == "OPENAI_API_KEY").then(|| "sk-upstream".to_string())
    }

    fn key(name: &str, tenant: &str, token: &str, extra: &str) -> String {
        format!(
            "[[keys]]\nname = \"{name}\"\ntenant = \"{tenant}\"\nsha256 = \"{}\"\n{extra}\n",
            digest(token)
        )
    }

    fn text(top: &str, keys: &str) -> String {
        format!(
            "version = 1\n{top}\n[providers.openai]\nkind = \"openai\"\n\
             api_key = {{ env = \"OPENAI_API_KEY\" }}\n{keys}"
        )
    }

    struct Fixture {
        directory: PathBuf,
        file: PathBuf,
        keys: LiveKeys,
        router: crate::router::RouterHandle,
        limiter: Arc<crate::proxy::ratelimit::InMemoryRateLimiter>,
        /// How many times the fleet's workers were asked to follow the providers.
        synced: Arc<std::sync::atomic::AtomicUsize>,
        reloader: Reloader,
    }

    impl Fixture {
        fn new(initial: &str) -> Self {
            Self::with_fleet(initial, false)
        }

        fn with_fleet(initial: &str, fleet: bool) -> Self {
            let directory = std::env::temp_dir()
                .join(format!("llmshim-reload-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&directory).unwrap();
            let file = directory.join("gateway.toml");
            std::fs::write(&file, initial).unwrap();
            let loaded = ServeConfig::load_with_text(&file).unwrap();
            let store = build_key_store(&loaded.config, &env).unwrap();
            let keys = LiveKeys::new(store);
            let tenants = TenantNames::new(loaded.config.keys.iter().map(|k| k.tenant.clone()));
            let key_names = KeyNames::new(crate::serve::keys::key_names(&loaded.config, &env));
            let mut issues = Vec::new();
            let router = crate::router::RouterHandle::new(
                super::super::build::build_router_for_test(&loaded.config, &env, &mut issues),
            );
            assert!(issues.is_empty(), "{issues:?}");
            let limiter = Arc::new(crate::proxy::ratelimit::InMemoryRateLimiter::new(
                super::super::build::rate_limit_config(&loaded.config),
            ));
            let retuned = limiter.clone();
            let synced = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = synced.clone();
            let reloader = Reloader::new(
                &file,
                &loaded.text,
                loaded.format,
                &loaded.config,
                &env,
                loaded.snapshot.as_ref(),
                ReloadHandles::new(
                    keys.clone(),
                    tenants,
                    key_names,
                    crate::serve::allowlist::AllowTable::default(),
                    router.clone(),
                    Arc::new(move |limits| {
                        crate::proxy::ratelimit::RateLimiter::retune(&*retuned, limits)
                    }),
                    fleet.then(|| {
                        Arc::new(move || {
                            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }) as Arc<dyn Fn() + Send + Sync>
                    }),
                    loaded
                        .config
                        .control_plane
                        .as_ref()
                        .map(crate::serve::snapshot::SnapshotClock::new),
                ),
            )
            .unwrap();
            Self {
                directory,
                file,
                keys,
                router,
                limiter,
                synced,
                reloader,
            }
        }

        fn write(&self, text: &str) {
            std::fs::write(&self.file, text).unwrap();
        }

        fn reload(&self) -> Outcome {
            self.reloader.reload(&env)
        }

        fn poll(&self) -> Outcome {
            self.reloader.poll(&env)
        }

        fn identity(&self, token: &str) -> Option<crate::gateway::auth::Identity> {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
            KeyStore::Live(self.keys.clone()).identify(&headers).ok()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn a_reload_adds_revokes_and_relimits_keys() {
        let fixture = Fixture::new(&text("", &key("a", "acme", OLD, "tier = 1")));
        assert_eq!(fixture.identity(OLD).unwrap().tier, 1);
        assert!(fixture.identity(NEW).is_none());

        fixture.write(&text(
            "",
            &format!(
                "{}{}",
                key("a", "acme", OLD, "tier = 7\nrpm = 5"),
                key("b", "globex", NEW, "")
            ),
        ));
        assert_eq!(
            fixture.reload(),
            Outcome::Applied {
                generation: 1,
                credentials: 2
            }
        );
        let relimited = fixture.identity(OLD).unwrap();
        assert_eq!((relimited.tier, relimited.rpm), (7, Some(5)));
        assert_eq!(fixture.identity(NEW).unwrap().tenant, "globex");

        // Revoke the first key.
        fixture.write(&text("", &key("b", "globex", NEW, "")));
        assert!(matches!(
            fixture.reload(),
            Outcome::Applied { generation: 2, .. }
        ));
        assert!(fixture.identity(OLD).is_none());
        assert!(fixture.identity(NEW).is_some());

        let status = fixture.reloader.status();
        assert_eq!(status.generation, 2);
        assert_eq!(status.credentials, 1);
        assert_eq!(status.last_reload.unwrap().outcome, "applied");
    }

    #[test]
    fn a_poll_applies_a_change_only_once_it_has_seen_the_same_one_twice() {
        let fixture = Fixture::new(&text(
            "",
            &format!("{}{}", key("a", "acme", OLD, ""), key("c", "acme", NEW, "")),
        ));
        // A file caught mid-write can parse as a smaller, valid set of keys.
        fixture.write(&text("", &key("a", "acme", OLD, "")));
        assert_eq!(fixture.poll(), Outcome::Deferred);
        assert!(fixture.identity(NEW).is_some(), "nothing was applied yet");
        assert!(!fixture.reloader.status().in_sync);
        // The write finishes: a different file, so the first sighting is void.
        fixture.write(&text(
            "",
            &format!(
                "{}{}",
                key("a", "acme", OLD, ""),
                key("c", "acme", NEW, "tier = 3")
            ),
        ));
        assert_eq!(fixture.poll(), Outcome::Deferred);
        assert_eq!(fixture.identity(NEW).unwrap().tier, 0);
        // Seen twice in a row, it is applied.
        assert!(matches!(
            fixture.poll(),
            Outcome::Applied { generation: 1, .. }
        ));
        assert_eq!(fixture.identity(NEW).unwrap().tier, 3);
        assert!(fixture.reloader.status().in_sync);
    }

    #[test]
    fn a_signal_applies_a_change_at_once() {
        let fixture = Fixture::new(&text("", &key("a", "acme", OLD, "")));
        fixture.write(&text("", &key("b", "globex", NEW, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        assert!(fixture.identity(OLD).is_none());
    }

    #[test]
    fn a_change_that_comes_and_goes_between_polls_is_never_applied() {
        let initial = text("", &key("a", "acme", OLD, ""));
        let fixture = Fixture::new(&initial);
        fixture.write(&text("", &key("b", "globex", NEW, "")));
        assert_eq!(fixture.poll(), Outcome::Deferred);
        // Put back before the second look.
        fixture.write(&initial);
        assert_eq!(fixture.poll(), Outcome::Unchanged);
        // And a later sighting of the change starts over.
        fixture.write(&text("", &key("b", "globex", NEW, "")));
        assert_eq!(fixture.poll(), Outcome::Deferred);
        assert!(fixture.identity(OLD).is_some());
        assert!(!fixture.reloader.status().in_sync);
    }

    #[test]
    fn putting_a_rejected_file_back_clears_the_alert() {
        let initial = text("", &key("a", "acme", OLD, ""));
        let fixture = Fixture::new(&initial);
        fixture.write(&text(
            "[limits]\nqueue_timeout_ms = 1",
            &key("a", "acme", OLD, ""),
        ));
        assert!(matches!(fixture.reload(), Outcome::Rejected { .. }));
        let rejected = fixture.reloader.status();
        assert!(!rejected.in_sync, "the file is not what is running");
        assert_eq!(rejected.last_reload.as_ref().unwrap().outcome, "rejected");
        // The edit is reverted: the running keys are the file's again.
        fixture.write(&initial);
        assert_eq!(fixture.reload(), Outcome::Unchanged);
        let status = fixture.reloader.status();
        assert!(
            status.in_sync,
            "an operator alerting on in_sync sees it clear"
        );
        // The history still says what happened.
        assert_eq!(status.last_reload.unwrap().outcome, "rejected");
    }

    #[test]
    fn an_unreadable_provider_secret_does_not_block_a_revocation() {
        let fixture = Fixture::new(&text(
            "",
            &format!(
                "{}{}",
                key("a", "acme", OLD, ""),
                key("b", "globex", NEW, "")
            ),
        ));
        fixture.write(&text("", &key("b", "globex", NEW, "")));
        // The environment can no longer resolve the provider's key, which has
        // nothing to do with who may call.
        let nothing = |_: &str| None;
        assert!(matches!(
            fixture.reloader.reload(&nothing),
            Outcome::Applied { .. }
        ));
        assert!(fixture.identity(OLD).is_none());
    }

    #[test]
    fn a_file_that_resolves_to_the_same_keys_is_no_change() {
        let initial = text("", &key("a", "acme", OLD, ""));
        let fixture = Fixture::new(&initial);
        let before = fixture.reloader.status().config_sha256;
        assert_eq!(fixture.reload(), Outcome::Unchanged);
        // A comment changes the bytes but not what is in force.
        fixture.write(&format!("# reviewed\n{initial}"));
        assert_eq!(fixture.reload(), Outcome::Unchanged);
        let status = fixture.reloader.status();
        assert_eq!(status.generation, 0);
        assert_ne!(status.config_sha256, before, "the digest follows the file");
        assert!(status.last_check_epoch_secs.is_some());
        assert!(status.last_reload.is_none());
    }

    #[test]
    fn a_file_that_does_not_load_changes_nothing_and_echoes_no_secret() {
        let initial = text("", &key("a", "acme", OLD, ""));
        let fixture = Fixture::new(&initial);
        // Truncated mid-key, as a half-written file looks.
        let broken = &initial[..initial.len() - 20];
        fixture.write(broken);
        let Outcome::Rejected { issues, repeated } = fixture.reload() else {
            panic!("a broken file must be rejected");
        };
        assert!(!repeated);
        assert_eq!(issues[0].path, "(file)");
        assert!(fixture.identity(OLD).is_some(), "the old keys keep serving");

        // The file is missing altogether.
        std::fs::remove_file(&fixture.file).unwrap();
        assert!(matches!(
            fixture.reload(),
            Outcome::Rejected {
                repeated: false,
                ..
            }
        ));
        assert!(fixture.identity(OLD).is_some());

        let status = fixture.reloader.status();
        assert_eq!(status.generation, 0);
        let attempt = status.last_reload.unwrap();
        assert_eq!(attempt.outcome, "rejected");
        let shown = format!("{attempt:?}");
        for secret in [OLD, "sk-upstream", digest(OLD).as_str()] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[test]
    fn a_change_outside_the_keys_is_refused_by_path_and_changes_nothing() {
        let fixture = Fixture::new(&text("[server]\nport = 3000", &key("a", "acme", OLD, "")));
        // Also revokes the key and adds one, which must not be applied alone.
        fixture.write(&text(
            "[server]\nport = 3001\n[limits]\nmax_concurrency = 10",
            &key("b", "globex", NEW, ""),
        ));
        let Outcome::Rejected { issues, .. } = fixture.reload() else {
            panic!("a restart-only change must be refused");
        };
        let paths: Vec<&str> = issues.iter().map(|issue| issue.path.as_str()).collect();
        assert_eq!(paths, ["limits", "server.port"]);
        assert!(issues
            .iter()
            .all(|issue| issue.message.contains("restart to apply")));
        // All or nothing: the new key was not added and the old one stays.
        assert!(fixture.identity(NEW).is_none());
        assert!(fixture.identity(OLD).is_some());
        // The values stay out of the report.
        assert!(!format!("{issues:?}").contains("3001"));
    }

    #[test]
    fn the_same_failure_is_flagged_as_a_repeat_until_the_file_changes() {
        let initial = text("", &key("a", "acme", OLD, ""));
        let fixture = Fixture::new(&initial);
        fixture.write(&text(
            "[limits]\nqueue_timeout_ms = 1",
            &key("a", "acme", OLD, ""),
        ));
        assert!(matches!(
            fixture.reload(),
            Outcome::Rejected {
                repeated: false,
                ..
            }
        ));
        let first = fixture.reloader.status().last_reload.unwrap().at_epoch_secs;
        assert!(matches!(
            fixture.reload(),
            Outcome::Rejected { repeated: true, .. }
        ));
        assert_eq!(
            fixture.reloader.status().last_reload.unwrap().at_epoch_secs,
            first,
            "a repeat keeps the time it first failed"
        );
        // A different failure is news again.
        fixture.write(&text(
            "[limits]\nqueue_timeout_ms = 2",
            &key("a", "acme", OLD, ""),
        ));
        assert!(matches!(
            fixture.reload(),
            Outcome::Rejected {
                repeated: false,
                ..
            }
        ));
        // Putting it right applies, and clears the memory of the failure.
        fixture.write(&text("", &key("b", "globex", NEW, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
    }

    #[test]
    fn a_key_that_breaks_a_caller_key_rule_is_rejected_whole() {
        let fixture = Fixture::new(&text("", &key("a", "acme", OLD, "")));
        // Two keys with the same digest.
        fixture.write(&text(
            "",
            &format!(
                "{}{}",
                key("a", "acme", OLD, ""),
                key("b", "globex", OLD, "")
            ),
        ));
        assert!(matches!(fixture.reload(), Outcome::Rejected { .. }));
        assert_eq!(fixture.reloader.status().generation, 0);
        assert!(fixture.identity(OLD).is_some());
    }

    #[test]
    fn a_rotated_secret_file_is_picked_up_though_the_config_is_unchanged() {
        let directory =
            std::env::temp_dir().join(format!("llmshim-secret-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).unwrap();
        let secret = directory.join("caller.key");
        std::fs::write(&secret, OLD).unwrap();
        let config = text(
            "",
            &format!(
                "[[keys]]\nname = \"a\"\ntenant = \"acme\"\nsecret = {{ file = \"{}\" }}\n",
                secret.display()
            ),
        );
        let fixture = Fixture::new(&config);
        assert!(fixture.identity(OLD).is_some());
        assert_eq!(fixture.reload(), Outcome::Unchanged);

        std::fs::File::create(&secret)
            .unwrap()
            .write_all(NEW.as_bytes())
            .unwrap();
        assert!(matches!(
            fixture.reload(),
            Outcome::Applied { generation: 1, .. }
        ));
        assert!(fixture.identity(OLD).is_none());
        assert!(fixture.identity(NEW).is_some());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_reload_makes_new_tenants_nameable_and_forgets_none() {
        let initial = text("", &key("a", "acme", OLD, ""));
        let fixture = Fixture::new(&initial);
        let tenants = fixture.reloader.handles.tenants.clone();
        let hash = |name: &str| crate::gateway::usage::UsageSetup::tenant_key(name);
        assert!(tenants.name(&hash("globex")).is_none());
        fixture.write(&text("", &key("b", "globex", NEW, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        assert_eq!(tenants.name(&hash("globex")).as_deref(), Some("globex"));
        // The tenant the reload removed is still named, for attempts in flight.
        assert_eq!(tenants.name(&hash("acme")).as_deref(), Some("acme"));
    }

    #[test]
    fn a_reload_makes_new_keys_nameable_and_forgets_none() {
        let initial = text("", &key("a", "acme", OLD, ""));
        let fixture = Fixture::new(&initial);
        let names = fixture.reloader.handles.key_names.clone();
        let (old_scope, new_scope) = (digest(OLD), digest(NEW));
        assert_eq!(names.name(&old_scope).as_deref(), Some("a"));
        assert!(names.name(&new_scope).is_none());
        fixture.write(&text("", &key("b", "globex", NEW, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        // The new key is named before a request under it can be logged, and the
        // key the reload revoked is still named, for attempts already in flight.
        assert_eq!(names.name(&new_scope).as_deref(), Some("b"));
        assert_eq!(names.name(&old_scope).as_deref(), Some("a"));
    }

    #[test]
    fn a_rename_alone_updates_the_names_the_logs_use() {
        // Same token, same tenant, new key name: nothing about who may call has
        // changed, so the reload is a no-op for authentication, but the logs must
        // say the new name from now on (0012: "rename a key freely").
        let initial = text("", &key("a", "acme", OLD, ""));
        let fixture = Fixture::new(&initial);
        let names = fixture.reloader.handles.key_names.clone();
        let scope = digest(OLD);
        assert_eq!(names.name(&scope).as_deref(), Some("a"));
        fixture.write(&text("", &key("renamed", "acme", OLD, "")));
        assert!(matches!(fixture.reload(), Outcome::Unchanged));
        assert_eq!(names.name(&scope).as_deref(), Some("renamed"));
    }

    #[test]
    fn differences_are_reported_by_path_and_never_by_value() {
        let old: Value = serde_json::json!({
            "server": {"port": 1, "host": "a"},
            "limits": {"rpm": 1},
            "providers": {"x": {"kind": "openai"}},
        });
        let new: Value = serde_json::json!({
            "server": {"port": 2, "host": "a"},
            "providers": {"x": {"kind": "openai"}, "y": {"kind": "xai"}},
            "fleet": {"redis_url": {"env": "R"}},
        });
        let mut paths = Vec::new();
        changed_paths(&old, &new, "", &mut paths);
        assert_eq!(paths, ["fleet", "limits", "providers.y", "server.port"]);
    }

    // ---- providers and routes (0022, slice 3) ----

    fn with_providers(providers: &str, routes: &str, keys: &str) -> String {
        format!("version = 1\n{providers}\n{routes}\n{keys}")
    }

    fn compat(name: &str, url: &str, key_file: &std::path::Path) -> String {
        format!(
            "[providers.{name}]\nkind = \"openai-compatible\"\nbase_url = \"{url}\"\n\
             api_key = {{ file = \"{}\" }}\n",
            key_file.display()
        )
    }

    fn chat() -> serde_json::Value {
        serde_json::json!({"model": "a/m", "messages": [{"role": "user", "content": "hi"}]})
    }

    fn completion_body() -> String {
        serde_json::json!({"id": "c", "created": 1, "model": "m",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
        .to_string()
    }

    #[tokio::test]
    async fn a_rotated_file_key_is_used_by_the_next_request_and_not_by_one_in_flight() {
        let mut upstream = mockito::Server::new_async().await;
        let old = upstream
            .mock("POST", "/chat/completions")
            .match_header("authorization", "Bearer key-one")
            .with_header("content-type", "application/json")
            .with_body(completion_body())
            .expect(1)
            .create_async()
            .await;
        let new = upstream
            .mock("POST", "/chat/completions")
            .match_header("authorization", "Bearer key-two")
            .with_header("content-type", "application/json")
            .with_body(completion_body())
            .expect(1)
            .create_async()
            .await;
        let directory =
            std::env::temp_dir().join(format!("llmshim-rotate-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).unwrap();
        let secret = directory.join("upstream.key");
        std::fs::write(&secret, "key-one\n").unwrap();
        let initial = with_providers(
            &compat("a", &upstream.url(), &secret),
            "",
            &key("k", "acme", OLD, ""),
        );
        let fixture = Fixture::new(&initial);
        // The configuration is byte-identical; only the secret file changed.
        let in_flight = fixture.router.current();
        std::fs::write(&secret, "key-two\n").unwrap();
        let Outcome::Applied { .. } = fixture.reload() else {
            panic!("a rotated secret file is a change");
        };
        let status = fixture.reloader.status();
        assert_eq!(status.router_generation, 1);
        let last = status.last_reload.unwrap();
        assert_eq!(last.changes, ["providers.changed: a"]);
        // Neither the status nor any report carries a secret.
        let shown = format!("{last:?}");
        assert!(
            !shown.contains("key-one") && !shown.contains("key-two"),
            "{shown}"
        );
        // The request admitted before the swap finishes on the old key.
        crate::completion(&in_flight, &chat()).await.unwrap();
        // The next request uses the new one.
        crate::completion(&fixture.router.current(), &chat())
            .await
            .unwrap();
        old.assert_async().await;
        new.assert_async().await;
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn providers_are_added_and_removed_and_routes_change_as_one_router() {
        let directory =
            std::env::temp_dir().join(format!("llmshim-add-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).unwrap();
        let secret = directory.join("k");
        std::fs::write(&secret, "secret-value").unwrap();
        let a = compat("a", "http://127.0.0.1:1/v1", &secret);
        let b = compat("b", "http://127.0.0.1:2/v1", &secret);
        let fixture = Fixture::new(&with_providers(&a, "", &key("k", "acme", OLD, "")));
        assert_eq!(fixture.router.current().provider_keys(), ["a"]);
        fixture.write(&with_providers(
            &format!("{a}{b}"),
            "[routes.fast]\nmodel = \"b/m\"\n",
            &key("k", "acme", OLD, ""),
        ));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        let router = fixture.router.current();
        let mut keys = router.provider_keys();
        keys.sort();
        assert_eq!(keys, ["a", "b"]);
        let last = fixture.reloader.status().last_reload.unwrap();
        assert_eq!(last.changes, ["providers.added: b", "routes"]);
        fixture.write(&with_providers(&b, "", &key("k", "acme", OLD, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        assert_eq!(fixture.router.current().provider_keys(), ["b"]);
        let last = fixture.reloader.status().last_reload.unwrap();
        assert_eq!(last.changes, ["providers.removed: a", "routes"]);
        assert_eq!(fixture.reloader.status().router_generation, 2);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn a_provider_that_cannot_be_built_rejects_the_whole_reload_and_echoes_no_secret() {
        let directory =
            std::env::temp_dir().join(format!("llmshim-bad-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).unwrap();
        let secret = directory.join("k");
        std::fs::write(&secret, "secret-value").unwrap();
        let a = compat("a", "http://127.0.0.1:1/v1", &secret);
        let fixture = Fixture::new(&with_providers(&a, "", &key("k", "acme", OLD, "")));
        let before = Arc::as_ptr(&fixture.router.current());
        // Adds a provider whose secret file does not exist, and swaps the caller key
        // in the same edit: neither applies.
        let missing = directory.join("missing");
        let b = compat("b", "http://127.0.0.1:2/v1", &missing);
        fixture.write(&with_providers(
            &format!("{a}{b}"),
            "",
            &key("j", "globex", NEW, ""),
        ));
        let Outcome::Rejected { issues, .. } = fixture.reload() else {
            panic!("an unbuildable provider rejects the reload");
        };
        assert!(issues
            .iter()
            .any(|issue| issue.path == "providers.b.api_key"));
        assert!(!format!("{issues:?}").contains("secret-value"));
        assert_eq!(Arc::as_ptr(&fixture.router.current()), before);
        assert!(fixture.identity(NEW).is_none());
        assert!(fixture.identity(OLD).is_some());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn env_form_provider_secrets_are_flagged_in_the_status() {
        let fixture = Fixture::new(&text("", &key("a", "acme", OLD, "")));
        // `text` configures the one provider with { env = ... }: a reload cannot
        // rotate it, so the status says so.
        assert_eq!(fixture.reloader.status().env_secret_providers, ["openai"]);
    }

    #[test]
    fn a_fleet_replica_follows_a_provider_change_with_its_workers() {
        let directory =
            std::env::temp_dir().join(format!("llmshim-fleet-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).unwrap();
        let secret = directory.join("k");
        std::fs::write(&secret, "one").unwrap();
        let a = compat("a", "http://127.0.0.1:1/v1", &secret);
        let b = compat("b", "http://127.0.0.1:2/v1", &secret);
        let fixture =
            Fixture::with_fleet(&with_providers(&a, "", &key("k", "acme", OLD, "")), true);
        let synced = || fixture.synced.load(std::sync::atomic::Ordering::SeqCst);
        // A keys-only edit changes no provider: the workers are left alone.
        fixture.write(&with_providers(&a, "", &key("k", "acme", NEW, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        assert_eq!(synced(), 0);
        // An added provider asks the workers to follow, after the router changed.
        fixture.write(&with_providers(
            &format!("{a}{b}"),
            "",
            &key("k", "acme", NEW, ""),
        ));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        assert_eq!(synced(), 1);
        assert_eq!(fixture.router.current().provider_keys().len(), 2);
        // A rotated secret and a revocation in one edit both apply there too.
        std::fs::write(&secret, "two").unwrap();
        fixture.write(&with_providers(
            &format!("{a}{b}"),
            "",
            &key("j", "globex", OLD, ""),
        ));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        assert_eq!(synced(), 2);
        assert!(fixture.identity(NEW).is_none());
        assert!(fixture.identity(OLD).is_some());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(feature = "serve-cloud")]
    #[test]
    fn a_service_account_file_that_is_missing_or_empty_rejects_the_reload() {
        let directory =
            std::env::temp_dir().join(format!("llmshim-sa-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).unwrap();
        let secret = directory.join("k");
        std::fs::write(&secret, "one").unwrap();
        let a = compat("a", "http://127.0.0.1:1/v1", &secret);
        let fixture = Fixture::new(&with_providers(&a, "", &key("k", "acme", OLD, "")));
        let vertex = |file: &std::path::Path| {
            format!(
                "[providers.vertex-gemini]\nkind = \"vertex-gemini\"\nproject = \"p\"\nregion = \"us-central1\"\n\
                 binding = \"b\"\nservice_account_file = \"{}\"\n",
                file.display()
            )
        };
        let account = directory.join("sa.json");
        for contents in [None, Some("")] {
            if let Some(contents) = contents {
                std::fs::write(&account, contents).unwrap();
            }
            fixture.write(&with_providers(
                &format!("{a}{}", vertex(&account)),
                "",
                &key("k", "acme", OLD, ""),
            ));
            let outcome = fixture.reload();
            let Outcome::Rejected { issues, .. } = outcome else {
                // Without the feature the file is refused whole, which is also a
                // rejection; anything applied is the defect.
                panic!("a missing or empty service-account file must reject");
            };
            assert!(
                issues
                    .iter()
                    .any(|issue| issue.path.ends_with(".service_account_file")),
                "{issues:?}"
            );
        }
        assert_eq!(fixture.reloader.status().router_generation, 0);
        let _ = std::fs::remove_dir_all(directory);
    }

    // ---- rate limits (0022, slice 4) ----

    fn key_for(provider: &str) -> crate::proxy::ratelimit::RateKey {
        crate::proxy::ratelimit::RateKey {
            provider: provider.to_string(),
            tenant: None,
        }
    }

    async fn drain(
        limiter: &crate::proxy::ratelimit::InMemoryRateLimiter,
        provider: &str,
    ) -> usize {
        let mut admitted = 0;
        while crate::proxy::ratelimit::RateLimiter::acquire(limiter, &key_for(provider), 1)
            .await
            .is_ok()
        {
            admitted += 1;
            assert!(admitted < 10_000, "the limiter never refused");
        }
        admitted
    }

    #[tokio::test]
    async fn a_lowered_limit_applies_to_the_next_request_and_keeps_the_tokens() {
        let fixture = Fixture::new(&text("[limits]\nrpm = 100", &key("k", "acme", OLD, "")));
        // Spend 60 of the 100 tokens.
        for _ in 0..60 {
            crate::proxy::ratelimit::RateLimiter::acquire(&*fixture.limiter, &key_for("openai"), 1)
                .await
                .unwrap();
        }
        // Lower to 30: the 40 tokens left are clamped to the new capacity.
        fixture.write(&text("[limits]\nrpm = 30", &key("k", "acme", OLD, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        assert_eq!(fixture.reloader.status().limits_generation, 1);
        assert_eq!(fixture.reloader.status().router_generation, 0);
        assert_eq!(
            fixture.reloader.status().last_reload.unwrap().changes,
            ["limits"]
        );
        assert_eq!(drain(&fixture.limiter, "openai").await, 30);
    }

    #[tokio::test]
    async fn a_reload_is_not_a_limit_reset_for_a_provider_whose_limit_did_not_change() {
        let fixture = Fixture::new(&text("[limits]\nrpm = 100", &key("k", "acme", OLD, "")));
        for _ in 0..90 {
            crate::proxy::ratelimit::RateLimiter::acquire(&*fixture.limiter, &key_for("openai"), 1)
                .await
                .unwrap();
        }
        // Another change entirely (a caller key); the same limit is presented again.
        fixture.write(&text("[limits]\nrpm = 100", &key("j", "globex", NEW, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        assert_eq!(fixture.reloader.status().limits_generation, 0);
        assert!(drain(&fixture.limiter, "openai").await <= 11);
    }

    #[tokio::test]
    async fn a_removed_limit_removes_the_bucket_and_a_raised_one_refills_from_what_is_left() {
        let fixture = Fixture::new(&text("[limits]\nrpm = 10", &key("k", "acme", OLD, "")));
        assert_eq!(drain(&fixture.limiter, "openai").await, 10);
        fixture.write(&text("", &key("k", "acme", OLD, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        // No limit: nothing is refused.
        assert!(crate::proxy::ratelimit::RateLimiter::acquire(
            &*fixture.limiter,
            &key_for("openai"),
            1
        )
        .await
        .is_ok());
        // The limit comes back, higher: a fresh bucket, not the drained one.
        fixture.write(&text("[limits]\nrpm = 20", &key("k", "acme", OLD, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        assert_eq!(drain(&fixture.limiter, "openai").await, 20);
        assert_eq!(fixture.reloader.status().limits_generation, 2);
    }

    #[test]
    fn a_limits_change_applies_on_a_fleet_replica_and_the_other_limits_stay_refused() {
        let fixture = Fixture::with_fleet(
            &text("[limits]\nrpm = 100", &key("k", "acme", OLD, "")),
            true,
        );
        fixture.write(&text("[limits]\nrpm = 50", &key("k", "acme", OLD, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        fixture.write(&text(
            "[limits]\nrpm = 50\nqueue_timeout_ms = 10",
            &key("k", "acme", OLD, ""),
        ));
        let Outcome::Rejected { issues, .. } = fixture.reload() else {
            panic!("the queue timeout is fixed at start");
        };
        assert_eq!(issues[0].path, "limits");
    }

    #[test]
    fn a_keys_only_edit_does_not_replace_the_router() {
        let fixture = Fixture::new(&text("", &key("a", "acme", OLD, "")));
        let before = Arc::as_ptr(&fixture.router.current());
        fixture.write(&text("", &key("a", "acme", NEW, "")));
        assert!(matches!(fixture.reload(), Outcome::Applied { .. }));
        assert_eq!(Arc::as_ptr(&fixture.router.current()), before);
        assert_eq!(fixture.reloader.status().router_generation, 0);
    }

    #[test]
    fn guardrails_are_a_fixed_section_so_a_reload_that_changes_them_is_refused() {
        // Guardrails are compiled once at start (note 0040): they are not among the sections a
        // reload replaces, so a file that differs only in them is a change outside the keys,
        // providers, routes and limits, which a reload refuses by path.
        let base = "version = 1\n[providers.p]\nkind = \"openai-compatible\"\nbase_url = \"https://x.example.com/v1\"\n";
        let with = format!("{base}[[guardrails]]\nname = \"r\"\nblock_if_matches = [\"x\"]\n");
        assert_ne!(
            document_without_reloadable(base, Format::Toml).unwrap(),
            document_without_reloadable(&with, Format::Toml).unwrap()
        );
    }
}

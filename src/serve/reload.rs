//! Replacing the caller keys of a running server (0012).
//!
//! Only `[[keys]]` can change while the server runs. Providers, listeners, TLS
//! paths, fleet settings and every other table are fixed at start, because they
//! are held by objects that cannot be swapped safely. A reload is therefore
//! all-or-nothing:
//!
//! 1. the new file must pass everything `--check` applies (shape, references,
//!    caller-key rules);
//! 2. everything outside `[[keys]]` must be exactly what the server started with,
//!    compared as parsed documents, and each difference is refused **by path**;
//! 3. only then is the new key set swapped in, in one step, for every holder of
//!    the key store (the gateway, the access log and the client-certificate
//!    binding), and new tenant names are made resolvable for the usage log.
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
use crate::gateway::usage::TenantNames;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// The parts of a running server that a reload replaces. Created while the
/// application is assembled; the application and the [`Reloader`] share them.
#[derive(Clone)]
pub struct ReloadHandles {
    pub(crate) keys: LiveKeys,
    pub(crate) tenants: TenantNames,
    pub(crate) status: StatusHandle,
}

impl ReloadHandles {
    pub(crate) fn new(keys: LiveKeys, tenants: TenantNames) -> Self {
        Self {
            keys,
            tenants,
            status: StatusHandle::default(),
        }
    }

    /// What the admin listener reports.
    pub fn status(&self) -> ReloadStatus {
        self.status.snapshot()
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
    /// The latest applied or rejected reload. A no-change check is not recorded
    /// here.
    pub last_reload: Option<ReloadAttempt>,
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
}

struct Active {
    /// The configuration document without `[[keys]]`.
    document: Value,
    fingerprint: String,
    enforced: bool,
    /// A digest of the last rejection, to recognize a repeat.
    last_rejection: Option<String>,
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
        handles: ReloadHandles,
    ) -> Result<Self, Vec<Issue>> {
        let document = document_without_keys(text, format).map_err(|message| {
            vec![Issue {
                path: "(file)".into(),
                message,
            }]
        })?;
        let current = handles.keys.current();
        let active = Active {
            document,
            fingerprint: current.fingerprint(),
            enforced: current.is_enforced(),
            last_rejection: None,
        };
        let credentials = current.credential_count();
        let config_sha256 = sha256_hex(text.as_bytes());
        handles.status.update(|status| {
            *status = ReloadStatus {
                generation: 0,
                config_sha256,
                credentials,
                loaded_at_epoch_secs: epoch_secs(),
                last_check_epoch_secs: None,
                last_reload: None,
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

    /// Read the file again and, if all of it is acceptable, replace the keys.
    /// Blocking: it reads files, so call it from a blocking task.
    pub fn reload(&self, env: &dyn Fn(&str) -> Option<String>) -> Outcome {
        // One reload at a time; a second caller waits and then sees the first's
        // result as its starting point.
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        let now = epoch_secs();
        let step = self.attempt(&mut active, env);
        self.handles.status.update(|status| {
            status.last_check_epoch_secs = Some(now);
            match &step {
                Step::Applied {
                    generation,
                    credentials,
                    config_sha256,
                } => {
                    status.generation = *generation;
                    status.credentials = *credentials;
                    status.config_sha256.clone_from(config_sha256);
                    status.last_reload = Some(ReloadAttempt {
                        at_epoch_secs: now,
                        outcome: "applied",
                        issues: Vec::new(),
                    });
                }
                Step::Unchanged { config_sha256 } => {
                    status.config_sha256.clone_from(config_sha256);
                }
                Step::Rejected { issues, repeated } => {
                    // A repeat keeps the time it first failed.
                    if !repeated {
                        status.last_reload = Some(ReloadAttempt {
                            at_epoch_secs: now,
                            outcome: "rejected",
                            issues: issues.iter().map(IssueView::from).collect(),
                        });
                    }
                }
            }
        });
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
            Step::Rejected { issues, repeated } => Outcome::Rejected { issues, repeated },
        }
    }

    fn attempt(&self, active: &mut Active, env: &dyn Fn(&str) -> Option<String>) -> Step {
        let loaded = match ServeConfig::load_with_text(&self.path) {
            Ok(loaded) => loaded,
            Err(issues) => return reject(active, "", issues),
        };
        let config_sha256 = sha256_hex(loaded.text.as_bytes());
        let reject_now = |active: &mut Active, issues| reject(active, &config_sha256, issues);

        // Everything outside the keys must be what the server started with.
        let document = match document_without_keys(&loaded.text, loaded.format) {
            Ok(document) => document,
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
        issues.extend(loaded.config.check_references(env));
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

        active.last_rejection = None;
        let fingerprint = store.fingerprint();
        if fingerprint == active.fingerprint {
            return Step::Unchanged { config_sha256 };
        }
        let credentials = store.credential_count();
        // Names first: a request admitted under the new keys must find its
        // tenant's name already resolvable.
        self.handles
            .tenants
            .add(loaded.config.keys.iter().map(|key| key.tenant.clone()));
        self.handles.keys.replace(store);
        active.fingerprint = fingerprint;
        let generation = self.handles.status.snapshot().generation + 1;
        Step::Applied {
            generation,
            credentials,
            config_sha256,
        }
    }
}

enum Step {
    Applied {
        generation: u64,
        credentials: usize,
        config_sha256: String,
    },
    Unchanged {
        config_sha256: String,
    },
    Rejected {
        issues: Vec<Issue>,
        repeated: bool,
    },
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
    Step::Rejected { issues, repeated }
}

/// The file as a generic document with the `keys` array removed.
fn document_without_keys(text: &str, format: Format) -> Result<Value, String> {
    let mut document: Value = match format {
        Format::Toml => toml::from_str(text).map_err(|error| error.message().to_string())?,
        Format::Json => serde_json::from_str(text).map_err(|error| error.to_string())?,
    };
    if let Value::Object(table) = &mut document {
        table.remove("keys");
    }
    Ok(document)
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
        reloader: Reloader,
    }

    impl Fixture {
        fn new(initial: &str) -> Self {
            let directory = std::env::temp_dir()
                .join(format!("llmshim-reload-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&directory).unwrap();
            let file = directory.join("gateway.toml");
            std::fs::write(&file, initial).unwrap();
            let loaded = ServeConfig::load_with_text(&file).unwrap();
            let store = build_key_store(&loaded.config, &env).unwrap();
            let keys = LiveKeys::new(store);
            let tenants = TenantNames::new(loaded.config.keys.iter().map(|k| k.tenant.clone()));
            let reloader = Reloader::new(
                &file,
                &loaded.text,
                loaded.format,
                ReloadHandles::new(keys.clone(), tenants),
            )
            .unwrap();
            Self {
                directory,
                file,
                keys,
                reloader,
            }
        }

        fn write(&self, text: &str) {
            std::fs::write(&self.file, text).unwrap();
        }

        fn reload(&self) -> Outcome {
            self.reloader.reload(&env)
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
            "[server]\nport = 3001\n[limits]\nrpm = 10",
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
        fixture.write(&text("[limits]\nrpm = 1", &key("a", "acme", OLD, "")));
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
        fixture.write(&text("[limits]\nrpm = 2", &key("a", "acme", OLD, "")));
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
}

//! The versioned server configuration file read by `llmshim serve --config`.
//!
//! This file is a compatibility contract, separate from `~/.llmshim/config.toml`
//! (which `llmshim set` rewrites and whose types are semver-protected). Rules:
//!
//! - Unknown fields are errors at every level. A typo never becomes a silently
//!   ignored setting.
//! - Secrets are given by reference (`{ env = "NAME" }` or `{ file = "/path" }`),
//!   never inline, so the file itself can be committed and reviewed.
//! - Callers are authenticated unless the file says otherwise in so many words.
//! - Loading returns every semantic problem at once, each with the path of the
//!   setting that caused it. Nothing here exits the process or reads the
//!   environment implicitly.
//!
//! Design notes: `docs/design/enterprise-gateway/0001-server-config.md`.

use schemars::JsonSchema;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// The only schema version this build reads.
pub const VERSION: u32 = 1;

/// Largest spend cap the ledger can represent exactly (nano-USD in an f64-safe
/// integer), matching the gateway's own bound.
pub const MAX_BUDGET_USD: f64 = 9_007_199.254_740_992;

/// Longest spend window: 366 days. The Redis ledger multiplies the window by 1000
/// and sets an expiry from it, so an unbounded value is an expiry beyond Redis's
/// range (the script errors and admission is refused) or one no operator meant. A
/// budget that never resets is not a window and is not offered.
pub const MAX_BUDGET_WINDOW_SECS: u64 = 366 * 24 * 60 * 60;

/// Longest rate-bucket back-off after an upstream 429: one hour. Longer is a
/// mis-set unit; the provider's own `Retry-After` is honored separately.
pub const MAX_PENALTY_SECS: u64 = 3600;

/// Longest a request may wait for provider capacity: one hour. A wait that long is
/// a mis-set unit (seconds written as milliseconds, or the reverse), not a policy.
pub const MAX_QUEUE_TIMEOUT_MS: u64 = 60 * 60 * 1000;

/// One problem with a configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Issue {
    /// Dotted path of the offending setting, e.g. `providers.openai.api_key`.
    pub path: String,
    pub message: String,
}

impl Issue {
    pub(crate) fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: printable(&path.into()),
            message: printable(&message.into()),
        }
    }
}

/// Paths and messages can carry a name taken from the file. Escape control
/// characters so one cannot break a line, or forge a CI annotation, in the
/// output of `--check`.
fn printable(text: &str) -> String {
    if !text.chars().any(char::is_control) {
        return text.to_string();
    }
    text.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Text encodings a configuration file may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    Toml,
    Json,
}

/// The whole file.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ServeConfig {
    /// The schema version. Only 1 exists; another is refused before anything else.
    #[schemars(range(min = 1, max = 1))]
    pub version: u32,
    #[serde(default)]
    pub server: Server,
    #[serde(default, deserialize_with = "unique_map")]
    pub providers: BTreeMap<String, Provider>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub state: State,
    #[serde(default)]
    pub logging: Logging,
    /// Run as one replica of a fleet sharing a Redis instance. Without it the
    /// server is a single instance with in-process state.
    #[serde(default)]
    pub fleet: Option<Fleet>,
    /// A separate listener for operational endpoints. Without it they are
    /// served on the main listener, as the gateway does.
    #[serde(default)]
    pub admin: Option<Admin>,
    #[serde(default)]
    pub keys: Vec<GatewayKey>,
    #[serde(default, deserialize_with = "unique_map")]
    pub routes: BTreeMap<String, Route>,
    /// How a running server picks up changes to `keys`.
    #[serde(default)]
    pub reload: Reload,
    /// Export traces to an OpenTelemetry collector (0016). Needs a binary built
    /// with `--features serve-otel`; without it a file that sets this table is
    /// refused at startup.
    #[serde(default)]
    pub telemetry: Option<Telemetry>,
    /// How the gateway talks to providers (0021): retries, the connection pool,
    /// the proxy policy and every attempt deadline. Fixed at start. `serve` reads
    /// none of the `LLMSHIM_*` variables these replace.
    #[serde(default)]
    pub upstream: Upstream,
    /// Other ways a caller proves who it is (0017). Fixed at start: a change is
    /// refused by a reload, though a key's `jwt` binding reloads with the key.
    #[serde(default)]
    pub auth: Auth,
    /// Take the caller keys from a signed policy snapshot (0024) instead of
    /// `[[keys]]`. The two cannot be combined. Fixed at start, except that a
    /// snapshot file's content reloads like keys do.
    #[serde(default)]
    pub control_plane: Option<ControlPlane>,
}

/// A signed snapshot as the source of caller keys (0024). Needs `state.policy_dir`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ControlPlane {
    /// The organization whose snapshots this replica takes (the payload's `org`). A
    /// snapshot for another organization is refused even if a trusted key signed it.
    pub org: String,
    /// Public keys that may have signed a snapshot, each `ed25519:<base64 of the
    /// 32-byte key>` (`llmshim snapshot keygen` prints one). More than one lets a
    /// signing key rotate: add the new public key first.
    pub verify_keys: Vec<String>,
    /// The snapshot file (an absolute path), written by whatever distributes it
    /// and watched like the keys of a file.
    pub snapshot_file: PathBuf,
    /// A floor for the snapshot's generation, so a replica with no record of what it
    /// applied (a first boot) still refuses a snapshot older than this. Defaults to 0.
    #[serde(default)]
    pub min_generation: Option<u64>,
    /// Report the snapshot in force as `stale` once it is older than this many seconds,
    /// counted from when its generation was applied (a distributor that wants silence to
    /// count as fresh re-issues). An alert only, unless `refuse_when_stale`.
    #[serde(default)]
    #[schemars(range(min = 1, max = 31536000))]
    pub max_age_secs: Option<u64>,
    /// With `max_age_secs`: refuse new inference requests with 503 while the snapshot is
    /// stale. `/health`, `/ready` and the operational endpoints keep answering. Off by
    /// default: a control plane that is down must not take inference down (0024).
    #[serde(default)]
    pub refuse_when_stale: bool,
}

/// Caller authentication beyond bearer keys and client certificates.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Auth {
    /// Trusted token issuers. A `[[keys]]` entry binds to one by `jwt.table`.
    #[serde(default)]
    pub jwt: Vec<JwtIssuer>,
}

/// One trusted JWT issuer (0017): who signs workload tokens, where its keys are,
/// and what a token must say. Every identity still maps to a configured key.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct JwtIssuer {
    /// What a key's `jwt.table` names, and how the issuer is labelled in metrics
    /// and `/v1/serve/config`.
    pub name: String,
    /// The exact `iss` a token must carry.
    pub issuer: String,
    /// Where the issuer's JWKS is, `https://` (or `http://` to a loopback address
    /// literal). No credentials in the address, no redirects followed, no
    /// discovery: a token never chooses where its key comes from.
    pub jwks_url: String,
    /// A PEM bundle that is the only trust anchor for fetching the JWKS (a private
    /// issuer's CA). Without it the platform's roots apply.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    /// Extra request headers for the JWKS fetch (an authorization token, say), by
    /// secret reference.
    #[serde(default, deserialize_with = "unique_map")]
    pub headers: BTreeMap<String, SecretRef>,
    /// `aud` must contain this.
    pub audience: String,
    /// The signature algorithms accepted from this issuer: `RS256`, `ES256` or
    /// `EdDSA`. Never taken from the token.
    pub algorithms: Vec<String>,
    /// Tolerance for clock difference, in seconds. Defaults to 30, at most 300.
    #[serde(default)]
    pub clock_skew_secs: Option<u64>,
    /// How long fetched keys are fresh, in seconds. Defaults to 300, at least 30.
    #[serde(default)]
    pub jwks_refresh_secs: Option<u64>,
    /// How long keys already fetched keep verifying when a refresh fails, in
    /// seconds. Defaults to 3600; 0 means never. Together with the refresh
    /// interval this is how long a revoked key can verify while the issuer is
    /// unreachable.
    #[serde(default)]
    pub jwks_max_stale_secs: Option<u64>,
}

/// A key that authenticates by a verified workload token.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct JwtBinding {
    /// The `name` of an `[[auth.jwt]]` table (not its `issuer` URL).
    pub table: String,
    /// The exact `sub` claim.
    pub subject: String,
}

/// Where to send traces. Fixed at start: a change is refused by a reload.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Telemetry {
    /// The collector's OTLP/HTTP address, `http://` or `https://` (plain http is
    /// allowed: a collector on a private network or a sidecar often has no TLS).
    pub otlp_endpoint: String,
    /// The `service.name` resource attribute. Defaults to `llmshim`.
    #[serde(default)]
    pub service_name: Option<String>,
    /// The fraction of new traces to record, 0 to 1, applied parent-based so a
    /// caller's own decision is respected. Defaults to 1.
    #[serde(default)]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub sample_ratio: Option<f64>,
    /// Extra request headers for the collector (an authorization token, say), by
    /// secret reference: the value is never written in the file.
    #[serde(default, deserialize_with = "unique_map")]
    pub headers: BTreeMap<String, SecretRef>,
}

/// How the gateway talks to providers (0021). Every key is optional and defaults
/// to what the library's client uses without any `LLMSHIM_*` variable, except
/// the pool, which follows the concurrency served. The deadlines are in
/// milliseconds and may not be set so as to make another unreachable.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Upstream {
    /// Retries after the first attempt of a transport error or a 429/5xx
    /// (0 to 10). Defaults to 3.
    #[serde(default)]
    #[schemars(range(min = 0, max = 10))]
    pub max_retries: Option<u32>,
    /// The cap of one retry wait, in seconds (1 to 300). Defaults to 60.
    #[serde(default)]
    #[schemars(range(min = 1, max = 300))]
    pub max_backoff_secs: Option<u64>,
    /// Idle connections kept per upstream host (1 to 4096). Defaults to the larger
    /// of 256 and `limits.max_concurrency`. An HTTP/1.1 upstream needs one per
    /// request in flight to avoid a connection per request; an HTTP/2 upstream
    /// multiplexes. Several providers on one host share it.
    #[serde(default)]
    #[schemars(range(min = 1, max = 4096))]
    pub pool_max_idle_per_host: Option<u32>,
    /// How long an idle connection is kept, in seconds (1 to 3600). Defaults to 90.
    #[serde(default)]
    #[schemars(range(min = 1, max = 3600))]
    pub pool_idle_timeout_secs: Option<u64>,
    /// Honour `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`. Defaults to true. The cloud
    /// transports never do.
    #[serde(default)]
    pub ambient_proxy: Option<bool>,
    /// Defaults to 30000.
    #[serde(default)]
    #[schemars(range(min = 1i64, max = 31536000000i64))]
    pub connect_timeout_ms: Option<u64>,
    /// Defaults to 600000.
    #[serde(default)]
    #[schemars(range(min = 1i64, max = 31536000000i64))]
    pub response_header_timeout_ms: Option<u64>,
    /// Defaults to 10000.
    #[serde(default)]
    #[schemars(range(min = 1i64, max = 31536000000i64))]
    pub policy_callback_timeout_ms: Option<u64>,
    /// Defaults to 10000.
    #[serde(default)]
    #[schemars(range(min = 1i64, max = 31536000000i64))]
    pub error_body_idle_timeout_ms: Option<u64>,
    /// Defaults to 30000.
    #[serde(default)]
    #[schemars(range(min = 1i64, max = 31536000000i64))]
    pub error_body_total_timeout_ms: Option<u64>,
    /// Defaults to 300000.
    #[serde(default)]
    #[schemars(range(min = 1i64, max = 31536000000i64))]
    pub unary_idle_timeout_ms: Option<u64>,
    /// Defaults to 1800000.
    #[serde(default)]
    #[schemars(range(min = 1i64, max = 31536000000i64))]
    pub unary_attempt_timeout_ms: Option<u64>,
    /// Defaults to 300000.
    #[serde(default)]
    #[schemars(range(min = 1i64, max = 31536000000i64))]
    pub stream_idle_timeout_ms: Option<u64>,
    /// Defaults to 7200000.
    #[serde(default)]
    #[schemars(range(min = 1i64, max = 31536000000i64))]
    pub stream_attempt_timeout_ms: Option<u64>,
}

impl Upstream {
    /// The settings this table describes, over the library's defaults. Never reads
    /// the environment. `concurrency` is `limits.max_concurrency`, which the pool
    /// default follows.
    pub fn settings(
        &self,
        concurrency: Option<u32>,
    ) -> Result<crate::client::ClientSettings, &'static str> {
        use std::time::Duration;
        let mut settings = crate::client::ClientSettings::default();
        if let Some(n) = self.max_retries {
            settings.max_retries = n;
        }
        if let Some(secs) = self.max_backoff_secs {
            settings.max_backoff = Duration::from_secs(secs);
        }
        settings.pool_max_idle_per_host = self
            .pool_max_idle_per_host
            .map(|n| n as usize)
            .unwrap_or_else(|| (concurrency.unwrap_or(0) as usize).clamp(256, 4096));
        if let Some(secs) = self.pool_idle_timeout_secs {
            settings.pool_idle_timeout = Duration::from_secs(secs);
        }
        if let Some(ambient) = self.ambient_proxy {
            settings.ambient_proxy = ambient;
        }
        // The defaults are the library's own (`AttemptDeadlines::default`), so
        // an empty table cannot drift from them.
        let ms = |value: Option<u64>, default: Duration| {
            value.map(Duration::from_millis).unwrap_or(default)
        };
        let d = settings.deadlines;
        settings.deadlines = d
            .with_connect_timeout(ms(self.connect_timeout_ms, d.connect))?
            .with_response_header_timeout(ms(self.response_header_timeout_ms, d.response_headers))?
            .with_policy_callback_timeout(ms(self.policy_callback_timeout_ms, d.policy_callback))?
            .with_error_body_timeouts(
                ms(self.error_body_idle_timeout_ms, d.error_body_idle),
                ms(self.error_body_total_timeout_ms, d.error_body_total),
            )?
            .with_unary_timeouts(
                ms(self.unary_idle_timeout_ms, d.unary_body_idle),
                ms(self.unary_attempt_timeout_ms, d.unary_attempt_total),
            )?
            .with_stream_timeouts(
                ms(self.stream_idle_timeout_ms, d.stream_semantic_idle),
                ms(self.stream_attempt_timeout_ms, d.stream_attempt_total),
            )?;
        settings.validate()?;
        Ok(settings)
    }
}

/// Reloading `[[keys]]` without a restart.
///
/// Only the caller keys can change while the server runs; a change to anything
/// else in the file is refused by path and needs a restart. The whole new file
/// must pass the checks `--check` applies, or none of it is used. `SIGHUP`
/// always triggers a reload (on platforms that have it); this table controls
/// whether the file is also polled.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Reload {
    /// Look for a changed file (and changed secret files) on a timer. Defaults
    /// to true.
    #[serde(default = "default_true")]
    pub watch: bool,
    /// How often to look, in seconds. Defaults to 30.
    #[serde(default)]
    pub interval_secs: Option<u64>,
}

impl Default for Reload {
    fn default() -> Self {
        Self {
            watch: true,
            interval_secs: None,
        }
    }
}

impl Reload {
    pub const DEFAULT_INTERVAL_SECS: u64 = 30;

    /// The polling interval, or `None` when polling is off.
    pub fn poll_interval(&self) -> Option<std::time::Duration> {
        self.watch.then(|| {
            std::time::Duration::from_secs(
                self.interval_secs.unwrap_or(Self::DEFAULT_INTERVAL_SECS),
            )
        })
    }
}

fn default_true() -> bool {
    true
}

/// A configuration and the text it was parsed from.
pub struct Loaded {
    pub config: ServeConfig,
    pub text: String,
    pub format: Format,
    /// The verified snapshot the keys came from, when `[control_plane]` is set.
    pub snapshot: Option<super::snapshot::Snapshot>,
}

/// Read a table, refusing a repeated name. JSON parsers otherwise keep the
/// last duplicate silently; TOML already refuses one.
fn unique_map<'de, D, V>(deserializer: D) -> Result<BTreeMap<String, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct Reader<V>(std::marker::PhantomData<V>);
    impl<'de, V: Deserialize<'de>> serde::de::Visitor<'de> for Reader<V> {
        type Value = BTreeMap<String, V>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a table")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some(name) = map.next_key::<String>()? {
                if out.contains_key(&name) {
                    return Err(serde::de::Error::custom(format!("duplicate name `{name}`")));
                }
                let value = map.next_value()?;
                out.insert(name, value);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_map(Reader(std::marker::PhantomData))
}

/// Listener and authentication posture.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Server {
    /// IP address to bind. Defaults to loopback; a container sets `0.0.0.0`,
    /// which needs `[[keys]]` (serving without callers is loopback-only).
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Serve callers with no key. Off by default: a file with no `[[keys]]`
    /// is refused unless this is set, and then `host` must be a loopback
    /// address. Cannot be set together with `[[keys]]`.
    #[serde(default)]
    pub allow_unauthenticated: bool,
    /// Terminate TLS on the main listener. Without it the listener is plain
    /// HTTP and belongs behind a terminating proxy or service mesh.
    #[serde(default)]
    pub tls: Option<Tls>,
    /// Inbound connections held at once (1 to 65535). Defaults to 1024. Replaces
    /// `LLMSHIM_HTTP_MAX_CONNECTIONS`, which `serve` does not read.
    #[serde(default)]
    #[schemars(range(min = 1, max = 65535))]
    pub max_connections: Option<u32>,
    /// How long a connection has to deliver a request's headers (the TLS handshake
    /// included), in milliseconds (1 to 600000). Defaults to 15000. Replaces
    /// `LLMSHIM_HTTP_HEADER_TIMEOUT_MS`, which `serve` does not read.
    #[serde(default)]
    #[schemars(range(min = 1, max = 600000))]
    pub header_timeout_ms: Option<u64>,
    /// Networks whose connections are load balancers or proxies that set
    /// `X-Forwarded-For`. Only a connection from inside them has its client
    /// address read from that header (the right-most entry that is not itself
    /// one of them); from anywhere else the header is ignored. Needed only for a
    /// key's `allowed_cidrs`. Empty by default. A change needs a restart.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            allow_unauthenticated: false,
            tls: None,
            max_connections: None,
            header_timeout_ms: None,
            trusted_proxies: Vec::new(),
        }
    }
}

/// Server certificate and key, as PEM files. The files are watched: when either
/// changes (a certificate manager renewed it) new connections use the new
/// material without a restart. A replacement that does not load is ignored and
/// the previous certificate stays in service.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Tls {
    /// PEM certificate chain, leaf first.
    pub cert_file: PathBuf,
    /// PEM private key (PKCS#8, PKCS#1 or SEC1).
    pub key_file: PathBuf,
    /// How often to look for changed files, in seconds. Defaults to 30.
    #[serde(default)]
    pub reload_interval_secs: Option<u64>,
    /// PEM bundle of the certificate authorities whose client certificates are
    /// accepted. When set, **every** connection to this listener must present a
    /// certificate signed by one of them, or the handshake fails; a key with
    /// `client_san` then authenticates by that certificate alone. Main listener
    /// only. The file is watched with the certificate and key.
    #[serde(default)]
    pub client_ca_file: Option<PathBuf>,
}

impl Tls {
    pub const DEFAULT_RELOAD_INTERVAL_SECS: u64 = 30;
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    3000
}

/// Fleet mode. Requires a binary built with `--features serve-redis`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Fleet {
    /// The Redis URL (`redis://` or `rediss://`), by reference because it can
    /// carry a password.
    pub redis_url: SecretRef,
}

/// A second listener for operational endpoints: `/metrics`,
/// `/v1/gateway/stats`, `/health` and `/ready`. When it is configured the main
/// listener answers 404 for all four, so callers never reach them.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Admin {
    /// IP address to bind. Defaults to loopback.
    #[serde(default = "default_host")]
    pub host: String,
    pub port: u16,
    /// Bearer token required for `/metrics` and `/v1/gateway/stats`, by
    /// reference. `/health` and `/ready` stay open so orchestrators can probe.
    /// Set this or `allow_unauthenticated`; neither is an error.
    #[serde(default)]
    pub token: Option<SecretRef>,
    /// Terminate TLS on the admin listener as well.
    #[serde(default)]
    pub tls: Option<Tls>,
    /// Serve the operational endpoints without a token. Without this, an
    /// `[admin]` table must set `token`: like the main listener, the admin
    /// listener is never open by accident.
    #[serde(default)]
    pub allow_unauthenticated: bool,
}

/// Where the server keeps what it must remember between requests.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct State {
    /// Directory for replay receipts (signed reasoning and tool-call identity a
    /// client replays later). Defaults to the platform's local data directory.
    /// Mount a persistent volume here to keep receipts across restarts.
    #[serde(default)]
    pub receipts_dir: Option<PathBuf>,
    /// How long a completed idempotent request's answer is kept, in seconds.
    /// Defaults to 300.
    #[serde(default)]
    pub idempotency_ttl_secs: Option<u64>,
    /// How long a request waits for the receipt directory's lock, in
    /// milliseconds (1 to 30000). Defaults to 2000. Replaces the
    /// `LLMSHIM_REPLAY_RECEIPTS_LOCK_TIMEOUT_MS` variable, which `serve` does not
    /// read. Several replicas sharing one receipts volume contend for this lock.
    #[serde(default)]
    #[schemars(range(min = 1, max = 30000))]
    pub receipt_lock_timeout_ms: Option<u64>,
    /// Where a replica records the generation of the snapshot it last applied (an
    /// absolute path to a writable directory), so a restart cannot be rolled back
    /// to an older snapshot. Required with `[control_plane]` (0024).
    #[serde(default)]
    pub policy_dir: Option<PathBuf>,
}

/// What the server records about the requests it handles.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Logging {
    /// One JSON line per request on the main listener: when, request id, method,
    /// path, status, latency to the response headers, the caller's tenant and
    /// key name, and a short fingerprint of the credential. Never a token, a
    /// header or a body.
    /// `"stdout"`, or an absolute file path (created with owner-only access and
    /// appended to; rotate it with `copytruncate` or let the platform collect
    /// stdout). Off when absent.
    #[serde(default)]
    pub access_log: Option<String>,
    /// One JSON line per provider attempt when it ends: tenant, key name, the
    /// request id (the one in the access log), provider, model, outcome, tokens
    /// and USD cost. Per attempt, not per request: a retry or a fallback is a
    /// separate charge, and the attempts of a request share its id. For showback
    /// and chargeback; sum `cost_usd` per tenant or key, treating `null` as
    /// unknown. Same targets as `access_log`.
    #[serde(default)]
    pub usage_log: Option<String>,
}

/// Which adapter a provider entry uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ProviderKind {
    Openai,
    Anthropic,
    Gemini,
    Xai,
    Cerebras,
    Openrouter,
    /// Any OpenAI Chat Completions-compatible server at `base_url`.
    OpenaiCompatible,
    /// Amazon Bedrock Converse, through an AWS SDK profile (0020). Needs a binary
    /// built with `--features serve-cloud`.
    Bedrock,
    /// Gemini on Vertex AI, through a service-account file (0020). Needs
    /// `--features serve-cloud`.
    VertexGemini,
    /// Anthropic models on Vertex AI, through a service-account file (0020).
    /// Needs `--features serve-cloud`.
    VertexAnthropic,
    /// Azure OpenAI / Foundry v1, through a key and named deployments (0020).
    /// Needs `--features serve-cloud`.
    Azure,
}

/// The names of the hosted providers. A hosted provider is registered under its
/// own name, because the catalog, rate limits and prices are looked up by it.
const HOSTED_NAMES: [&str; 10] = [
    "openai",
    "anthropic",
    "gemini",
    "xai",
    "cerebras",
    "openrouter",
    "bedrock",
    "vertex-gemini",
    "vertex-anthropic",
    "azure",
];

impl ProviderKind {
    /// The only name a hosted provider may have; `None` for a self-hosted server.
    pub fn hosted_name(self) -> Option<&'static str> {
        Some(match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::Xai => "xai",
            Self::Cerebras => "cerebras",
            Self::Openrouter => "openrouter",
            Self::Bedrock => "bedrock",
            Self::VertexGemini => "vertex-gemini",
            Self::VertexAnthropic => "vertex-anthropic",
            Self::Azure => "azure",
            Self::OpenaiCompatible => return None,
        })
    }

    /// One of the cloud transports (0020), which a `serve` binary has only when
    /// built with `--features serve-cloud`.
    pub fn is_cloud(self) -> bool {
        matches!(
            self,
            Self::Bedrock | Self::VertexGemini | Self::VertexAnthropic | Self::Azure
        )
    }

    fn kind_name(self) -> &'static str {
        match self {
            Self::OpenaiCompatible => "openai-compatible",
            other => other.hosted_name().unwrap_or("provider"),
        }
    }

    fn requires_base_url(self) -> bool {
        matches!(self, Self::OpenaiCompatible)
    }
    fn requires_api_key(self) -> bool {
        !matches!(
            self,
            Self::OpenaiCompatible | Self::Bedrock | Self::VertexGemini | Self::VertexAnthropic
        )
    }
}

/// One upstream. Its table name is the routing prefix (`<name>/<model>`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Provider {
    pub kind: ProviderKind,
    #[serde(default)]
    pub api_key: Option<SecretRef>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Bedrock: the AWS SDK profile to use. The profile file is trusted operator
    /// input (0020): it may select a role, SSO, a credential source or a
    /// `credential_process`.
    #[serde(default)]
    pub profile: Option<String>,
    /// Bedrock and Vertex: the cloud region (Vertex also takes `global`, `us`,
    /// `eu`).
    #[serde(default)]
    pub region: Option<String>,
    /// Bedrock: the AWS config file, with `credentials_file` (both or neither),
    /// replacing the SDK's default locations. Absolute paths.
    #[serde(default)]
    pub config_file: Option<PathBuf>,
    /// Bedrock: see `config_file`.
    #[serde(default)]
    pub credentials_file: Option<PathBuf>,
    /// Vertex: the Google Cloud project.
    #[serde(default)]
    pub project: Option<String>,
    /// Vertex and Azure: a stable, non-secret name for this identity. It is part
    /// of the replay boundary: change it when the identity changes.
    #[serde(default)]
    pub binding: Option<String>,
    /// Vertex: the service-account JSON file (an absolute path). Never
    /// application-default credentials.
    #[serde(default)]
    pub service_account_file: Option<PathBuf>,
    /// Azure: the resource endpoint, `https://<resource>.openai.azure.com`.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Azure: deployment name to the catalog model it serves. At least one.
    #[serde(default, deserialize_with = "unique_map")]
    pub deployments: BTreeMap<String, String>,
}

/// Where a secret lives. Exactly one field is set; the value never appears in
/// the file, in `Debug` output, or in an [`Issue`].
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SecretRef {
    pub env: Option<String>,
    pub file: Option<PathBuf>,
}

/// A hand-written reader so that a secret pasted inline is refused with a
/// fixed message. serde's default "invalid type" error would quote the value.
impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            #[serde(default)]
            env: Option<String>,
            #[serde(default)]
            file: Option<PathBuf>,
        }
        const BY_REFERENCE: &str =
            "secrets are given by reference: { env = \"NAME\" } or { file = \"/path\" }";
        struct Reader;
        impl<'de> serde::de::Visitor<'de> for Reader {
            type Value = SecretRef;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a secret reference table")
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<SecretRef, E> {
                Err(E::custom(BY_REFERENCE))
            }
            fn visit_bytes<E: serde::de::Error>(self, _: &[u8]) -> Result<SecretRef, E> {
                Err(E::custom(BY_REFERENCE))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<SecretRef, A::Error> {
                let fields =
                    Fields::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(SecretRef {
                    env: fields.env,
                    file: fields.file,
                })
            }
        }
        deserializer.deserialize_any(Reader)
    }
}

/// The schema of the reference, written by hand because the reader above is: a
/// derived schema would describe the two optional fields, not "exactly one".
impl JsonSchema for SecretRef {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SecretRef".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "A secret by reference, never inline: exactly one of `env` (the name of an environment variable) or `file` (the path of a file holding the value).",
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "env": {"type": "string", "pattern": "^[A-Z0-9_]+$"},
                        "file": {"type": "null"}
                    },
                    "required": ["env"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "file": {"type": "string"},
                        "env": {"type": "null"}
                    },
                    "required": ["file"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

/// The configuration contract as a JSON Schema (draft 2020-12), derived from the
/// types the parser reads so the two cannot drift. `llmshim serve --print-schema`
/// prints it; a copy is checked in and compared by a test, so a change to the
/// contract is a visible diff.
///
/// The schema describes shape: field names, which are required, types and each
/// type's own range (a port is 0 to 65535), closed objects, the one version.
/// The rules that compare values, read the environment or need two fields
/// together (a caller token must differ from an upstream key; a path must be
/// absolute; the admin port must differ from the main one; a spend window is at
/// most 366 days) are `serve --check`'s, not a schema's: a file the schema
/// accepts can still fail `--check`, and one it refuses always does.
pub fn schema() -> serde_json::Value {
    // `RestrictFormats` drops the non-standard `format` values schemars adds for
    // Rust's integer and float types (`uint32`, `double`), which a strict
    // consumer validator would reject; the numeric `minimum`/`maximum` stay.
    let generator = schemars::generate::SchemaSettings::draft2020_12()
        .with_transform(schemars::transform::RestrictFormats::default())
        .into_generator();
    serde_json::to_value(generator.into_root_schema_for::<ServeConfig>()).expect("a schema is JSON")
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.env, &self.file) {
            (Some(name), _) => write!(f, "SecretRef(env {name})"),
            (None, Some(path)) => write!(f, "SecretRef(file {})", path.display()),
            (None, None) => f.write_str("SecretRef(unset)"),
        }
    }
}

/// Provider-side admission limits.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Limits {
    /// In-flight upstream requests per provider, per instance.
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    /// Longest a request may wait for capacity before a 503.
    #[serde(default)]
    pub queue_timeout_ms: Option<u64>,
    /// How long a provider's rate bucket backs off after an upstream 429 that
    /// names no `Retry-After`, in seconds (1 to 3600). Defaults to 5. Replaces
    /// the `LLMSHIM_PENALTY_SECS` variable, which `serve` does not read.
    #[serde(default)]
    #[schemars(range(min = 1, max = 3600))]
    pub penalty_secs: Option<u64>,
    /// Requests per minute for every provider without its own entry.
    #[serde(default)]
    pub rpm: Option<u32>,
    /// Tokens per minute for every provider without its own entry.
    #[serde(default)]
    pub tpm: Option<u32>,
    #[serde(default, deserialize_with = "unique_map")]
    pub providers: BTreeMap<String, ProviderLimits>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ProviderLimits {
    #[serde(default)]
    pub rpm: Option<u32>,
    #[serde(default)]
    pub tpm: Option<u32>,
}

/// A caller credential and the identity it carries.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct GatewayKey {
    /// Operator-facing label. It names the key in the access log and the usage
    /// records (`key`); it is never sent to a provider.
    pub name: String,
    /// The bearer token, by reference. Mutually exclusive with `sha256`.
    #[serde(default)]
    pub secret: Option<SecretRef>,
    /// Lower-case hex SHA-256 of the bearer token, so the deployment holds no
    /// caller secret at all. Mutually exclusive with `secret`.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Authenticate by client certificate instead of a bearer token: the
    /// subject alternative name to match, written `dns:<name>` or
    /// `uri:<uri>` (for example `uri:spiffe://corp/payments/checkout`). Needs
    /// `server.tls.client_ca_file`. A certificate is renewed far more often than
    /// a token, so this names the workload, not one certificate. A request that
    /// also sends a bearer token is authenticated by the token alone.
    #[serde(default)]
    pub client_san: Option<String>,
    /// Authenticate by a workload token from an `[[auth.jwt]]` issuer instead:
    /// the issuer's table and the token's exact `sub` (0017). Only one of
    /// `secret`, `sha256`, `client_san` and `jwt` is set. A request that sends a
    /// token-shaped bearer value is authenticated as a token, never as a key.
    #[serde(default)]
    pub jwt: Option<JwtBinding>,
    /// Networks this key may be used from, as CIDRs (`10.0.0.0/8`, `2001:db8::/32`,
    /// a bare address for one host). A request authenticated as this key from
    /// anywhere else is refused with 403 before it reaches the queue or a
    /// budget. Applies to every kind of credential. Behind a proxy, list the
    /// proxy in `server.trusted_proxies` so the caller's address is known. Omit
    /// it for no restriction; an empty list is refused.
    #[serde(default)]
    pub allowed_cidrs: Option<Vec<String>>,
    /// When this key stops working, as an RFC 3339 time (`2026-12-31T00:00:00Z`). A request
    /// authenticated as it at or after that moment is refused with 401, checked at request
    /// time against this process's clock: no reload is needed for it to take effect (0024).
    #[serde(default)]
    pub expires_at: Option<String>,
    pub tenant: String,
    /// Queue priority; higher is served sooner.
    #[serde(default)]
    pub tier: u8,
    /// Per-tenant requests per minute. Zero denies every request.
    #[serde(default)]
    pub rpm: Option<u32>,
    #[serde(default)]
    pub tpm: Option<u32>,
    /// USD spend cap per window.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub budget_window_secs: Option<u64>,
    /// Admit requests whose cost cannot be bounded before sending.
    #[serde(default)]
    pub budget_allow_unpriced: bool,
}

/// A named route: `route/<name>` resolves to `model` with default settings.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Route {
    pub model: String,
    /// Request settings applied when the request does not set them itself.
    #[serde(default, deserialize_with = "unique_map")]
    pub settings: BTreeMap<String, serde_json::Value>,
}

impl ServeConfig {
    /// Parse and validate. Returns every semantic issue found; a syntax or
    /// shape error is reported alone, because nothing after it can be trusted.
    pub fn parse(text: &str, format: Format) -> Result<Self, Vec<Issue>> {
        // A different version may mean different fields, so it is judged before
        // the strict parse would reject a field of the newer schema by name.
        if let Some(issue) = unsupported_version(text, format) {
            return Err(vec![issue]);
        }
        let config: Self = match format {
            Format::Toml => toml::from_str(text).map_err(|error| {
                let at = error.span().map(|span| position(text, span.start));
                vec![Issue::new("(file)", describe(error.message(), at))]
            })?,
            Format::Json => serde_json::from_str(text).map_err(|error| {
                let at = (error.line() > 0).then(|| (error.line(), error.column()));
                // serde_json appends " at line L column C" to its text.
                let full = error.to_string();
                let message = full
                    .rsplit_once(" at line ")
                    .map_or(&*full, |(head, _)| head);
                vec![Issue::new("(file)", describe(message, at))]
            })?,
        };
        let issues = config.validate();
        if issues.is_empty() {
            Ok(config)
        } else {
            Err(issues)
        }
    }

    /// Read `path`, choosing the format from its extension.
    pub fn load(path: &Path) -> Result<Self, Vec<Issue>> {
        Self::load_with_text(path).map(|loaded| loaded.config)
    }

    /// As [`Self::load`], also returning the text that was parsed and its
    /// format, so a caller that compares files later compares what it started
    /// from rather than a second read.
    pub fn load_with_text(path: &Path) -> Result<Loaded, Vec<Issue>> {
        Self::load_with_text_resolving(path, true)
    }

    /// [`Self::load_with_text`], optionally leaving a `[control_plane]` snapshot
    /// unread: `serve --check --structure-only` reads nothing but the file.
    pub fn load_with_text_resolving(
        path: &Path,
        resolve_snapshot: bool,
    ) -> Result<Loaded, Vec<Issue>> {
        let format = match path.extension().and_then(|e| e.to_str()) {
            Some("toml") => Format::Toml,
            Some("json") => Format::Json,
            _ => {
                return Err(vec![Issue::new(
                    "(file)",
                    "configuration file must end in .toml or .json",
                )])
            }
        };
        let text = std::fs::read_to_string(path).map_err(|error| {
            vec![Issue::new(
                "(file)",
                format!("cannot read {}: {}", path.display(), error.kind()),
            )]
        })?;
        let mut config = Self::parse(&text, format)?;
        let snapshot = if resolve_snapshot {
            config.apply_snapshot()?
        } else {
            None
        };
        Ok(Loaded {
            config,
            text,
            format,
            snapshot,
        })
    }

    /// With `[control_plane]`: read and verify the snapshot, judge it against the
    /// floor `state.policy_dir` holds, put its keys where `[[keys]]` would be and
    /// check them by the rules file keys are checked by (0024). Reads files; records
    /// nothing (the caller records the generation once the snapshot is applied).
    fn apply_snapshot(&mut self) -> Result<Option<super::snapshot::Snapshot>, Vec<Issue>> {
        let Some(control_plane) = self.control_plane.clone() else {
            return Ok(None);
        };
        let floor = match &self.state.policy_dir {
            Some(dir) => super::snapshot::read_floor(dir).map_err(|issue| vec![issue])?,
            None => None,
        };
        let snapshot = super::snapshot::load(&control_plane, floor.as_ref(), chrono::Utc::now())?;
        if snapshot.keys.is_empty() {
            return Err(vec![Issue::new(
                "control_plane.snapshot_file",
                "the snapshot has no active key, so no caller could be identified",
            )]);
        }
        self.keys = snapshot.keys.clone();
        let mut issues = Vec::new();
        self.validate_keys(&mut issues);
        self.validate_tenant_limits(&mut issues);
        if issues.is_empty() {
            Ok(Some(snapshot))
        } else {
            // The snapshot's keys are reported at the setting that named it, with
            // the rule that refused them; the keys[N] paths index its active keys.
            Err(issues
                .into_iter()
                .map(|issue| {
                    Issue::new(
                        format!("control_plane.snapshot_file ({})", issue.path),
                        issue.message,
                    )
                })
                .collect())
        }
    }

    fn validate_control_plane(&self, issues: &mut Vec<Issue>) {
        let Some(control_plane) = &self.control_plane else {
            return;
        };
        if !self.keys.is_empty() {
            issues.push(Issue::new(
                "control_plane",
                "cannot be combined with [[keys]]: two sources of truth for who may call",
            ));
        }
        if self.server.allow_unauthenticated {
            issues.push(Issue::new(
                "server.allow_unauthenticated",
                "cannot be true with [control_plane], which exists to identify callers",
            ));
        }
        if let Some(max) = control_plane.max_age_secs {
            if !(1..=31_536_000).contains(&max) {
                issues.push(Issue::new(
                    "control_plane.max_age_secs",
                    "must be 1 to 31536000",
                ));
            }
        }
        if control_plane.refuse_when_stale && control_plane.max_age_secs.is_none() {
            issues.push(Issue::new(
                "control_plane.refuse_when_stale",
                "needs max_age_secs: it is the age at which requests are refused",
            ));
        }
        if control_plane.org.is_empty() || control_plane.org.len() > 128 {
            issues.push(Issue::new(
                "control_plane.org",
                "must be 1 to 128 characters",
            ));
        }
        if control_plane.verify_keys.is_empty() {
            issues.push(Issue::new(
                "control_plane.verify_keys",
                "needs at least one public key",
            ));
        }
        for (n, key) in control_plane.verify_keys.iter().enumerate() {
            if let Err(message) = super::snapshot::parse_verify_key(key) {
                issues.push(Issue::new(
                    format!("control_plane.verify_keys[{n}]"),
                    message,
                ));
            }
        }
        if !control_plane.snapshot_file.is_absolute() {
            issues.push(Issue::new(
                "control_plane.snapshot_file",
                "must be an absolute path",
            ));
        }
        match &self.state.policy_dir {
            None => issues.push(Issue::new(
                "state.policy_dir",
                "is required with [control_plane]: it records the generation applied, so a restart cannot be rolled back",
            )),
            Some(dir) if !dir.is_absolute() => {
                issues.push(Issue::new("state.policy_dir", "must be an absolute path"))
            }
            Some(_) => {}
        }
    }

    fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        if self.version != VERSION {
            issues.push(Issue::new(
                "version",
                format!(
                    "unsupported version {}; this build reads version {VERSION}",
                    self.version
                ),
            ));
            // A different version may mean different rules for everything else.
            return issues;
        }
        self.validate_server(&mut issues);
        self.validate_admin(&mut issues);
        self.validate_reload(&mut issues);
        if let Some(fleet) = &self.fleet {
            validate_secret(&fleet.redis_url, "fleet.redis_url", &mut issues);
        }
        self.validate_providers(&mut issues);
        self.validate_limits(&mut issues);
        self.validate_state(&mut issues);
        self.validate_logging(&mut issues);
        self.validate_telemetry(&mut issues);
        self.validate_auth(&mut issues);
        self.validate_upstream(&mut issues);
        self.validate_keys(&mut issues);
        self.validate_tenant_limits(&mut issues);
        self.validate_control_plane(&mut issues);
        self.validate_routes(&mut issues);
        issues
    }

    fn validate_server(&self, issues: &mut Vec<Issue>) {
        for (n, cidr) in self.server.trusted_proxies.iter().enumerate() {
            if let Err(message) = super::allowlist::Cidr::parse(cidr) {
                issues.push(Issue::new(format!("server.trusted_proxies[{n}]"), message));
            }
        }
        let host = self.server.host.as_str();
        let bare = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        match bare.parse::<std::net::IpAddr>() {
            Err(_) => {
                issues.push(Issue::new("server.host", "must be an IPv4 or IPv6 address"));
            }
            // Serving with no caller identity on a routable interface hands the
            // upstream keys' spend to anyone who can reach the port, so it is
            // limited to loopback (a sidecar, a local developer). A deployment that
            // listens on a routable address defines [[keys]].
            Ok(address) if self.server.allow_unauthenticated && !address.is_loopback() => {
                issues.push(Issue::new(
                    "server.host",
                    "must be a loopback address (127.0.0.0/8 or ::1) while \
                     server.allow_unauthenticated is true; to listen on a routable \
                     address, remove server.allow_unauthenticated and define [[keys]]",
                ));
            }
            Ok(_) => {}
        }
        if self.keys.is_empty()
            && !self.server.allow_unauthenticated
            && self.control_plane.is_none()
        {
            issues.push(Issue::new(
                "keys",
                "no caller keys are defined; add [[keys]], or set server.allow_unauthenticated = true \
                 (which needs a loopback server.host)",
            ));
        }
        if !self.keys.is_empty() && self.server.allow_unauthenticated {
            issues.push(Issue::new(
                "server.allow_unauthenticated",
                "cannot be true while [[keys]] are defined",
            ));
        }
    }

    fn validate_tls(tls: &Tls, at: &str, issues: &mut Vec<Issue>) {
        let ca = tls.client_ca_file.iter().map(|p| ("client_ca_file", p));
        for (field, path) in [("cert_file", &tls.cert_file), ("key_file", &tls.key_file)]
            .into_iter()
            .chain(ca)
        {
            if !path.has_root() {
                issues.push(Issue::new(
                    format!("{at}.{field}"),
                    "must be an absolute path",
                ));
            }
        }
        if tls.cert_file == tls.key_file {
            issues.push(Issue::new(
                format!("{at}.key_file"),
                "must be a different file from cert_file",
            ));
        }
        if let Some(seconds) = tls.reload_interval_secs {
            if !(1..=86_400).contains(&seconds) {
                issues.push(Issue::new(
                    format!("{at}.reload_interval_secs"),
                    "must be between 1 and 86400",
                ));
            }
        }
    }

    fn validate_reload(&self, issues: &mut Vec<Issue>) {
        if let Some(seconds) = self.reload.interval_secs {
            if !(1..=86_400).contains(&seconds) {
                issues.push(Issue::new(
                    "reload.interval_secs",
                    "must be between 1 and 86400",
                ));
            }
        }
    }

    fn validate_admin(&self, issues: &mut Vec<Issue>) {
        if let Some(tls) = &self.server.tls {
            Self::validate_tls(tls, "server.tls", issues);
        }
        if let Some(tls) = self.admin.as_ref().and_then(|admin| admin.tls.as_ref()) {
            Self::validate_tls(tls, "admin.tls", issues);
            if tls.client_ca_file.is_some() {
                issues.push(Issue::new(
                    "admin.tls.client_ca_file",
                    "client certificates are supported on the main listener only",
                ));
            }
        }
        let Some(admin) = &self.admin else {
            return;
        };
        let bare = admin
            .host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(&admin.host);
        if bare.parse::<std::net::IpAddr>().is_err() {
            issues.push(Issue::new("admin.host", "must be an IPv4 or IPv6 address"));
        }
        if admin.port != 0
            && admin.port == self.server.port
            && admin.host.trim_matches(['[', ']']) == self.server.host.trim_matches(['[', ']'])
        {
            issues.push(Issue::new(
                "admin.port",
                "must differ from server.port when both listen on the same address",
            ));
        }
        match (&admin.token, admin.allow_unauthenticated) {
            (None, false) => issues.push(Issue::new(
                "admin.token",
                "set a token, or set admin.allow_unauthenticated = true to serve \
                 /metrics and /v1/gateway/stats without one",
            )),
            (Some(_), true) => issues.push(Issue::new(
                "admin.allow_unauthenticated",
                "cannot be true while admin.token is set",
            )),
            _ => {}
        }
        if let Some(token) = &admin.token {
            validate_secret(token, "admin.token", issues);
            let reference = format!("{token:?}");
            let shared = self
                .providers
                .values()
                .filter_map(|p| p.api_key.as_ref())
                .chain(self.keys.iter().filter_map(|k| k.secret.as_ref()))
                .any(|other| format!("{other:?}") == reference);
            if shared {
                issues.push(Issue::new(
                    "admin.token",
                    "refers to a provider key or a caller key; use a separate secret",
                ));
            }
        }
    }

    fn validate_providers(&self, issues: &mut Vec<Issue>) {
        if self.providers.is_empty() {
            issues.push(Issue::new("providers", "at least one provider is required"));
        }
        for (name, provider) in &self.providers {
            let at = format!("providers.{name}");
            if !valid_name(name) {
                issues.push(Issue::new(&at, NAME_RULE));
            }
            if name == "route" {
                issues.push(Issue::new(&at, "\"route\" is reserved for named routes"));
            }
            match provider.kind.hosted_name() {
                Some(expected) if name != expected => issues.push(Issue::new(
                    &at,
                    format!(
                        "a {expected} provider must be named \"{expected}\" (one per kind for now)"
                    ),
                )),
                None if HOSTED_NAMES.contains(&name.as_str()) => issues.push(Issue::new(
                    &at,
                    "is the name of a hosted provider; name an openai-compatible server differently",
                )),
                _ => {}
            }
            match &provider.api_key {
                Some(secret) => validate_secret(secret, &format!("{at}.api_key"), issues),
                None if provider.kind.requires_api_key() => {
                    issues.push(Issue::new(format!("{at}.api_key"), "is required"))
                }
                None => {}
            }
            match &provider.base_url {
                Some(url) => {
                    if let Err(reason) = validate_base_url(url) {
                        issues.push(Issue::new(format!("{at}.base_url"), reason));
                    }
                }
                None if provider.kind.requires_base_url() => issues.push(Issue::new(
                    format!("{at}.base_url"),
                    "is required for kind \"openai-compatible\"",
                )),
                None => {}
            }
            self.validate_cloud_fields(name, provider, issues);
        }
        self.validate_shared_vertex_identity(issues);
    }

    /// The fields only a cloud kind takes, and the rules of the transports
    /// themselves (0020), so a value that would fail every request fails here.
    fn validate_cloud_fields(&self, name: &str, provider: &Provider, issues: &mut Vec<Issue>) {
        use super::config::ProviderKind as K;
        use crate::providers::cloud_rules as rules;
        let at = format!("providers.{name}");
        let kind = provider.kind;
        let owners: [(&str, bool, &[K]); 9] = [
            ("profile", provider.profile.is_some(), &[K::Bedrock]),
            (
                "region",
                provider.region.is_some(),
                &[K::Bedrock, K::VertexGemini, K::VertexAnthropic],
            ),
            ("config_file", provider.config_file.is_some(), &[K::Bedrock]),
            (
                "credentials_file",
                provider.credentials_file.is_some(),
                &[K::Bedrock],
            ),
            (
                "project",
                provider.project.is_some(),
                &[K::VertexGemini, K::VertexAnthropic],
            ),
            (
                "binding",
                provider.binding.is_some(),
                &[K::VertexGemini, K::VertexAnthropic, K::Azure],
            ),
            (
                "service_account_file",
                provider.service_account_file.is_some(),
                &[K::VertexGemini, K::VertexAnthropic],
            ),
            ("endpoint", provider.endpoint.is_some(), &[K::Azure]),
            ("deployments", !provider.deployments.is_empty(), &[K::Azure]),
        ];
        for (field, set, allowed) in owners {
            if set && !allowed.contains(&kind) {
                let kinds: Vec<&str> = allowed.iter().map(|k| k.kind_name()).collect();
                issues.push(Issue::new(
                    format!("{at}.{field}"),
                    format!("applies to kind {} only", kinds.join(" or ")),
                ));
            }
        }
        if provider.api_key.is_some() && !matches!(kind, K::Azure) && kind.is_cloud() {
            issues.push(Issue::new(
                format!("{at}.api_key"),
                format!("is not used by kind \"{}\"", kind.kind_name()),
            ));
        }
        if provider.base_url.is_some() && kind.is_cloud() {
            issues.push(Issue::new(
                format!("{at}.base_url"),
                format!(
                    "is not used by kind \"{}\" (the endpoint follows from the region)",
                    kind.kind_name()
                ),
            ));
        }
        let need = |field: &str, present: bool, issues: &mut Vec<Issue>| {
            if !present {
                issues.push(Issue::new(
                    format!("{at}.{field}"),
                    format!("is required for kind \"{}\"", kind.kind_name()),
                ));
            }
        };
        let binding_ok = |binding: &str| valid_name(binding);
        match kind {
            K::Bedrock => {
                need("profile", provider.profile.is_some(), issues);
                need("region", provider.region.is_some(), issues);
                if let Some(profile) = &provider.profile {
                    if profile.is_empty()
                        || profile.len() > 128
                        || !profile.bytes().all(|b| b.is_ascii_graphic())
                    {
                        issues.push(Issue::new(
                            format!("{at}.profile"),
                            "must be 1 to 128 visible ASCII characters",
                        ));
                    }
                }
                if let Some(region) = &provider.region {
                    if let Err(reason) = rules::bedrock_region(region) {
                        issues.push(Issue::new(format!("{at}.region"), reason));
                    }
                }
                if provider.config_file.is_some() != provider.credentials_file.is_some() {
                    issues.push(Issue::new(
                        &at,
                        "set config_file and credentials_file together, or neither",
                    ));
                }
                for (field, path) in [
                    ("config_file", &provider.config_file),
                    ("credentials_file", &provider.credentials_file),
                ] {
                    if path.as_ref().is_some_and(|p| !p.has_root()) {
                        issues.push(Issue::new(
                            format!("{at}.{field}"),
                            "must be an absolute path",
                        ));
                    }
                }
            }
            K::VertexGemini | K::VertexAnthropic => {
                need("project", provider.project.is_some(), issues);
                need("region", provider.region.is_some(), issues);
                need("binding", provider.binding.is_some(), issues);
                need(
                    "service_account_file",
                    provider.service_account_file.is_some(),
                    issues,
                );
                if provider
                    .project
                    .as_deref()
                    .is_some_and(|p| !rules::vertex_label(p))
                {
                    issues.push(Issue::new(
                        format!("{at}.project"),
                        "must be 1 to 128 lower-case letters, digits or hyphens",
                    ));
                }
                if provider.region.as_deref().is_some_and(|region| {
                    // The transport also maps `global`, `us` and `eu` to their own hosts.
                    !rules::vertex_label(region)
                }) {
                    issues.push(Issue::new(
                        format!("{at}.region"),
                        "must be 1 to 128 lower-case letters, digits or hyphens",
                    ));
                }
                if provider.binding.as_deref().is_some_and(|b| !binding_ok(b)) {
                    issues.push(Issue::new(format!("{at}.binding"), NAME_RULE));
                }
                if provider
                    .service_account_file
                    .as_ref()
                    .is_some_and(|p| !p.has_root())
                {
                    issues.push(Issue::new(
                        format!("{at}.service_account_file"),
                        "must be an absolute path",
                    ));
                }
            }
            K::Azure => {
                need("endpoint", provider.endpoint.is_some(), issues);
                need("binding", provider.binding.is_some(), issues);
                need("api_key", provider.api_key.is_some(), issues);
                if provider
                    .endpoint
                    .as_deref()
                    .is_some_and(|e| rules::azure_root(e).is_none())
                {
                    issues.push(Issue::new(
                        format!("{at}.endpoint"),
                        "must be an https resource origin (or its /openai/v1 base) without credentials, query or fragment",
                    ));
                }
                if provider.binding.as_deref().is_some_and(|b| !binding_ok(b)) {
                    issues.push(Issue::new(format!("{at}.binding"), NAME_RULE));
                }
                if provider.deployments.is_empty() {
                    issues.push(Issue::new(
                        format!("{at}.deployments"),
                        "is required: an Azure provider with no deployment could serve nothing",
                    ));
                }
                for (deployment, model) in &provider.deployments {
                    if !rules::azure_deployment(deployment, model) {
                        issues.push(Issue::new(
                            format!("{at}.deployments.{deployment}"),
                            "a deployment name is 1 to 256 letters, digits, '-', '_' or '.', and its model is not empty",
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    /// The two Vertex kinds share one identity and one token cache, so when both
    /// are configured they must name the same one.
    fn validate_shared_vertex_identity(&self, issues: &mut Vec<Issue>) {
        let (Some(gemini), Some(anthropic)) = (
            self.providers.get("vertex-gemini"),
            self.providers.get("vertex-anthropic"),
        ) else {
            return;
        };
        for (field, same) in [
            ("project", gemini.project == anthropic.project),
            ("region", gemini.region == anthropic.region),
            ("binding", gemini.binding == anthropic.binding),
            (
                "service_account_file",
                gemini.service_account_file == anthropic.service_account_file,
            ),
        ] {
            if !same {
                issues.push(Issue::new(
                    format!("providers.vertex-anthropic.{field}"),
                    "must equal providers.vertex-gemini's: the two share one identity",
                ));
            }
        }
    }

    fn validate_logging(&self, issues: &mut Vec<Issue>) {
        for (field, target) in [
            ("logging.access_log", &self.logging.access_log),
            ("logging.usage_log", &self.logging.usage_log),
        ] {
            let Some(target) = target else { continue };
            let is_file = std::path::Path::new(target).has_root();
            if target != "stdout" && !is_file {
                issues.push(Issue::new(
                    field,
                    "must be \"stdout\" or an absolute file path",
                ));
            }
            if is_file && target.ends_with('/') {
                issues.push(Issue::new(field, "must name a file, not a directory"));
            }
        }
        // Two writers on one file would interleave their lines. Both on
        // "stdout" is fine: each writes whole lines to the same stream.
        if let (Some(access), Some(usage)) = (&self.logging.access_log, &self.logging.usage_log) {
            if access == usage && access != "stdout" {
                issues.push(Issue::new(
                    "logging.usage_log",
                    "must differ from logging.access_log; two logs cannot share a file",
                ));
            }
        }
    }

    /// The upstream client's and the listener's limits (0021): each key's range,
    /// then the ordering of the deadlines, so no deadline is set past the one that
    /// bounds it.
    fn validate_upstream(&self, issues: &mut Vec<Issue>) {
        let u = &self.upstream;
        let mut bound = |key: &str, value: Option<u64>, lo: u64, hi: u64| {
            if value.is_some_and(|v| !(lo..=hi).contains(&v)) {
                issues.push(Issue::new(key, format!("must be between {lo} and {hi}")));
                false
            } else {
                true
            }
        };
        let mut ok = true;
        ok &= bound("upstream.max_retries", u.max_retries.map(u64::from), 0, 10);
        ok &= bound("upstream.max_backoff_secs", u.max_backoff_secs, 1, 300);
        ok &= bound(
            "upstream.pool_max_idle_per_host",
            u.pool_max_idle_per_host.map(u64::from),
            1,
            4096,
        );
        ok &= bound(
            "upstream.pool_idle_timeout_secs",
            u.pool_idle_timeout_secs,
            1,
            3600,
        );
        for (key, value) in [
            ("connect_timeout_ms", u.connect_timeout_ms),
            ("response_header_timeout_ms", u.response_header_timeout_ms),
            ("policy_callback_timeout_ms", u.policy_callback_timeout_ms),
            ("error_body_idle_timeout_ms", u.error_body_idle_timeout_ms),
            ("error_body_total_timeout_ms", u.error_body_total_timeout_ms),
            ("unary_idle_timeout_ms", u.unary_idle_timeout_ms),
            ("unary_attempt_timeout_ms", u.unary_attempt_timeout_ms),
            ("stream_idle_timeout_ms", u.stream_idle_timeout_ms),
            ("stream_attempt_timeout_ms", u.stream_attempt_timeout_ms),
        ] {
            // Positive; the upper end is "finite", which the library checks.
            ok &= bound(&format!("upstream.{key}"), value, 1, 24 * 3600 * 1000 * 365);
        }
        ok &= bound(
            "server.max_connections",
            self.server.max_connections.map(u64::from),
            1,
            65535,
        );
        ok &= bound(
            "server.header_timeout_ms",
            self.server.header_timeout_ms,
            1,
            600_000,
        );
        if !ok {
            return;
        }
        let settings = match u.settings(self.limits.max_concurrency) {
            Ok(settings) => settings,
            Err(message) => {
                issues.push(Issue::new("upstream", message));
                return;
            }
        };
        // Each deadline may not exceed the one that bounds it, or it could never
        // fire. The values are the effective ones, defaults included; the issue is
        // reported at the key the file set (the one written is the one to change).
        let d = settings.deadlines;
        let ms = |value: std::time::Duration| value.as_millis();
        let set = |key: &str| match key {
            "connect_timeout_ms" => u.connect_timeout_ms.is_some(),
            "response_header_timeout_ms" => u.response_header_timeout_ms.is_some(),
            "unary_idle_timeout_ms" => u.unary_idle_timeout_ms.is_some(),
            "unary_attempt_timeout_ms" => u.unary_attempt_timeout_ms.is_some(),
            "stream_idle_timeout_ms" => u.stream_idle_timeout_ms.is_some(),
            "stream_attempt_timeout_ms" => u.stream_attempt_timeout_ms.is_some(),
            "error_body_idle_timeout_ms" => u.error_body_idle_timeout_ms.is_some(),
            "error_body_total_timeout_ms" => u.error_body_total_timeout_ms.is_some(),
            _ => false,
        };
        let attempts = [
            ("unary_attempt_timeout_ms", ms(d.unary_attempt_total)),
            ("stream_attempt_timeout_ms", ms(d.stream_attempt_total)),
        ];
        for (key, value, limits) in [
            ("connect_timeout_ms", ms(d.connect), attempts.to_vec()),
            (
                "response_header_timeout_ms",
                ms(d.response_headers),
                attempts.to_vec(),
            ),
            (
                "unary_idle_timeout_ms",
                ms(d.unary_body_idle),
                vec![("unary_attempt_timeout_ms", ms(d.unary_attempt_total))],
            ),
            (
                "stream_idle_timeout_ms",
                ms(d.stream_semantic_idle),
                vec![("stream_attempt_timeout_ms", ms(d.stream_attempt_total))],
            ),
            (
                "error_body_idle_timeout_ms",
                ms(d.error_body_idle),
                vec![("error_body_total_timeout_ms", ms(d.error_body_total))],
            ),
        ] {
            for (limit_key, limit) in limits {
                if value <= limit {
                    continue;
                }
                if set(key) {
                    issues.push(Issue::new(
                        format!("upstream.{key}"),
                        format!("may not exceed {limit_key} ({limit} ms): it could never fire"),
                    ));
                } else if set(limit_key) {
                    issues.push(Issue::new(
                        format!("upstream.{limit_key}"),
                        format!(
                            "may not be shorter than {key} ({value} ms): that one could never fire"
                        ),
                    ));
                }
            }
        }
    }

    fn validate_auth(&self, issues: &mut Vec<Issue>) {
        let mut names = BTreeSet::new();
        let mut issuers = BTreeSet::new();
        for (index, table) in self.auth.jwt.iter().enumerate() {
            let at = format!("auth.jwt[{index}]");
            if !valid_name(&table.name) {
                issues.push(Issue::new(format!("{at}.name"), NAME_RULE));
            } else if !names.insert(table.name.as_str()) {
                issues.push(Issue::new(
                    format!("{at}.name"),
                    "duplicates another issuer's name",
                ));
            }
            if table.issuer.is_empty()
                || table.issuer.len() > 512
                || !table.issuer.bytes().all(|b| b.is_ascii_graphic())
            {
                issues.push(Issue::new(
                    format!("{at}.issuer"),
                    "must be 1 to 512 visible ASCII characters",
                ));
            } else if !issuers.insert(table.issuer.as_str()) {
                issues.push(Issue::new(
                    format!("{at}.issuer"),
                    "duplicates another table's issuer, so the table would depend on order",
                ));
            }
            if let Some(problem) = super::jwks::url_problem(&table.jwks_url) {
                issues.push(Issue::new(format!("{at}.jwks_url"), problem));
            }
            if let Some(path) = &table.ca_file {
                if !path.has_root() {
                    issues.push(Issue::new(
                        format!("{at}.ca_file"),
                        "must be an absolute path",
                    ));
                }
            }
            if table.audience.is_empty()
                || table.audience.len() > 256
                || !table.audience.bytes().all(|b| b.is_ascii_graphic())
            {
                issues.push(Issue::new(
                    format!("{at}.audience"),
                    "must be 1 to 256 visible ASCII characters",
                ));
            }
            if table.algorithms.is_empty() {
                issues.push(Issue::new(
                    format!("{at}.algorithms"),
                    "must list at least one of RS256, ES256 and EdDSA",
                ));
            }
            let mut seen = BTreeSet::new();
            for (n, name) in table.algorithms.iter().enumerate() {
                if let Err(message) = super::jwt::Alg::parse(name) {
                    issues.push(Issue::new(format!("{at}.algorithms[{n}]"), message));
                } else if !seen.insert(name.as_str()) {
                    issues.push(Issue::new(
                        format!("{at}.algorithms[{n}]"),
                        "is listed twice",
                    ));
                }
            }
            let bound =
                |value: Option<u64>, lo: u64, hi: u64, name: &str, issues: &mut Vec<Issue>| {
                    if value.is_some_and(|v| !(lo..=hi).contains(&v)) {
                        issues.push(Issue::new(
                            format!("{at}.{name}"),
                            format!("must be between {lo} and {hi}"),
                        ));
                    }
                };
            bound(table.clock_skew_secs, 0, 300, "clock_skew_secs", issues);
            bound(
                table.jwks_refresh_secs,
                30,
                86_400,
                "jwks_refresh_secs",
                issues,
            );
            bound(
                table.jwks_max_stale_secs,
                0,
                604_800,
                "jwks_max_stale_secs",
                issues,
            );
            for (name, secret) in &table.headers {
                let header = format!("{at}.headers.{name}");
                let token = !name.is_empty()
                    && name.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte)
                    });
                if !token {
                    issues.push(Issue::new(
                        &header,
                        "a header name is lower-case letters, digits, '-' or '_'",
                    ));
                }
                validate_secret(secret, &header, issues);
            }
        }
    }

    fn validate_telemetry(&self, issues: &mut Vec<Issue>) {
        let Some(telemetry) = &self.telemetry else {
            return;
        };
        if let Err(reason) = validate_base_url(&telemetry.otlp_endpoint) {
            issues.push(Issue::new("telemetry.otlp_endpoint", reason));
        }
        if let Some(name) = &telemetry.service_name {
            if name.is_empty()
                || name.len() > 128
                || !name.bytes().all(|byte| byte.is_ascii_graphic())
            {
                issues.push(Issue::new(
                    "telemetry.service_name",
                    "must be 1 to 128 visible ASCII characters",
                ));
            }
        }
        if let Some(ratio) = telemetry.sample_ratio {
            if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
                issues.push(Issue::new(
                    "telemetry.sample_ratio",
                    "must be a number from 0 to 1",
                ));
            }
        }
        for (name, secret) in &telemetry.headers {
            let at = format!("telemetry.headers.{name}");
            let token = !name.is_empty()
                && name.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte)
                });
            if !token {
                issues.push(Issue::new(
                    &at,
                    "a header name is lower-case letters, digits, '-' or '_'",
                ));
            }
            validate_secret(secret, &at, issues);
        }
    }

    fn validate_state(&self, issues: &mut Vec<Issue>) {
        if let Some(dir) = &self.state.receipts_dir {
            if !dir.has_root() {
                issues.push(Issue::new("state.receipts_dir", "must be an absolute path"));
            }
        }
        if let Some(ttl) = self.state.idempotency_ttl_secs {
            if !(1..=604_800).contains(&ttl) {
                issues.push(Issue::new(
                    "state.idempotency_ttl_secs",
                    "must be between 1 and 604800 (seven days)",
                ));
            }
        }
        if let Some(ms) = self.state.receipt_lock_timeout_ms {
            if !(1..=30_000).contains(&ms) {
                issues.push(Issue::new(
                    "state.receipt_lock_timeout_ms",
                    "must be between 1 and 30000 (thirty seconds)",
                ));
            }
        }
    }

    fn validate_limits(&self, issues: &mut Vec<Issue>) {
        if self.limits.max_concurrency == Some(0) {
            issues.push(Issue::new("limits.max_concurrency", "must be at least 1"));
        }
        if self
            .limits
            .penalty_secs
            .is_some_and(|secs| !(1..=MAX_PENALTY_SECS).contains(&secs))
        {
            issues.push(Issue::new(
                "limits.penalty_secs",
                format!("must be between 1 and {MAX_PENALTY_SECS} (one hour)"),
            ));
        }
        if self
            .limits
            .queue_timeout_ms
            .is_some_and(|ms| ms > MAX_QUEUE_TIMEOUT_MS)
        {
            issues.push(Issue::new(
                "limits.queue_timeout_ms",
                format!("must be at most {MAX_QUEUE_TIMEOUT_MS} (one hour)"),
            ));
        }
        for name in self.limits.providers.keys() {
            if !self.providers.contains_key(name) {
                issues.push(Issue::new(
                    format!("limits.providers.{name}"),
                    "no provider with this name is defined",
                ));
            }
        }
    }

    /// The keys of one tenant share one rate bucket (rate limits are per tenant
    /// and provider, not per key), so they must agree on its size. Otherwise the
    /// bucket would be sized by whichever key happened to arrive first, and a
    /// reload that retunes buckets would flip it back and forth as the keys
    /// alternate.
    fn validate_tenant_limits(&self, issues: &mut Vec<Issue>) {
        let mut first: BTreeMap<&str, (usize, &GatewayKey)> = BTreeMap::new();
        for (index, key) in self.keys.iter().enumerate() {
            let Some((earlier, other)) = first.get(key.tenant.as_str()).copied() else {
                first.insert(&key.tenant, (index, key));
                continue;
            };
            for (name, mine, theirs) in [("rpm", key.rpm, other.rpm), ("tpm", key.tpm, other.tpm)] {
                if mine != theirs {
                    issues.push(Issue::new(
                        format!("keys[{index}].{name}"),
                        format!(
                            "differs from keys[{earlier}].{name}; the keys of one tenant share \
                             one rate bucket, so they must carry the same rpm and tpm"
                        ),
                    ));
                }
            }
        }
    }

    fn validate_keys(&self, issues: &mut Vec<Issue>) {
        let mut names = BTreeSet::new();
        let mut digests = BTreeSet::new();
        let mut references = BTreeSet::new();
        let mut client_sans = BTreeSet::new();
        let mut jwt_bindings = BTreeSet::new();
        let upstream: BTreeSet<String> = self
            .providers
            .values()
            .filter_map(|provider| provider.api_key.as_ref())
            .map(|secret| format!("{secret:?}"))
            .collect();
        for (index, key) in self.keys.iter().enumerate() {
            let at = format!("keys[{index}]");
            if !valid_name(&key.name) {
                issues.push(Issue::new(format!("{at}.name"), NAME_RULE));
            } else if !names.insert(key.name.as_str()) {
                issues.push(Issue::new(
                    format!("{at}.name"),
                    "duplicates another key's name",
                ));
            }
            if !valid_tenant(&key.tenant) {
                issues.push(Issue::new(format!("{at}.tenant"), TENANT_RULE));
            }
            if let Some(expires) = &key.expires_at {
                if chrono::DateTime::parse_from_rfc3339(expires).is_err() {
                    issues.push(Issue::new(
                        format!("{at}.expires_at"),
                        "must be an RFC 3339 time such as 2026-12-31T00:00:00Z",
                    ));
                }
            }
            if let Some(cidrs) = &key.allowed_cidrs {
                if cidrs.is_empty() {
                    issues.push(Issue::new(
                        format!("{at}.allowed_cidrs"),
                        "is empty, which would refuse every request; omit it for no restriction",
                    ));
                }
                for (n, cidr) in cidrs.iter().enumerate() {
                    if let Err(message) = super::allowlist::Cidr::parse(cidr) {
                        issues.push(Issue::new(format!("{at}.allowed_cidrs[{n}]"), message));
                    }
                }
            }
            if let Some(binding) = &key.jwt {
                if key.secret.is_some() || key.sha256.is_some() || key.client_san.is_some() {
                    issues.push(Issue::new(
                        &at,
                        "set jwt, or one of secret, sha256 and client_san, not several",
                    ));
                }
                if !self
                    .auth
                    .jwt
                    .iter()
                    .any(|table| table.name == binding.table)
                {
                    issues.push(Issue::new(
                        format!("{at}.jwt.table"),
                        "names no [[auth.jwt]] table",
                    ));
                }
                if binding.subject.is_empty()
                    || binding.subject.len() > super::jwt::MAX_SUBJECT_BYTES
                    || !binding.subject.bytes().all(|b| b.is_ascii_graphic())
                {
                    issues.push(Issue::new(
                        format!("{at}.jwt.subject"),
                        "must be 1 to 512 visible ASCII characters",
                    ));
                } else if !jwt_bindings.insert((binding.table.clone(), binding.subject.clone())) {
                    issues.push(Issue::new(
                        format!("{at}.jwt"),
                        "duplicates another key's table and subject",
                    ));
                }
            } else if let Some(san) = &key.client_san {
                if key.secret.is_some() || key.sha256.is_some() {
                    issues.push(Issue::new(
                        &at,
                        "set client_san, or secret or sha256, not both",
                    ));
                }
                if !valid_client_san(san) {
                    issues.push(Issue::new(
                        format!("{at}.client_san"),
                        "must be \"dns:<name>\" or \"uri:<uri>\"",
                    ));
                } else if !client_sans.insert(canonical_san(san)) {
                    issues.push(Issue::new(
                        format!("{at}.client_san"),
                        "duplicates another key's client_san",
                    ));
                }
                if self
                    .server
                    .tls
                    .as_ref()
                    .and_then(|tls| tls.client_ca_file.as_ref())
                    .is_none()
                {
                    issues.push(Issue::new(
                        format!("{at}.client_san"),
                        "needs server.tls.client_ca_file; without it no client certificate is requested",
                    ));
                }
                // The remaining checks below concern bearer credentials.
            } else {
                match (&key.secret, &key.sha256) {
                    (Some(_), Some(_)) => {
                        issues.push(Issue::new(&at, "set either secret or sha256, not both"))
                    }
                    (None, None) => {
                        issues.push(Issue::new(&at, "set secret, sha256, client_san or jwt"))
                    }
                    (Some(secret), None) => {
                        validate_secret(secret, &format!("{at}.secret"), issues);
                        if upstream.contains(&format!("{secret:?}")) {
                            issues.push(Issue::new(
                            format!("{at}.secret"),
                            "refers to a provider's api_key; a caller key must be a different secret",
                        ));
                        }
                        if !references.insert(format!("{secret:?}")) {
                            issues.push(Issue::new(
                                format!("{at}.secret"),
                                "refers to the same secret as another key",
                            ));
                        }
                    }
                    (None, Some(digest)) => {
                        let well_formed = digest.len() == 64
                            && digest
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                        if !well_formed {
                            issues.push(Issue::new(
                                format!("{at}.sha256"),
                                "must be 64 lower-case hexadecimal characters",
                            ));
                        } else if !digests.insert(digest.as_str()) {
                            issues.push(Issue::new(
                                format!("{at}.sha256"),
                                "duplicates another key's digest",
                            ));
                        }
                    }
                }
            }
            if let Some(budget) = key.budget_usd {
                if !budget.is_finite() || !(0.0..=MAX_BUDGET_USD).contains(&budget) {
                    issues.push(Issue::new(
                        format!("{at}.budget_usd"),
                        format!("must be between 0 and {MAX_BUDGET_USD}"),
                    ));
                }
            }
            if key
                .budget_window_secs
                .is_some_and(|secs| !(1..=MAX_BUDGET_WINDOW_SECS).contains(&secs))
            {
                issues.push(Issue::new(
                    format!("{at}.budget_window_secs"),
                    format!("must be between 1 and {MAX_BUDGET_WINDOW_SECS} (366 days)"),
                ));
            }
            if key.budget_usd.is_none()
                && (key.budget_window_secs.is_some() || key.budget_allow_unpriced)
            {
                issues.push(Issue::new(
                    &at,
                    "budget_window_secs and budget_allow_unpriced need budget_usd",
                ));
            }
        }
    }

    fn validate_routes(&self, issues: &mut Vec<Issue>) {
        for (name, route) in &self.routes {
            let at = format!("routes.{name}");
            if !valid_name(name) {
                issues.push(Issue::new(&at, NAME_RULE));
            }
            let model = route.model.trim();
            if model.is_empty() {
                issues.push(Issue::new(format!("{at}.model"), "must not be empty"));
            } else if model != route.model {
                issues.push(Issue::new(
                    format!("{at}.model"),
                    "must not have leading or trailing whitespace",
                ));
            } else if model.starts_with("route/") {
                issues.push(Issue::new(
                    format!("{at}.model"),
                    "routes do not chain; name a provider model",
                ));
            } else {
                match model.split_once('/') {
                    Some((provider, name))
                        if self.providers.contains_key(provider) && !name.is_empty() => {}
                    Some((provider, _)) if !self.providers.contains_key(provider) => {
                        issues.push(Issue::new(
                            format!("{at}.model"),
                            format!("no provider named \"{provider}\" is defined"),
                        ))
                    }
                    _ => issues.push(Issue::new(
                        format!("{at}.model"),
                        "must be written as <provider>/<model>",
                    )),
                }
            }
            if route.settings.contains_key("model") {
                issues.push(Issue::new(
                    format!("{at}.settings.model"),
                    "set the route's model with `model`, not in settings",
                ));
            }
        }
    }

    /// Every secret reference in the file, with its path.
    fn secret_refs(&self) -> Vec<(String, &SecretRef)> {
        let providers = self.providers.iter().filter_map(|(name, provider)| {
            provider
                .api_key
                .as_ref()
                .map(|secret| (format!("providers.{name}.api_key"), secret))
        });
        let keys = self.keys.iter().enumerate().filter_map(|(index, key)| {
            key.secret
                .as_ref()
                .map(|secret| (format!("keys[{index}].secret"), secret))
        });
        let admin = self
            .admin
            .iter()
            .filter_map(|admin| admin.token.as_ref())
            .map(|secret| ("admin.token".to_string(), secret));
        let fleet = self
            .fleet
            .iter()
            .map(|fleet| ("fleet.redis_url".to_string(), &fleet.redis_url));
        let telemetry = self.telemetry.iter().flat_map(|telemetry| {
            telemetry
                .headers
                .iter()
                .map(|(name, secret)| (format!("telemetry.headers.{name}"), secret))
        });
        let jwks = self.auth.jwt.iter().enumerate().flat_map(|(index, table)| {
            table
                .headers
                .iter()
                .map(move |(name, secret)| (format!("auth.jwt[{index}].headers.{name}"), secret))
        });
        providers
            .chain(keys)
            .chain(admin)
            .chain(fleet)
            .chain(telemetry)
            .chain(jwks)
            .collect()
    }

    /// Settings this build cannot honour because a Cargo feature is off. Reported
    /// at `--check` and at startup, naming the flag to rebuild with.
    pub fn feature_issues(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        if self.fleet.is_some() && !cfg!(feature = "serve-redis") {
            issues.push(Issue::new(
                "fleet",
                "fleet mode needs a binary built with --features serve-redis",
            ));
        }
        if !cfg!(feature = "serve-cloud") {
            for (name, provider) in &self.providers {
                if provider.kind.is_cloud() {
                    issues.push(Issue::new(
                        format!("providers.{name}"),
                        "kind \"".to_string()
                            + provider.kind.kind_name()
                            + "\" needs a binary built with --features serve-cloud",
                    ));
                }
            }
        }
        if self.telemetry.is_some() && !cfg!(feature = "serve-otel") {
            // A setting that is parsed and then ignored would say traces are
            // leaving when none are.
            issues.push(Issue::new(
                "telemetry",
                "trace export needs a binary built with --features serve-otel",
            ));
        }
        issues
    }

    /// Check that every referenced secret exists and is non-empty, without
    /// returning or printing any value. `env` looks up one variable, so a
    /// caller (or a test) decides what the environment is.
    pub fn check_references(&self, env: &dyn Fn(&str) -> Option<String>) -> Vec<Issue> {
        let mut issues = self.check_same_values(env);
        issues.extend(self.check_secrets_present(env));
        issues
    }

    /// Whether every referenced secret is available: the environment variable is
    /// set and not blank, or the file can be read and is not blank. These are
    /// facts about the environment the check runs in, not about the file, so
    /// `serve` reports them with their own exit status; a pipeline that holds no
    /// secrets can skip them with `--structure-only`.
    pub fn check_secrets_present(&self, env: &dyn Fn(&str) -> Option<String>) -> Vec<Issue> {
        let mut issues = Vec::new();
        // A service-account file is a credential: missing or empty is the same
        // kind of absence as an unset secret.
        for (name, provider) in &self.providers {
            if let Some(path) = &provider.service_account_file {
                let at = format!("providers.{name}.service_account_file");
                match crate::providers::cloud_rules::read_service_account(path) {
                    Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => {
                        issues.push(Issue::new(
                            at,
                            format!("service-account file {} is empty", path.display()),
                        ))
                    }
                    Ok(_) => {}
                    Err(error) => issues.push(Issue::new(
                        at,
                        format!(
                            "cannot read service-account file {}: {}",
                            path.display(),
                            error.kind()
                        ),
                    )),
                }
            }
        }
        for (at, secret) in self.secret_refs() {
            if let Some(name) = &secret.env {
                match env(name) {
                    Some(value) if !value.trim().is_empty() => {}
                    Some(_) => issues.push(Issue::new(
                        at,
                        format!("environment variable {name} is empty"),
                    )),
                    None => issues.push(Issue::new(
                        at,
                        format!("environment variable {name} is not set"),
                    )),
                }
            } else if let Some(path) = &secret.file {
                match std::fs::read_to_string(path) {
                    Ok(value) if !value.trim().is_empty() => {}
                    Ok(_) => issues.push(Issue::new(
                        at,
                        format!("secret file {} is empty", path.display()),
                    )),
                    Err(error) => issues.push(Issue::new(
                        at,
                        format!(
                            "cannot read secret file {}: {}",
                            path.display(),
                            error.kind()
                        ),
                    )),
                }
            }
        }
        issues
    }

    /// What is wrong with a service-account file that can be read: the transport's
    /// own rules (type, token endpoint, a usable key), by path, never the content.
    /// A file that cannot be read is [`Self::check_secrets_present`]'s to report.
    pub fn check_credential_files(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        for (name, provider) in &self.providers {
            let Some(path) = &provider.service_account_file else {
                continue;
            };
            let Ok(bytes) = crate::providers::cloud_rules::read_service_account(path) else {
                continue;
            };
            if bytes.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if let Some(problem) = crate::providers::cloud_rules::service_account_problem(&bytes) {
                issues.push(Issue::new(
                    format!("providers.{name}.service_account_file"),
                    format!("the file {problem}"),
                ));
            }
        }
        issues
    }

    /// The rules that compare resolved secret values: a caller token must not
    /// equal an upstream key, another caller's token, or the admin token. A
    /// secret that cannot be read is skipped here ([`Self::check_references`]
    /// reports it), so a reload can ask about the caller keys without being
    /// blocked by an unrelated secret that is unreadable at that moment.
    pub fn check_same_values(&self, env: &dyn Fn(&str) -> Option<String>) -> Vec<Issue> {
        let mut issues = Vec::new();
        let read = |secret: &SecretRef| read_secret(secret, env).ok();
        // Two references can name the same value. A caller token equal to an
        // upstream key would make that upstream key a valid bearer token.
        // Compared as digests, so a `sha256` key is held to the same rule as a
        // `secret` key.
        let digest = |value: &str| format!("{:x}", Sha256::digest(value.as_bytes()));
        let upstream: Vec<String> = self
            .providers
            .values()
            .filter_map(|provider| provider.api_key.as_ref())
            .filter_map(read)
            .map(|value| digest(&value))
            .collect();
        let mut callers = BTreeSet::new();
        for (index, key) in self.keys.iter().enumerate() {
            let (at, caller) = match (&key.secret, &key.sha256) {
                (Some(secret), None) => match read(secret) {
                    Some(value) => {
                        // With a token issuer configured, a token-shaped bearer
                        // value is only ever verified as a token (0017), so a
                        // key written like one could never authenticate.
                        if !self.auth.jwt.is_empty() && super::jwt::is_jwt_shaped(value.trim()) {
                            issues.push(Issue::new(
                                format!("keys[{index}].secret"),
                                "looks like a JWT; with [[auth.jwt]] configured a token-shaped bearer value is verified as a token and never looked up as a key",
                            ));
                        }
                        (format!("keys[{index}].secret"), digest(&value))
                    }
                    None => continue,
                },
                (None, Some(sha256)) => (format!("keys[{index}].sha256"), sha256.clone()),
                _ => continue,
            };
            if upstream.contains(&caller) {
                issues.push(Issue::new(
                    &at,
                    "has the same value as a provider's api_key",
                ));
            }
            if !callers.insert(caller) {
                issues.push(Issue::new(&at, "has the same value as another caller key"));
            }
        }
        if let Some(value) = self
            .admin
            .as_ref()
            .and_then(|admin| admin.token.as_ref())
            .and_then(read)
            .map(|value| digest(&value))
        {
            // Both sets hold digests, so this holds for `sha256` caller keys too.
            if upstream.contains(&value) || callers.contains(&value) {
                issues.push(Issue::new(
                    "admin.token",
                    "has the same value as a provider key or a caller key",
                ));
            }
        }
        issues
    }
}

/// The Redis URL schemes fleet mode accepts.
pub(crate) fn is_redis_url(url: &str) -> bool {
    (url.starts_with("redis://") || url.starts_with("rediss://"))
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Resolve a secret reference to its value, trimmed. The error names the
/// variable or file, never a value.
pub(crate) fn read_secret(
    secret: &SecretRef,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let value = match (&secret.env, &secret.file) {
        (Some(name), _) => {
            env(name).ok_or_else(|| format!("environment variable {name} is not set"))?
        }
        (None, Some(path)) => std::fs::read_to_string(path).map_err(|error| {
            format!(
                "cannot read secret file {}: {}",
                path.display(),
                error.kind()
            )
        })?,
        (None, None) => return Err("no secret reference".into()),
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(match (&secret.env, &secret.file) {
            (Some(name), _) => format!("environment variable {name} is empty"),
            (None, Some(path)) => format!("secret file {} is empty", path.display()),
            (None, None) => unreachable!("handled above"),
        });
    }
    Ok(value.to_string())
}

/// Line and column (1-based) of a byte offset.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text.as_bytes()[..offset];
    let line = before.iter().filter(|b| **b == b'\n').count() + 1;
    let column = offset
        - before
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |i| i + 1)
        + 1;
    (line, column)
}

/// Turn a parser message into one that is safe to print.
///
/// serde formats a value of the wrong kind as `invalid type: string "<value>",
/// expected ...`, and an unknown enum value as ``unknown variant `<value>` ``,
/// so a secret pasted where a table or a name was expected would be echoed.
/// Those two shapes keep only what the format expected; every other shape
/// names a key, a count or a fixed phrase, and is kept.
fn describe(raw: &str, at: Option<(usize, usize)>) -> String {
    let after = |marker: &str| raw.rfind(marker).map(|i| &raw[i + marker.len()..]);
    let message = if raw.starts_with("unknown variant `") {
        match after("`, expected ") {
            Some(expected) => format!("unknown value, expected {expected}"),
            None => "unknown value".to_string(),
        }
    } else if raw.starts_with("invalid type: ") || raw.starts_with("invalid value: ") {
        match after(", expected ") {
            Some(expected) => format!("wrong kind of value (not shown), expected {expected}"),
            None => "wrong kind of value (not shown)".to_string(),
        }
    } else {
        raw.to_string()
    };
    match at {
        Some((line, column)) => format!("{message} (line {line}, column {column})"),
        None => message,
    }
}

/// `version` read on its own, so that a file of another version is reported as
/// such whatever else it contains.
fn unsupported_version(text: &str, format: Format) -> Option<Issue> {
    #[derive(Deserialize)]
    struct Probe {
        version: Option<i64>,
    }
    let probe: Probe = match format {
        Format::Toml => toml::from_str(text).ok()?,
        Format::Json => serde_json::from_str(text).ok()?,
    };
    let version = probe.version?;
    (version != i64::from(VERSION)).then(|| {
        Issue::new(
            "version",
            format!("unsupported version {version}; this build reads version {VERSION}"),
        )
    })
}

const NAME_RULE: &str =
    "names use 1 to 64 lower-case letters, digits, '-' or '_', starting with a letter or digit";

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}

/// `dns:<name>` or `uri:<uri>`, with something after the prefix and no
/// whitespace or control characters.
pub fn valid_client_san(san: &str) -> bool {
    let rest = san
        .strip_prefix("dns:")
        .or_else(|| san.strip_prefix("uri:"));
    // ASCII only: the name travels in an HTTP header, which cannot carry
    // anything else, so a non-ASCII name could never match.
    rest.is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii() && !c.is_whitespace() && !c.is_control())
    })
}

/// DNS names compare case-insensitively; URIs exactly.
pub(crate) fn canonical_san(san: &str) -> String {
    match san.strip_prefix("dns:") {
        Some(name) => format!("dns:{}", name.to_ascii_lowercase()),
        None => san.to_string(),
    }
}

const TENANT_RULE: &str = "tenants use 1 to 64 letters, digits, '-', '_' or '.'";

/// Tenants become parts of ledger keys and metric labels, so separators such
/// as ':' and whitespace are not allowed.
fn valid_tenant(tenant: &str) -> bool {
    (1..=64).contains(&tenant.len())
        && tenant
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn validate_secret(secret: &SecretRef, at: &str, issues: &mut Vec<Issue>) {
    match (&secret.env, &secret.file) {
        (Some(_), Some(_)) => issues.push(Issue::new(at, "set either env or file, not both")),
        (None, None) => issues.push(Issue::new(at, "set env or file")),
        (Some(name), None) => {
            // Upper-case only: a token pasted here by mistake almost always
            // has a lower-case letter or punctuation, and is then refused
            // without being printed.
            let valid = !name.is_empty()
                && name.len() <= 128
                && !name.as_bytes()[0].is_ascii_digit()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
            if !valid {
                issues.push(Issue::new(
                    format!("{at}.env"),
                    "must be an environment variable name: upper-case letters, digits and '_'",
                ));
            }
        }
        (None, Some(path)) => {
            // `has_root` rather than `is_absolute`, so a Unix-style path in a
            // shared file is judged the same on every platform.
            if !path.has_root() {
                issues.push(Issue::new(format!("{at}.file"), "must be an absolute path"));
            }
        }
    }
}

/// A base URL the client can use as given: redirects are disabled, so it must
/// already be the final endpoint, and it must not smuggle credentials or a
/// query string.
fn validate_base_url(url: &str) -> Result<(), &'static str> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("must start with https:// or http://");
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("must not contain whitespace");
    }
    if url.contains('?') || url.contains('#') {
        return Err("must not contain a query string or fragment");
    }
    // The authority is read from the text, before the parser sees it: the parser
    // discards extra slashes after the scheme, so `https:///v1` would otherwise
    // become the host `v1` and the provider key would be sent there, and it drops
    // an empty `@`, so `https://@host` would pass as credential-free.
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '\\']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err("must name a host");
    }
    if authority.contains('@') {
        return Err("must not contain credentials; use api_key");
    }
    // A real parser, not a split on '/': it refuses an empty host (`http://:8000`),
    // a port that is not a number or is out of range, and a malformed address.
    let parsed =
        reqwest::Url::parse(url).map_err(|_| "is not a valid URL (check the host and port)")?;
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("must name a host");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("must not contain credentials; use api_key");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        version = 1
        [providers.openai]
        kind = "openai"
        api_key = { env = "OPENAI_API_KEY" }
        [[keys]]
        name = "ci"
        tenant = "platform"
        secret = { env = "CI_GATEWAY_KEY" }
    "#;

    #[test]
    fn reload_watches_every_thirty_seconds_unless_told_otherwise() {
        let config = ServeConfig::parse(MINIMAL, Format::Toml).unwrap();
        assert!(config.reload.watch);
        assert_eq!(
            config.reload.poll_interval(),
            Some(std::time::Duration::from_secs(30))
        );
        let tuned = format!("{MINIMAL}\n[reload]\ninterval_secs = 5");
        let tuned = ServeConfig::parse(&tuned, Format::Toml).unwrap();
        assert_eq!(
            tuned.reload.poll_interval(),
            Some(std::time::Duration::from_secs(5))
        );
        let off = format!("{MINIMAL}\n[reload]\nwatch = false");
        let off = ServeConfig::parse(&off, Format::Toml).unwrap();
        assert_eq!(off.reload.poll_interval(), None);
    }

    #[test]
    fn reload_settings_are_validated_and_closed() {
        for bad in ["0", "90000"] {
            let found = issues(&format!("{MINIMAL}\n[reload]\ninterval_secs = {bad}"));
            assert!(has(&found, "reload.interval_secs"), "{found:?}");
        }
        let found = issues(&format!("{MINIMAL}\n[reload]\nsignal = true"));
        assert!(!found.is_empty(), "an unknown reload setting is refused");
    }

    fn issues(text: &str) -> Vec<String> {
        ServeConfig::parse(text, Format::Toml)
            .err()
            .unwrap_or_default()
            .iter()
            .map(Issue::to_string)
            .collect()
    }

    fn has(found: &[String], path: &str) -> bool {
        found
            .iter()
            .any(|issue| issue.starts_with(&format!("{path}:")))
    }

    const CONTROL_PLANE: &str = r#"
        version = 1
        [providers.openai]
        kind = "openai"
        api_key = { env = "OPENAI_API_KEY" }
        [state]
        policy_dir = "/var/lib/llmshim/policy"
        [control_plane]
        org = "org_test"
        verify_keys = ["ed25519:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaA="]
        snapshot_file = "/var/lib/llmshim/snapshot.json"
    "#;

    #[test]
    fn a_control_plane_replaces_keys_and_has_its_own_rules() {
        // Valid without [[keys]]: the snapshot is the source of who may call (0024).
        assert!(
            issues(CONTROL_PLANE).is_empty(),
            "{:?}",
            issues(CONTROL_PLANE)
        );
        // Never both: two sources of truth.
        let both = format!(
            "{CONTROL_PLANE}\n[[keys]]\nname = \"ci\"\ntenant = \"t\"\nsecret = {{ env = \"K\" }}"
        );
        assert!(has(&issues(&both), "control_plane"));
        // The record of what was applied is required, or a restart could be rolled back.
        let no_dir = CONTROL_PLANE.replace("policy_dir = \"/var/lib/llmshim/policy\"", "");
        assert!(has(&issues(&no_dir), "state.policy_dir"));
        let relative = CONTROL_PLANE.replace("/var/lib/llmshim/policy", "policy");
        assert!(has(&issues(&relative), "state.policy_dir"));
        let relative_file =
            CONTROL_PLANE.replace("/var/lib/llmshim/snapshot.json", "snapshot.json");
        assert!(has(&issues(&relative_file), "control_plane.snapshot_file"));
        // A verify key that is not a 32-byte Ed25519 key is refused by position.
        let bad_key = CONTROL_PLANE.replace(
            "ed25519:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaA=",
            "ed25519:AAAA",
        );
        assert!(has(&issues(&bad_key), "control_plane.verify_keys[0]"));
        let none = CONTROL_PLANE.replace(
            "verify_keys = [\"ed25519:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaA=\"]",
            "verify_keys = []",
        );
        assert!(has(&issues(&none), "control_plane.verify_keys"));
        // Open serving cannot be combined with it.
        let open = format!("{CONTROL_PLANE}\n[server]\nallow_unauthenticated = true");
        assert!(has(&issues(&open), "server.allow_unauthenticated"));
        // Without a control plane the old rule stands: callers must be defined.
        let neither = MINIMAL.replace(
            "[[keys]]\n        name = \"ci\"\n        tenant = \"platform\"\n        secret = { env = \"CI_GATEWAY_KEY\" }",
            "",
        );
        assert!(has(&issues(&neither), "keys"));
    }

    #[test]
    fn the_documented_example_is_valid() {
        let example = include_str!("../../docs/design/enterprise-gateway/example.toml");
        let config = ServeConfig::parse(example, Format::Toml).unwrap();
        assert_eq!(config.providers.len(), 3);
        assert_eq!(config.keys.len(), 3);
    }

    fn schema_validator() -> jsonschema::Validator {
        jsonschema::validator_for(&schema()).expect("the schema compiles")
    }

    fn toml_as_json(text: &str) -> serde_json::Value {
        let value: toml::Value = toml::from_str(text).expect("valid TOML");
        serde_json::to_value(value).expect("TOML is JSON-shaped")
    }

    #[test]
    fn the_documented_example_satisfies_the_schema() {
        let example = include_str!("../../docs/design/enterprise-gateway/example.toml");
        let errors: Vec<String> = schema_validator()
            .iter_errors(&toml_as_json(example))
            .map(|error| format!("{} at {}", error, error.instance_path()))
            .collect();
        assert!(errors.is_empty(), "{errors:#?}");
        assert!(schema_validator().is_valid(&toml_as_json(MINIMAL)));
    }

    #[test]
    fn the_schema_refuses_what_the_parser_refuses_by_shape() {
        let validator = schema_validator();
        for (what, text) in [
            (
                // Before any table, so it is a root key and not keys[0]'s.
                "an unknown top-level field",
                format!("surprise = 1\n{MINIMAL}"),
            ),
            (
                "an unknown nested field",
                format!("{MINIMAL}\n[server]\ncolour = \"red\""),
            ),
            (
                "another version",
                MINIMAL.replace("version = 1", "version = 2"),
            ),
            (
                "deployments as a list",
                format!(
                    "{MINIMAL}\n{}",
                    AZURE.replace("{ \"chat-prod\" = \"gpt-6-sol\" }", "[\"chat-prod\"]")
                ),
            ),
            (
                "a region that is a number",
                format!("{MINIMAL}\n{}", BEDROCK.replace("\"us-east-1\"", "7")),
            ),
            (
                "an unknown provider kind",
                format!(
                    "{MINIMAL}\n{}",
                    BEDROCK.replace("\"bedrock\"\nprofile", "\"bedrock2\"\nprofile")
                ),
            ),
            (
                "an unknown cloud field",
                format!("{MINIMAL}\n{BEDROCK}access_key = \"x\"\n"),
            ),
            ("no version", MINIMAL.replace("version = 1", "")),
            (
                "an inline secret",
                MINIMAL.replace("{ env = \"OPENAI_API_KEY\" }", "\"sk-pasted-inline\""),
            ),
            (
                "a secret naming both an env var and a file",
                MINIMAL.replace(
                    "{ env = \"OPENAI_API_KEY\" }",
                    "{ env = \"OPENAI_API_KEY\", file = \"/run/key\" }",
                ),
            ),
            (
                "a port that does not fit",
                format!("{MINIMAL}\n[server]\nport = 70000"),
            ),
            (
                "a provider kind that does not exist",
                MINIMAL.replace("kind = \"openai\"", "kind = \"nope\""),
            ),
            (
                "a key with no tenant",
                MINIMAL.replace("tenant = \"platform\"", ""),
            ),
        ] {
            assert!(
                !validator.is_valid(&toml_as_json(&text)),
                "the schema must refuse {what}"
            );
            // And the parser agrees: no file the schema refuses parses.
            assert!(
                ServeConfig::parse(&text, Format::Toml).is_err(),
                "the parser must refuse {what} too"
            );
        }
    }

    #[test]
    fn every_file_the_parser_accepts_satisfies_the_schema() {
        let validator = schema_validator();
        for (what, extra) in [
            (
                "state",
                "[state]\nreceipts_dir = \"/var/lib/llmshim\"\nidempotency_ttl_secs = 60",
            ),
            (
                "logging",
                "[logging]\naccess_log = \"stdout\"\nusage_log = \"/var/log/usage.jsonl\"",
            ),
            (
                "limits",
                "[limits]\nrpm = 100\nmax_concurrency = 4\n[limits.providers.openai]\nrpm = 10",
            ),
            ("reload", "[reload]\nwatch = false"),
            (
                "telemetry",
                "[telemetry]\notlp_endpoint = \"http://127.0.0.1:4318\"\nsample_ratio = 0.5\n\
                 [telemetry.headers]\nauthorization = { file = \"/run/otlp\" }",
            ),
            (
                "admin",
                "[admin]\nport = 9090\ntoken = { env = \"ADMIN_TOKEN\" }",
            ),
            ("fleet", "[fleet]\nredis_url = { env = \"REDIS_URL\" }"),
            (
                "a route",
                "[routes.cheap]\nmodel = \"openai/gpt-6-luna\"\n[routes.cheap.settings]\nreasoning_effort = \"low\"",
            ),
            (
                "server tls",
                "[server.tls]\ncert_file = \"/etc/tls/server.pem\"\nkey_file = \"/etc/tls/server.key\"",
            ),
            ("an Azure provider", AZURE),
            ("a Bedrock provider", BEDROCK),
            (
                "a Bedrock provider with profile files",
                "[providers.bedrock]\nkind = \"bedrock\"\nprofile = \"p\"\nregion = \"eu-west-1\"\n\
                 config_file = \"/etc/aws/config\"\ncredentials_file = \"/etc/aws/credentials\"",
            ),
            ("a Vertex provider", VERTEX),
        ] {
            let text = format!("{MINIMAL}\n{extra}");
            assert!(
                ServeConfig::parse(&text, Format::Toml).is_ok(),
                "the fixture for {what} is meant to parse: {:?}",
                ServeConfig::parse(&text, Format::Toml).err()
            );
            let errors: Vec<String> = validator
                .iter_errors(&toml_as_json(&text))
                .map(|e| format!("{e} at {}", e.instance_path()))
                .collect();
            assert!(errors.is_empty(), "{what}: {errors:?}");
        }
    }

    #[test]
    fn a_secret_with_an_explicit_null_is_read_the_same_by_the_schema_and_the_parser() {
        // A JSON generator emits `"file": null` for an absent key. Both accept it
        // beside a value and both refuse it as the only content.
        let validator = schema_validator();
        let with = |secret: &str| {
            format!(
                r#"{{"version":1,"providers":{{"openai":{{"kind":"openai","api_key":{secret}}}}},
                "keys":[{{"name":"ci","tenant":"platform","secret":{{"env":"CI_GATEWAY_KEY"}}}}]}}"#
            )
        };
        for (secret, accepted) in [
            (r#"{"env":"OPENAI_API_KEY","file":null}"#, true),
            (r#"{"env":null,"file":"/run/key"}"#, true),
            (r#"{"env":null,"file":null}"#, false),
            (r#"{"env":"OPENAI_API_KEY","file":"/run/key"}"#, false),
        ] {
            let text = with(secret);
            let instance: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(validator.is_valid(&instance), accepted, "schema: {secret}");
            assert_eq!(
                ServeConfig::parse(&text, Format::Json).is_ok(),
                accepted,
                "parser: {secret}"
            );
        }
    }

    #[test]
    fn every_documented_top_level_table_is_in_the_schema() {
        // A table added to ServeConfig and left out of the schema would be
        // invisible to a consumer validating its files.
        let schema = schema();
        let properties = schema["properties"].as_object().unwrap();
        for table in [
            "version",
            "server",
            "providers",
            "limits",
            "state",
            "logging",
            "fleet",
            "admin",
            "keys",
            "routes",
            "reload",
            "telemetry",
            "auth",
            "upstream",
            "control_plane",
        ] {
            assert!(properties.contains_key(table), "{table}");
        }
        assert_eq!(properties.len(), 15, "a new table needs a line here");
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn a_minimal_file_loads_with_safe_defaults() {
        let config = ServeConfig::parse(MINIMAL, Format::Toml).unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 3000);
        assert!(!config.server.allow_unauthenticated);
        assert_eq!(config.providers["openai"].kind, ProviderKind::Openai);
        assert_eq!(config.keys[0].tier, 0);
    }

    #[test]
    fn json_and_toml_describe_the_same_file() {
        let json = r#"{"version":1,
            "providers":{"openai":{"kind":"openai","api_key":{"env":"OPENAI_API_KEY"}}},
            "keys":[{"name":"ci","tenant":"platform","secret":{"env":"CI_GATEWAY_KEY"}}]}"#;
        let from_json = ServeConfig::parse(json, Format::Json).unwrap();
        let from_toml = ServeConfig::parse(MINIMAL, Format::Toml).unwrap();
        assert_eq!(format!("{from_json:?}"), format!("{from_toml:?}"));
    }

    #[test]
    fn unknown_fields_are_errors_at_every_level() {
        // Before `version`, so it sits at the top level rather than inside the
        // last `[[keys]]` table.
        let at_top = format!("surprise = 1\n{MINIMAL}");
        let found = issues(&at_top);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("surprise"), "{found:?}");
        for extra in [
            "\n[server]\nsurprise = 1",
            "\n[limits]\nsurprise = 1",
            "\n[limits.providers.openai]\nsurprise = 1",
            "\n[routes.fast]\nmodel = \"openai/gpt-6-luna\"\nsurprise = 1",
        ] {
            let found = issues(&format!("{MINIMAL}{extra}"));
            assert_eq!(found.len(), 1, "{extra}: {found:?}");
            assert!(found[0].contains("surprise"), "{found:?}");
        }
        let in_provider = MINIMAL.replace("kind = \"openai\"", "kind = \"openai\"\nsurprise = 1");
        assert!(issues(&in_provider)[0].contains("surprise"));
        let in_key = MINIMAL.replace("name = \"ci\"", "name = \"ci\"\nsurprise = 1");
        assert!(issues(&in_key)[0].contains("surprise"));
        let in_secret = MINIMAL.replace(
            "{ env = \"OPENAI_API_KEY\" }",
            "{ env = \"OPENAI_API_KEY\", surprise = 1 }",
        );
        assert!(issues(&in_secret)[0].contains("surprise"));
    }

    #[test]
    fn a_value_in_the_wrong_place_is_never_echoed() {
        const TOKEN: &str = "tok-0123456789-do-not-print";
        let toml_files = [
            format!("version = 1\nkeys = [\"{TOKEN}\"]"),
            format!("version = 1\n[providers]\nopenai = \"{TOKEN}\""),
            format!("{MINIMAL}\n[providers.other]\nkind = \"{TOKEN}\""),
            "version = 1\n[providers.p]\nkind = \"openai\"\napi_key = 4929183".to_string(),
            format!("version = 1\n[server]\nport = \"{TOKEN}\""),
            format!("version = 1\n[server]\nhost = [\"{TOKEN}\"]"),
        ];
        let json_files = [
            format!(r#"{{"version":1,"keys":["{TOKEN}"]}}"#),
            format!(r#"{{"version":1,"providers":{{"openai":"{TOKEN}"}}}}"#),
            format!(r#"{{"version":1,"providers":{{"p":{{"kind":"{TOKEN}"}}}}}}"#),
            r#"{"version":1,"providers":{"p":{"kind":"openai","api_key":4929183}}}"#.to_string(),
            format!(r#"{{"version":1,"server":{{"port":"{TOKEN}"}}}}"#),
        ];
        for (format, files) in [
            (Format::Toml, &toml_files[..]),
            (Format::Json, &json_files[..]),
        ] {
            for text in files {
                let found = ServeConfig::parse(text, format).expect_err(text);
                let shown = found
                    .iter()
                    .map(Issue::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(!shown.contains(TOKEN), "{format:?}: {shown}");
                assert!(!shown.contains("4929183"), "{format:?}: {shown}");
                assert!(shown.contains("line"), "{format:?}: no position in {shown}");
            }
        }
    }

    #[test]
    fn an_unknown_enum_value_keeps_the_choices_and_drops_the_value() {
        let found = issues(&MINIMAL.replace("kind = \"openai\"", "kind = \"hunter2-secret\""));
        assert!(found[0].contains("expected one of"), "{found:?}");
        assert!(!found[0].contains("hunter2-secret"), "{found:?}");
    }

    #[test]
    fn control_characters_in_a_name_cannot_break_a_line() {
        let found = issues(&format!(
            "{MINIMAL}\n[routes.\"a\\nb\"]\nmodel = \"openai/x\""
        ));
        assert!(!found.is_empty());
        for issue in &found {
            assert!(!issue.contains('\n'), "{issue:?}");
        }
    }

    #[test]
    fn a_digest_key_cannot_equal_a_provider_key_or_another_callers_secret() {
        let digest_of = |value: &str| format!("{:x}", Sha256::digest(value.as_bytes()));
        let text = format!(
            r#"
            version = 1
            [providers.openai]
            kind = "openai"
            api_key = {{ env = "UPSTREAM" }}
            [[keys]]
            name = "a"
            tenant = "t"
            sha256 = "{upstream}"
            [[keys]]
            name = "b"
            tenant = "t"
            secret = {{ env = "CALLER" }}
            [[keys]]
            name = "c"
            tenant = "t"
            sha256 = "{caller}"
            "#,
            upstream = digest_of("upstream-key-value"),
            caller = digest_of("caller-token-value-0123"),
        );
        let config = ServeConfig::parse(&text, Format::Toml).unwrap();
        let env = |name: &str| match name {
            "UPSTREAM" => Some("upstream-key-value".to_string()),
            "CALLER" => Some("caller-token-value-0123".to_string()),
            _ => None,
        };
        let found: Vec<String> = config
            .check_references(&env)
            .iter()
            .map(Issue::to_string)
            .collect();
        assert!(has(&found, "keys[0].sha256"), "{found:?}");
        assert!(found.iter().any(|i| i.contains("provider's api_key")));
        assert!(has(&found, "keys[2].sha256"), "{found:?}");
        assert!(found.iter().any(|i| i.contains("another caller key")));
    }

    #[test]
    fn a_route_model_with_surrounding_whitespace_is_refused() {
        let found = issues(&format!(
            "{MINIMAL}\n[routes.fast]\nmodel = \" openai/gpt-6-luna\""
        ));
        assert!(has(&found, "routes.fast.model"), "{found:?}");
    }

    #[test]
    fn a_future_version_is_reported_as_such_even_with_fields_this_build_lacks() {
        let found = issues("version = 2\nsurprise = 1\n[new_table]\nx = 1");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("unsupported version 2"), "{found:?}");
        let json = ServeConfig::parse(r#"{"version":3,"surprise":1}"#, Format::Json)
            .err()
            .unwrap();
        assert!(
            json[0].message.contains("unsupported version 3"),
            "{json:?}"
        );
    }

    #[test]
    fn an_inline_secret_is_refused_and_never_echoed() {
        let inline = MINIMAL.replace("{ env = \"OPENAI_API_KEY\" }", "\"sk-live-do-not-print\"");
        let found = issues(&inline);
        assert_eq!(found.len(), 1);
        assert!(!found[0].contains("sk-live-do-not-print"), "{found:?}");
    }

    #[test]
    fn another_version_is_refused_before_anything_else() {
        let found = issues("version = 2\n[server]\nhost = \"not-an-ip\"");
        assert_eq!(found.len(), 1);
        assert!(has(&found, "version"));
    }

    #[test]
    fn callers_must_be_authenticated_unless_the_file_says_otherwise() {
        let no_keys = r#"
            version = 1
            [providers.openai]
            kind = "openai"
            api_key = { env = "OPENAI_API_KEY" }
        "#;
        assert!(has(&issues(no_keys), "keys"));
        let open = format!("{no_keys}\n[server]\nallow_unauthenticated = true");
        assert!(ServeConfig::parse(&open, Format::Toml).is_ok());
        let contradictory = format!("{MINIMAL}\n[server]\nallow_unauthenticated = true");
        assert!(has(&issues(&contradictory), "server.allow_unauthenticated"));
    }

    #[test]
    fn every_semantic_problem_is_reported_with_its_path() {
        let found = issues(
            r#"
            version = 1
            [server]
            host = "localhost"
            [providers.Bad_Name]
            kind = "anthropic"
            [providers.local]
            kind = "openai-compatible"
            [providers.route]
            kind = "openai"
            api_key = { env = "A", file = "/x" }
            [providers.hosted]
            kind = "openai"
            api_key = { file = "relative/path" }
            base_url = "https://user:pw@example.com/v1?x=1"
            [limits]
            max_concurrency = 0
            [limits.providers.missing]
            rpm = 1
            [[keys]]
            name = "a"
            tenant = " "
            [[keys]]
            name = "a"
            tenant = "t"
            sha256 = "ABC"
            budget_window_secs = 0
            [[keys]]
            name = "b"
            tenant = "t"
            secret = { env = "1BAD" }
            sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
            budget_usd = -1.0
            [routes.loop]
            model = "route/other"
            [routes.orphan]
            model = "nowhere/model"
            [routes.empty]
            model = ""
            "#,
        );
        for path in [
            "server.host",
            "providers.Bad_Name",
            "providers.Bad_Name.api_key",
            "providers.local.base_url",
            "providers.route",
            "providers.route.api_key",
            "providers.hosted.api_key.file",
            "providers.hosted.base_url",
            "limits.max_concurrency",
            "limits.providers.missing",
            "keys[0].tenant",
            "keys[0]",
            "keys[1].name",
            "keys[1].sha256",
            "keys[1].budget_window_secs",
            "keys[1]",
            "keys[2]",
            "keys[2].budget_usd",
            "routes.loop.model",
            "routes.orphan.model",
            "routes.empty.model",
        ] {
            assert!(has(&found, path), "missing {path} in {found:#?}");
        }
    }

    #[test]
    fn json_duplicate_names_are_refused_instead_of_last_one_winning() {
        let json = r#"{"version":1,"server":{"allow_unauthenticated":true},
            "providers":{"openai":{"kind":"openai","api_key":{"env":"A"}},
                         "openai":{"kind":"anthropic","api_key":{"env":"B"}}}}"#;
        let found = ServeConfig::parse(json, Format::Json).unwrap_err();
        assert!(
            found[0].message.contains("duplicate name `openai`"),
            "{found:?}"
        );
    }

    #[test]
    fn allowed_cidrs_and_trusted_proxies_are_validated_by_path() {
        let key = |extra: &str| {
            format!(
                "{MINIMAL}\n[[keys]]\nname = \"net\"\ntenant = \"t\"\nsha256 = \"{}\"\n{extra}",
                "0".repeat(64)
            )
        };
        let ok = key("allowed_cidrs = [\"10.0.0.0/8\", \"2001:db8::/32\", \"192.0.2.7\"]");
        assert!(ServeConfig::parse(&ok, Format::Toml).is_ok());
        for (extra, path) in [
            ("allowed_cidrs = []", "keys[1].allowed_cidrs"),
            (
                "allowed_cidrs = [\"10.1.2.3/8\"]",
                "keys[1].allowed_cidrs[0]",
            ),
            (
                "allowed_cidrs = [\"10.0.0.0/8\", \"nope\"]",
                "keys[1].allowed_cidrs[1]",
            ),
            (
                "allowed_cidrs = [\"::ffff:10.0.0.0/104\"]",
                "keys[1].allowed_cidrs[0]",
            ),
        ] {
            assert!(
                has(&issues(&key(extra)), path),
                "{extra}: {:?}",
                issues(&key(extra))
            );
        }
        let proxies = |list: &str| {
            MINIMAL.replacen(
                "[providers.openai]",
                &format!("[server]\ntrusted_proxies = {list}\n[providers.openai]"),
                1,
            )
        };
        assert!(ServeConfig::parse(&proxies("[\"10.0.0.0/8\"]"), Format::Toml).is_ok());
        assert!(has(
            &issues(&proxies("[\"bad\"]")),
            "server.trusted_proxies[0]"
        ));
    }

    #[test]
    fn jwt_issuers_and_bindings_are_validated_by_path() {
        let table = |extra: &str| {
            format!(
                "[[auth.jwt]]\nname = \"idp\"\nissuer = \"https://idp.example\"\n\
                 jwks_url = \"https://idp.example/keys\"\naudience = \"llmshim\"\n\
                 algorithms = [\"RS256\", \"EdDSA\"]\n{extra}\n"
            )
        };
        let bound = "[[keys]]\nname = \"wl\"\ntenant = \"t\"\n\
                     jwt = { table = \"idp\", subject = \"svc:a\" }\n";
        let ok = format!("{MINIMAL}\n{}\n{bound}", table(""));
        assert!(
            ServeConfig::parse(&ok, Format::Toml).is_ok(),
            "{:?}",
            issues(&ok)
        );
        for (text, path) in [
            (
                table("clock_skew_secs = 301"),
                "auth.jwt[0].clock_skew_secs",
            ),
            (
                table("jwks_refresh_secs = 5"),
                "auth.jwt[0].jwks_refresh_secs",
            ),
            (
                table("jwks_max_stale_secs = 999999999"),
                "auth.jwt[0].jwks_max_stale_secs",
            ),
            (table("ca_file = \"relative.pem\""), "auth.jwt[0].ca_file"),
            (
                table("").replace("\"RS256\", \"EdDSA\"", "\"HS256\""),
                "auth.jwt[0].algorithms[0]",
            ),
            (
                table("").replace("\"RS256\", \"EdDSA\"", "\"RS256\", \"RS256\""),
                "auth.jwt[0].algorithms[1]",
            ),
            (
                table("").replace("\"RS256\", \"EdDSA\"", ""),
                "auth.jwt[0].algorithms",
            ),
            (
                table("").replace("https://idp.example/keys", "http://idp.example/keys"),
                "auth.jwt[0].jwks_url",
            ),
            (
                table("") + &table("").replace("name = \"idp\"", "name = \"idp2\""),
                "auth.jwt[1].issuer",
            ),
            (
                table("")
                    + &table("").replace("issuer = \"https://idp.example\"", "issuer = \"x\""),
                "auth.jwt[1].name",
            ),
        ] {
            let text = format!("{MINIMAL}\n{text}\n{bound}");
            assert!(has(&issues(&text), path), "{path}: {:?}", issues(&text));
        }
        // Bindings: an unknown table, a duplicate, both a secret and a binding.
        let unknown = format!(
            "{MINIMAL}\n{}\n{}",
            table(""),
            bound.replace("\"idp\"", "\"nope\"")
        );
        assert!(has(&issues(&unknown), "keys[1].jwt.table"));
        let twice = format!(
            "{MINIMAL}\n{}\n{bound}\n{}",
            table(""),
            bound.replace("\"wl\"", "\"wl2\"")
        );
        assert!(has(&issues(&twice), "keys[2].jwt"), "{:?}", issues(&twice));
        let both = format!(
            "{MINIMAL}\n{}\n{}sha256 = \"{}\"\n",
            table(""),
            bound,
            "0".repeat(64)
        );
        assert!(has(&issues(&both), "keys[1]"), "{:?}", issues(&both));
    }

    #[test]
    fn a_token_shaped_secret_is_refused_only_when_an_issuer_exists() {
        use base64::Engine as _;
        let enc = |text: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text);
        let shaped = format!("{}.{}.c2ln", enc("{\"alg\":\"EdDSA\"}"), enc("{}"));
        let env = |name: &str| (name == "CI_GATEWAY_KEY").then(|| shaped.clone());
        let plain = ServeConfig::parse(MINIMAL, Format::Toml).unwrap();
        assert!(
            plain.check_same_values(&env).is_empty(),
            "no issuer, no rule"
        );
        let with = format!(
            "{MINIMAL}\n[[auth.jwt]]\nname = \"idp\"\nissuer = \"i\"\n\
             jwks_url = \"https://idp.example/keys\"\naudience = \"a\"\nalgorithms = [\"EdDSA\"]\n"
        );
        let config = ServeConfig::parse(&with, Format::Toml).unwrap();
        let found: Vec<String> = config
            .check_same_values(&env)
            .iter()
            .map(Issue::to_string)
            .collect();
        assert!(has(&found, "keys[0].secret"), "{found:?}");
    }

    fn cloud(extra: &str) -> String {
        format!("{MINIMAL}\n{extra}\n")
    }

    const AZURE: &str =
        "[providers.azure]\nkind = \"azure\"\nendpoint = \"https://res.openai.azure.com\"\n\
        binding = \"azure-prod\"\napi_key = { env = \"AZ_KEY\" }\n\
        deployments = { \"chat-prod\" = \"gpt-6-sol\" }\n";
    const BEDROCK: &str =
        "[providers.bedrock]\nkind = \"bedrock\"\nprofile = \"prod\"\nregion = \"us-east-1\"\n";
    const VERTEX: &str =
        "[providers.vertex-gemini]\nkind = \"vertex-gemini\"\nproject = \"my-project\"\n\
        region = \"us-central1\"\nbinding = \"v-prod\"\nservice_account_file = \"/etc/sa.json\"\n";

    #[test]
    fn cloud_providers_are_accepted_and_validated_by_path() {
        for ok in [AZURE, BEDROCK, VERTEX] {
            assert!(
                ServeConfig::parse(&cloud(ok), Format::Toml).is_ok(),
                "{ok}: {:?}",
                issues(&cloud(ok))
            );
        }
        let both = cloud(&format!(
            "{VERTEX}{}",
            VERTEX.replace("vertex-gemini", "vertex-anthropic")
        ));
        assert!(
            ServeConfig::parse(&both, Format::Toml).is_ok(),
            "{:?}",
            issues(&both)
        );
        for (text, path) in [
            // a required field missing
            (BEDROCK.replace("profile = \"prod\"\n", ""), "providers.bedrock.profile"),
            (VERTEX.replace("binding = \"v-prod\"\n", ""), "providers.vertex-gemini.binding"),
            (AZURE.replace("deployments = { \"chat-prod\" = \"gpt-6-sol\" }\n", ""), "providers.azure.deployments"),
            (AZURE.replace("api_key = { env = \"AZ_KEY\" }\n", ""), "providers.azure.api_key"),
            // a value the transport would refuse on every request
            (BEDROCK.replace("us-east-1", "us-iso-east-1"), "providers.bedrock.region"),
            (BEDROCK.replace("us-east-1", "US_EAST"), "providers.bedrock.region"),
            (VERTEX.replace("my-project", "My_Project"), "providers.vertex-gemini.project"),
            (AZURE.replace("https://res", "http://res"), "providers.azure.endpoint"),
            (AZURE.replace("\"chat-prod\"", "\"bad name\""), "providers.azure.deployments.bad name"),
            (AZURE.replace("\"gpt-6-sol\"", "\" \""), "providers.azure.deployments.chat-prod"),
            (VERTEX.replace("/etc/sa.json", "sa.json"), "providers.vertex-gemini.service_account_file"),
            // another kind's field, and fields a cloud kind does not take
            (format!("{BEDROCK}project = \"p\"\n"), "providers.bedrock.project"),
            (format!("{AZURE}profile = \"p\"\n"), "providers.azure.profile"),
            (format!("{BEDROCK}api_key = {{ env = \"X\" }}\n"), "providers.bedrock.api_key"),
            (format!("{VERTEX}base_url = \"https://x.example\"\n"), "providers.vertex-gemini.base_url"),
            (format!("{BEDROCK}config_file = \"/etc/aws/config\"\n"), "providers.bedrock"),
            // names are reserved for their kinds
            (BEDROCK.replace("[providers.bedrock]", "[providers.aws]"), "providers.aws"),
            (
                "[providers.azure]\nkind = \"openai-compatible\"\nbase_url = \"https://x.example/v1\"\n".into(),
                "providers.azure",
            ),
        ] {
            let text = cloud(&text);
            assert!(has(&issues(&text), path), "{path}: {:?}", issues(&text));
        }
        // The two Vertex kinds share one identity.
        let split = cloud(&format!(
            "{VERTEX}{}",
            VERTEX
                .replace("vertex-gemini", "vertex-anthropic")
                .replace("v-prod", "other")
        ));
        assert!(
            has(&issues(&split), "providers.vertex-anthropic.binding"),
            "{:?}",
            issues(&split)
        );
    }

    #[test]
    fn more_cloud_rules_hold_by_path() {
        let second = |change: &dyn Fn(&str) -> String| {
            cloud(&format!(
                "{VERTEX}{}",
                change(&VERTEX.replace("vertex-gemini", "vertex-anthropic"))
            ))
        };
        for (field, text) in [
            (
                "service_account_file",
                second(&|v| v.replace("/etc/sa.json", "/etc/other.json")),
            ),
            (
                "project",
                second(&|v| v.replace("my-project", "other-project")),
            ),
            (
                "region",
                second(&|v| v.replace("us-central1", "europe-west4")),
            ),
        ] {
            assert!(
                has(
                    &issues(&text),
                    &format!("providers.vertex-anthropic.{field}")
                ),
                "{field}: {:?}",
                issues(&text)
            );
        }
        // Relative profile files, and one file without the other.
        let relative = BEDROCK.to_string()
            + "config_file = \"aws/config\"\ncredentials_file = \"aws/credentials\"\n";
        assert!(has(
            &issues(&cloud(&relative)),
            "providers.bedrock.config_file"
        ));
        assert!(has(
            &issues(&cloud(&relative)),
            "providers.bedrock.credentials_file"
        ));
        // The Vertex locations the transport maps to their own hosts.
        for region in ["global", "us", "eu", "europe-west4"] {
            let text = cloud(&VERTEX.replace("us-central1", region));
            assert!(
                ServeConfig::parse(&text, Format::Toml).is_ok(),
                "{region}: {:?}",
                issues(&text)
            );
        }
    }

    #[test]
    fn a_service_account_path_that_is_not_a_plain_small_file_is_never_read_whole() {
        let dir = std::env::temp_dir().join(format!("llmshim-sa-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = |path: &std::path::Path| {
            ServeConfig::parse(
                &cloud(&VERTEX.replace("/etc/sa.json", path.to_str().unwrap())),
                Format::Toml,
            )
            .unwrap()
        };
        // Over a megabyte is a defect, and is read only as far as the limit.
        let big = dir.join("big.json");
        std::fs::write(&big, vec![b'x'; 2 * 1024 * 1024 + 7]).unwrap();
        let found = config(&big).check_credential_files();
        assert!(
            found.iter().any(|i| i.to_string().contains("larger")),
            "{found:?}"
        );
        // A directory is not a file, and does not hang or read.
        let absent: Vec<String> = config(&dir)
            .check_secrets_present(&|_| None)
            .iter()
            .map(Issue::to_string)
            .filter(|i| i.contains("service_account_file"))
            .collect();
        assert_eq!(absent.len(), 1, "{absent:?}");
        // A FIFO with no writer would block `open` forever: the check must return.
        #[cfg(unix)]
        {
            let fifo = dir.join("fifo.json");
            let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            let (sender, receiver) = std::sync::mpsc::channel();
            let probe = config(&fifo);
            std::thread::spawn(move || {
                let _ = sender.send(probe.check_secrets_present(&|_| None));
            });
            let answer = receiver
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("a FIFO must not hang the check");
            assert!(answer
                .iter()
                .any(|i| i.to_string().contains("service_account_file")));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cloud_kinds_need_the_serve_cloud_build() {
        let config = ServeConfig::parse(&cloud(BEDROCK), Format::Toml).unwrap();
        let found: Vec<String> = config
            .feature_issues()
            .iter()
            .map(Issue::to_string)
            .collect();
        assert_eq!(found.is_empty(), cfg!(feature = "serve-cloud"), "{found:?}");
        if !cfg!(feature = "serve-cloud") {
            assert!(has(&found, "providers.bedrock"));
            assert!(found[0].contains("--features serve-cloud"), "{found:?}");
        }
        // A file with no cloud kind is unaffected either way.
        assert!(ServeConfig::parse(MINIMAL, Format::Toml)
            .unwrap()
            .feature_issues()
            .iter()
            .all(|i| !i.to_string().contains("serve-cloud")));
    }

    #[test]
    fn a_service_account_file_is_checked_without_echoing_it() {
        let dir = std::env::temp_dir().join(format!("llmshim-sa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            include_str!("jwt_test_rsa_2048.b64").trim()
        );
        let account = |token_uri: &str, kind: &str| {
            serde_json::json!({"type": kind, "client_email": "sa@p.iam.example",
                "private_key": pem, "token_uri": token_uri, "private_key_id": "k"})
            .to_string()
        };
        let write = |name: &str, content: String| {
            let path = dir.join(name);
            std::fs::write(&path, content).unwrap();
            path
        };
        let config = |path: &std::path::Path| {
            ServeConfig::parse(
                &cloud(&VERTEX.replace("/etc/sa.json", path.to_str().unwrap())),
                Format::Toml,
            )
            .unwrap()
        };
        // Only the service-account findings: MINIMAL's own secrets are unset here.
        let absent = |config: &ServeConfig| -> Vec<String> {
            config
                .check_secrets_present(&|_| None)
                .iter()
                .map(Issue::to_string)
                .filter(|issue| issue.contains("service_account_file"))
                .collect()
        };
        let good = write(
            "good.json",
            account(
                crate::providers::cloud_rules::GOOGLE_TOKEN_URI,
                "service_account",
            ),
        );
        assert!(absent(&config(&good)).is_empty());
        assert!(config(&good).check_credential_files().is_empty());
        // Missing and empty are absences (status 3); malformed is a defect (status 1).
        let missing = dir.join("nope.json");
        let found = absent(&config(&missing));
        assert!(
            has(&found, "providers.vertex-gemini.service_account_file"),
            "{found:?}"
        );
        let empty = write("empty.json", " \n".into());
        assert_eq!(absent(&config(&empty)).len(), 1);
        assert!(
            config(&empty).check_credential_files().is_empty(),
            "absence is not a defect"
        );
        for (name, content) in [
            ("junk.json", "not json".to_string()),
            (
                "kind.json",
                account(
                    crate::providers::cloud_rules::GOOGLE_TOKEN_URI,
                    "authorized_user",
                ),
            ),
            (
                "uri.json",
                account("https://evil.example/token", "service_account"),
            ),
            (
                "key.json",
                account(
                    crate::providers::cloud_rules::GOOGLE_TOKEN_URI,
                    "service_account",
                )
                .replace("BEGIN PRIVATE KEY", "BEGIN WRONG"),
            ),
        ] {
            let path = write(name, content);
            let found: Vec<String> = config(&path)
                .check_credential_files()
                .iter()
                .map(Issue::to_string)
                .collect();
            assert_eq!(found.len(), 1, "{name}: {found:?}");
            assert!(
                has(&found, "providers.vertex-gemini.service_account_file"),
                "{name}"
            );
            // The content is never in the message.
            assert!(
                !found[0].contains("sa@p.iam") && !found[0].contains("BEGIN"),
                "{name}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_upstream_table_is_the_librarys_defaults_with_a_pool_that_follows_the_concurrency() {
        use crate::client::ClientSettings;
        let library = ClientSettings::default();
        let empty = Upstream::default().settings(None).unwrap();
        // Every field but the pool is the library's own, so this cannot drift.
        let mut expected = library;
        expected.pool_max_idle_per_host = 256;
        assert_eq!(empty, expected);
        // The pool follows the concurrency served, floored at 256 and capped at 4096.
        for (concurrency, pool) in [
            (None, 256),
            (Some(10), 256),
            (Some(1000), 1000),
            (Some(100_000), 4096),
        ] {
            assert_eq!(
                Upstream::default()
                    .settings(concurrency)
                    .unwrap()
                    .pool_max_idle_per_host,
                pool,
                "{concurrency:?}"
            );
        }
        // A value the file sets is used as written, even below the concurrency.
        let set = Upstream {
            pool_max_idle_per_host: Some(8),
            ..Upstream::default()
        };
        assert_eq!(set.settings(Some(1000)).unwrap().pool_max_idle_per_host, 8);
    }

    #[test]
    fn upstream_and_listener_keys_are_validated_by_path() {
        let with = |upstream: &str, server: &str| {
            let text = MINIMAL.replacen(
                "[providers.openai]",
                &format!("[server]\n{server}\n[upstream]\n{upstream}\n[providers.openai]"),
                1,
            );
            text
        };
        assert!(ServeConfig::parse(
            &with("max_retries = 10", "max_connections = 65535"),
            Format::Toml
        )
        .is_ok());
        for (upstream, server, path) in [
            ("max_retries = 11", "", "upstream.max_retries"),
            ("max_backoff_secs = 0", "", "upstream.max_backoff_secs"),
            ("max_backoff_secs = 301", "", "upstream.max_backoff_secs"),
            (
                "pool_max_idle_per_host = 0",
                "",
                "upstream.pool_max_idle_per_host",
            ),
            (
                "pool_max_idle_per_host = 4097",
                "",
                "upstream.pool_max_idle_per_host",
            ),
            (
                "pool_idle_timeout_secs = 3601",
                "",
                "upstream.pool_idle_timeout_secs",
            ),
            ("connect_timeout_ms = 0", "", "upstream.connect_timeout_ms"),
            ("", "max_connections = 0", "server.max_connections"),
            ("", "max_connections = 65536", "server.max_connections"),
            ("", "header_timeout_ms = 0", "server.header_timeout_ms"),
            ("", "header_timeout_ms = 600001", "server.header_timeout_ms"),
            // A deadline past the one that bounds it could never fire.
            (
                "connect_timeout_ms = 3600000",
                "",
                "upstream.connect_timeout_ms",
            ),
            (
                "response_header_timeout_ms = 1900000",
                "",
                "upstream.response_header_timeout_ms",
            ),
            (
                "unary_idle_timeout_ms = 2000000",
                "",
                "upstream.unary_idle_timeout_ms",
            ),
            (
                "stream_idle_timeout_ms = 8000000",
                "",
                "upstream.stream_idle_timeout_ms",
            ),
            (
                "error_body_idle_timeout_ms = 40000",
                "",
                "upstream.error_body_idle_timeout_ms",
            ),
        ] {
            let text = with(upstream, server);
            assert!(
                has(&issues(&text), path),
                "{upstream} {server}: {:?}",
                issues(&text)
            );
        }
        // An unknown key is refused (the parser names it; it has no path of its own).
        assert!(!issues(&with("unknown_key = 1", "")).is_empty());
        // The ordering uses the effective values, defaults included: shortening
        // the attempt below the default header deadline is the same mistake.
        // and the issue lands on the key the file wrote, not on a default.
        let text = with("unary_attempt_timeout_ms = 1000", "");
        let found = issues(&text);
        assert!(
            has(&found, "upstream.unary_attempt_timeout_ms"),
            "{found:?}"
        );
        assert!(!has(&found, "upstream.connect_timeout_ms"), "{found:?}");
        assert!(
            !has(&found, "upstream.response_header_timeout_ms"),
            "{found:?}"
        );
    }

    #[test]
    fn routes_must_name_a_defined_provider() {
        for (model, ok) in [
            ("openai/gpt-6-luna", true),
            ("openai/vendor/model", true),
            ("gpt-6-luna", false),
            ("missing/gpt-6-luna", false),
            ("openai/", false),
        ] {
            let text = format!("{MINIMAL}\n[routes.r]\nmodel = \"{model}\"");
            assert_eq!(
                ServeConfig::parse(&text, Format::Toml).is_ok(),
                ok,
                "{model}"
            );
        }
    }

    #[test]
    fn a_caller_key_cannot_be_an_upstream_key() {
        let same_reference = MINIMAL.replace("CI_GATEWAY_KEY", "OPENAI_API_KEY");
        assert!(has(&issues(&same_reference), "keys[0].secret"));
        let config = ServeConfig::parse(MINIMAL, Format::Toml).unwrap();
        let same_value = |_: &str| Some("identical-value".to_string());
        let found = config.check_references(&same_value);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "keys[0].secret");
        assert!(!found[0].message.contains("identical-value"));
    }

    #[test]
    fn tenants_and_variable_names_are_restricted() {
        for tenant in ["a:b", "has space", "", "tenant/x"] {
            let text = MINIMAL.replace("tenant = \"platform\"", &format!("tenant = \"{tenant}\""));
            assert!(has(&issues(&text), "keys[0].tenant"), "{tenant}");
        }
        assert!(
            ServeConfig::parse(&MINIMAL.replace("platform", "Team.A-1_x"), Format::Toml).is_ok()
        );
        let pasted = MINIMAL.replace("OPENAI_API_KEY", "sk_live_AbCdEf123");
        let found = issues(&pasted);
        assert!(has(&found, "providers.openai.api_key.env"), "{found:?}");
    }

    #[test]
    fn certificate_keys_are_validated() {
        let tls = "[server.tls]\ncert_file = \"/c\"\nkey_file = \"/k\"\nclient_ca_file = \"/ca\"\n";
        let key = |body: &str| format!("{MINIMAL}\n[[keys]]\nname = \"w\"\ntenant = \"t\"\n{body}");
        let with_tls = |body: &str| {
            format!("{MINIMAL}\n{tls}\n[[keys]]\nname = \"w\"\ntenant = \"t\"\n{body}")
        };
        // A workload key and a bearer key coexist.
        let ok = with_tls("client_san = \"uri:spiffe://corp/a/b\"");
        assert!(ServeConfig::parse(&ok, Format::Toml).is_ok());
        let dns = with_tls("client_san = \"dns:Service.Corp\"");
        assert!(ServeConfig::parse(&dns, Format::Toml).is_ok());
        for (body, path) in [
            ("client_san = \"spiffe://corp/a\"", "keys[1].client_san"),
            ("client_san = \"uri:\"", "keys[1].client_san"),
            ("client_san = \"dns:has space\"", "keys[1].client_san"),
            ("client_san = \"dns:x\"\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"", "keys[1]"),
        ] {
            let found = issues(&with_tls(body));
            assert!(has(&found, path), "{body}: {found:?}");
        }
        // Without required client certificates the key could never match.
        let found = issues(&key("client_san = \"uri:spiffe://corp/a/b\""));
        assert!(has(&found, "keys[1].client_san"), "{found:?}");
        // DNS names compare case-insensitively, so these collide; URIs are exact.
        let twice = with_tls("client_san = \"dns:Svc.Corp\"")
            + "\n[[keys]]\nname = \"x\"\ntenant = \"u\"\nclient_san = \"dns:svc.corp\"";
        assert!(has(&issues(&twice), "keys[2].client_san"));
        // Client certificates are for the main listener.
        let admin = format!(
            "{MINIMAL}\n[admin]\nport = 9090\n[admin.tls]\ncert_file = \"/c\"\nkey_file = \"/k\"\nclient_ca_file = \"/ca\""
        );
        assert!(has(&issues(&admin), "admin.tls.client_ca_file"));
    }

    #[test]
    fn the_log_targets_are_validated() {
        for field in ["access_log", "usage_log"] {
            for ok in ["stdout", "/var/log/llmshim/out.jsonl"] {
                let text = format!("{MINIMAL}\n[logging]\n{field} = \"{ok}\"");
                assert!(
                    ServeConfig::parse(&text, Format::Toml).is_ok(),
                    "{field} {ok}"
                );
            }
            for bad in ["stderr", "out.jsonl", "", "/var/log/dir/"] {
                let found = issues(&format!("{MINIMAL}\n[logging]\n{field} = \"{bad}\""));
                assert!(
                    has(&found, &format!("logging.{field}")),
                    "{field} {bad:?}: {found:?}"
                );
            }
        }
    }

    #[test]
    fn the_keys_of_one_tenant_must_agree_on_their_rate_limits() {
        let keys = |second: &str| {
            format!(
                "{MINIMAL}\n[[keys]]\nname = \"ci-two\"\ntenant = \"platform\"\n\
                 secret = {{ env = \"OTHER_KEY\" }}\n{second}"
            )
        };
        // MINIMAL's key has no limits; a second key of the tenant with one differs.
        let found = issues(&keys("rpm = 60"));
        assert!(has(&found, "keys[1].rpm"), "{found:?}");
        let found = issues(&keys("tpm = 1000"));
        assert!(has(&found, "keys[1].tpm"), "{found:?}");
        // Agreeing (here, both unlimited) is fine, and so are different tenants.
        assert!(ServeConfig::parse(&keys(""), Format::Toml).is_ok());
        let other_tenant = keys("rpm = 60").replace(
            "tenant = \"platform\"\nsecret",
            "tenant = \"other\"\nsecret",
        );
        assert!(ServeConfig::parse(&other_tenant, Format::Toml).is_ok());
    }

    #[test]
    fn two_logs_cannot_share_a_file_but_may_share_stdout() {
        let same_file = format!(
            "{MINIMAL}\n[logging]\naccess_log = \"/var/log/a.jsonl\"\nusage_log = \"/var/log/a.jsonl\""
        );
        let found = issues(&same_file);
        assert!(has(&found, "logging.usage_log"), "{found:?}");
        let both_stdout =
            format!("{MINIMAL}\n[logging]\naccess_log = \"stdout\"\nusage_log = \"stdout\"");
        assert!(ServeConfig::parse(&both_stdout, Format::Toml).is_ok());
        let two_files = format!(
            "{MINIMAL}\n[logging]\naccess_log = \"/var/log/a.jsonl\"\nusage_log = \"/var/log/u.jsonl\""
        );
        assert!(ServeConfig::parse(&two_files, Format::Toml).is_ok());
    }

    #[test]
    fn tls_settings_are_validated() {
        let ok = format!(
            "{MINIMAL}\n[server.tls]\ncert_file = \"/etc/tls/tls.crt\"\nkey_file = \"/etc/tls/tls.key\"\nreload_interval_secs = 5"
        );
        let config = ServeConfig::parse(&ok, Format::Toml).unwrap();
        assert_eq!(
            config.server.tls.as_ref().unwrap().reload_interval_secs,
            Some(5)
        );
        for (bad, path) in [
            (
                "cert_file = \"tls.crt\"\nkey_file = \"/k\"",
                "server.tls.cert_file",
            ),
            (
                "cert_file = \"/c\"\nkey_file = \"key\"",
                "server.tls.key_file",
            ),
            (
                "cert_file = \"/same\"\nkey_file = \"/same\"",
                "server.tls.key_file",
            ),
            (
                "cert_file = \"/c\"\nkey_file = \"/k\"\nreload_interval_secs = 0",
                "server.tls.reload_interval_secs",
            ),
            (
                "cert_file = \"/c\"\nkey_file = \"/k\"\nreload_interval_secs = 90000",
                "server.tls.reload_interval_secs",
            ),
        ] {
            let found = issues(&format!("{MINIMAL}\n[server.tls]\n{bad}"));
            assert!(has(&found, path), "{bad}: {found:?}");
        }
        // The admin listener has its own table, validated the same way.
        let admin = format!(
            "{MINIMAL}\n[admin]\nport = 9090\n[admin.tls]\ncert_file = \"rel\"\nkey_file = \"/k\""
        );
        assert!(has(&issues(&admin), "admin.tls.cert_file"));
        // A private key is never an inline value.
        let inline =
            format!("{MINIMAL}\n[server.tls]\ncert_file = \"/c\"\nkey_file = \"/k\"\nkey = \"x\"");
        assert!(issues(&inline)[0].contains("unknown field"));
    }

    #[test]
    fn fleet_mode_is_a_secret_reference_and_needs_its_feature() {
        let text = format!("{MINIMAL}\n[fleet]\nredis_url = {{ env = \"REDIS_URL\" }}");
        let config = ServeConfig::parse(&text, Format::Toml).unwrap();
        assert!(config.fleet.is_some());
        let inline = format!("{MINIMAL}\n[fleet]\nredis_url = \"redis://:pw@host\"");
        let found = issues(&inline);
        assert_eq!(found.len(), 1);
        assert!(!found[0].contains("pw@host"), "{found:?}");
        // The reference is resolved by --check like any other secret.
        let missing = config.check_references(&|name| (name != "REDIS_URL").then(|| "x".into()));
        assert!(
            missing.iter().any(|i| i.path == "fleet.redis_url"),
            "{missing:?}"
        );
        // A build without serve-redis reports the flag to rebuild with.
        let feature = config.feature_issues();
        assert_eq!(feature.is_empty(), cfg!(feature = "serve-redis"));
        if let Some(issue) = feature.first() {
            assert!(issue.message.contains("--features serve-redis"));
        }
        assert!(is_redis_url("redis://127.0.0.1/0") && is_redis_url("rediss://h:6380"));
        for bad in ["http://h", "redis://h /0", "host:6379", ""] {
            assert!(!is_redis_url(bad), "{bad}");
        }
    }

    #[test]
    fn the_admin_listener_is_validated() {
        let ok = format!("{MINIMAL}\n[admin]\nport = 9090\ntoken = {{ env = \"ADMIN_TOKEN\" }}");
        let config = ServeConfig::parse(&ok, Format::Toml).unwrap();
        assert_eq!(config.admin.as_ref().unwrap().host, "127.0.0.1");
        for (bad, path) in [
            ("host = \"nope\"\nport = 9090", "admin.host"),
            ("port = 3000", "admin.port"),
            (
                "port = 9090\ntoken = { env = \"OPENAI_API_KEY\" }",
                "admin.token",
            ),
            (
                "port = 9090\ntoken = { env = \"CI_GATEWAY_KEY\" }",
                "admin.token",
            ),
            (
                "port = 9090\ntoken = { env = \"lower\" }",
                "admin.token.env",
            ),
        ] {
            let found = issues(&format!("{MINIMAL}\n[admin]\n{bad}"));
            assert!(has(&found, path), "{bad}: {found:?}");
        }
        // Port 0 means "pick one", so two zeros do not collide.
        let zeros = format!(
            "{MINIMAL}\n[server]\nport = 0\n[admin]\nport = 0\nallow_unauthenticated = true"
        );
        assert!(ServeConfig::parse(&zeros, Format::Toml).is_ok());
    }

    #[test]
    fn an_admin_listener_needs_a_token_or_an_explicit_opt_out() {
        let tokenless = format!("{MINIMAL}\n[admin]\nport = 9090");
        let found = issues(&tokenless);
        assert!(has(&found, "admin.token"), "{found:?}");
        assert!(found[0].contains("allow_unauthenticated"), "{found:?}");
        let opted_out = format!("{MINIMAL}\n[admin]\nport = 9090\nallow_unauthenticated = true");
        assert!(ServeConfig::parse(&opted_out, Format::Toml).is_ok());
        let both = format!(
            "{MINIMAL}\n[admin]\nport = 9090\nallow_unauthenticated = true\n\
             token = {{ env = \"ADMIN_TOKEN\" }}"
        );
        assert!(has(&issues(&both), "admin.allow_unauthenticated"));
    }

    #[test]
    fn an_admin_token_cannot_equal_a_sha256_caller_key() {
        let digest = format!("{:x}", Sha256::digest(b"the-admin-token-0123456789"));
        let text = format!(
            "version = 1\n\
             [providers.openai]\nkind = \"openai\"\napi_key = {{ env = \"OPENAI_API_KEY\" }}\n\
             [[keys]]\nname = \"batch\"\ntenant = \"t\"\nsha256 = \"{digest}\"\n\
             [admin]\nport = 9090\ntoken = {{ env = \"ADMIN_TOKEN\" }}"
        );
        let config = ServeConfig::parse(&text, Format::Toml).unwrap();
        let env = |name: &str| match name {
            "OPENAI_API_KEY" => Some("upstream-key-0123456789".to_string()),
            "ADMIN_TOKEN" => Some("the-admin-token-0123456789".to_string()),
            _ => None,
        };
        let found = config.check_references(&env);
        assert!(found.iter().any(|i| i.path == "admin.token"), "{found:?}");
    }

    #[test]
    fn an_admin_token_cannot_share_a_value_with_other_secrets() {
        let text = format!("{MINIMAL}\n[admin]\nport = 9090\ntoken = {{ env = \"ADMIN_TOKEN\" }}");
        let config = ServeConfig::parse(&text, Format::Toml).unwrap();
        let all_same = |_: &str| Some("one-value-for-everything".to_string());
        let found = config.check_references(&all_same);
        assert!(found.iter().any(|i| i.path == "admin.token"), "{found:?}");
        assert!(!found
            .iter()
            .any(|i| i.message.contains("one-value-for-everything")));
        // Each arm alone: equal to a provider key, and equal to a caller key.
        let as_provider = |name: &str| match name {
            "ADMIN_TOKEN" | "OPENAI_API_KEY" => Some("shared-with-the-provider".to_string()),
            _ => Some("different-value-0123456789".to_string()),
        };
        assert!(config
            .check_references(&as_provider)
            .iter()
            .any(|i| i.path == "admin.token"));
        let as_caller = |name: &str| match name {
            "ADMIN_TOKEN" | "CI_GATEWAY_KEY" => Some("shared-with-the-caller".to_string()),
            _ => Some("different-value-0123456789".to_string()),
        };
        assert!(config
            .check_references(&as_caller)
            .iter()
            .any(|i| i.path == "admin.token"));
    }

    #[test]
    fn hosted_providers_use_their_own_name_and_servers_may_not_borrow_one() {
        let renamed = MINIMAL.replace("[providers.openai]", "[providers.openai-eu]");
        assert!(has(&issues(&renamed), "providers.openai-eu"));
        let borrowed = r#"
            version = 1
            [server]
            allow_unauthenticated = true
            [providers.openai]
            kind = "openai-compatible"
            base_url = "http://x.example/v1"
        "#;
        assert!(has(&issues(borrowed), "providers.openai"));
        let server = borrowed.replace("[providers.openai]", "[providers.inference]");
        assert!(ServeConfig::parse(&server, Format::Toml).is_ok());
    }

    #[test]
    fn state_settings_are_bounded() {
        let ok = format!(
            "{MINIMAL}\n[state]\nreceipts_dir = \"/var/lib/llmshim\"\nidempotency_ttl_secs = 600"
        );
        let config = ServeConfig::parse(&ok, Format::Toml).unwrap();
        assert_eq!(config.state.idempotency_ttl_secs, Some(600));
        for good in [1, 2000, 30_000] {
            let text = format!("{MINIMAL}\n[state]\nreceipt_lock_timeout_ms = {good}");
            assert!(issues(&text).is_empty(), "{good}");
        }
        for bad in [
            "receipts_dir = \"relative/dir\"",
            "idempotency_ttl_secs = 0",
            "idempotency_ttl_secs = 604801",
            "receipt_lock_timeout_ms = 0",
            "receipt_lock_timeout_ms = 30001",
        ] {
            let found = issues(&format!("{MINIMAL}\n[state]\n{bad}"));
            assert!(
                has(&found, "state.receipts_dir")
                    || has(&found, "state.idempotency_ttl_secs")
                    || has(&found, "state.receipt_lock_timeout_ms"),
                "{bad}: {found:?}"
            );
        }
    }

    #[test]
    fn base_urls_must_be_final_endpoints_without_credentials() {
        assert!(validate_base_url("https://llm.internal.example/v1").is_ok());
        assert!(validate_base_url("http://localhost:8000/v1").is_ok());
        for ok in [
            "https://[::1]:8000/v1",
            "http://127.0.0.1:8000",
            "http://localhost",
            "https://example.com/v1/",
            "https://EXAMPLE.com/v1",
            "http://llm_server:8000/v1",
            "http://llm.internal.example:80/v1",
        ] {
            assert!(validate_base_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "llm.internal.example/v1",
            "ftp://example.com",
            "https://",
            "https://key@example.com/v1",
            "https://example.com/v1?key=1",
            "https://example.com/v1#frag",
            "https://exa mple.com",
            // What a split on '/' let through: no host, a port that is not a
            // number or does not fit, an unterminated address.
            "http://:8000/v1",
            "http://example.com:notaport/v1",
            "http://example.com:99999/v1",
            "http://[::1/v1",
            // What the URL parser alone would let through: it discards extra
            // slashes (so `https:///v1` becomes the host `v1`, and the provider
            // key would be sent there) and an empty userinfo.
            "https:///v1",
            "http:////h/v1",
            "https://@h/v1",
            "https://:@h/v1",
            "https://:@/v1",
        ] {
            assert!(validate_base_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn serving_without_keys_is_limited_to_loopback() {
        let open = |host: &str| {
            format!(
                "version = 1\n[server]\nhost = \"{host}\"\nallow_unauthenticated = true\n\
                 [providers.openai]\nkind = \"openai\"\napi_key = {{ env = \"OPENAI_API_KEY\" }}"
            )
        };
        for ok in ["127.0.0.1", "127.8.9.10", "::1", "[::1]"] {
            assert!(
                issues(&open(ok)).is_empty(),
                "{ok}: {:?}",
                issues(&open(ok))
            );
        }
        // An IPv4-mapped IPv6 loopback is not accepted: fail closed, say 127.0.0.1.
        for routable in [
            "0.0.0.0",
            "::",
            "10.0.0.5",
            "192.168.1.9",
            "203.0.113.7",
            "::ffff:127.0.0.1",
        ] {
            let found = issues(&open(routable));
            assert_eq!(
                found.len(),
                1,
                "one problem, no duplicate: {routable}: {found:?}"
            );
            assert!(has(&found, "server.host"), "{routable}: {found:?}");
            assert!(
                found[0].contains("loopback"),
                "the message says what to do: {found:?}"
            );
        }
        // With no keys and no opt-in, a routable host does not add a second problem.
        let neither = "version = 1\n[server]\nhost = \"0.0.0.0\"\n[providers.openai]\n\
                       kind = \"openai\"\napi_key = { env = \"OPENAI_API_KEY\" }";
        let found = issues(neither);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(has(&found, "keys"));
        // With caller keys any address is allowed: the keys decide.
        let keyed = format!("{MINIMAL}\n[server]\nhost = \"0.0.0.0\"");
        assert!(issues(&keyed).is_empty(), "{:?}", issues(&keyed));
    }

    fn with_telemetry(extra: &str) -> String {
        format!(
            "{MINIMAL}\n[telemetry]\notlp_endpoint = \"https://collector.internal:4318\"\n{extra}"
        )
    }

    #[test]
    fn the_telemetry_table_is_validated_and_secrets_stay_references() {
        let good = ServeConfig::parse(
            &with_telemetry(
                "service_name = \"edge\"\nsample_ratio = 0.25\n\
                 [telemetry.headers]\nauthorization = { env = \"OTLP_AUTHORIZATION\" }",
            ),
            Format::Toml,
        )
        .unwrap();
        let telemetry = good.telemetry.as_ref().unwrap();
        assert_eq!(telemetry.sample_ratio, Some(0.25));
        // The header is a secret reference like every other: checked for presence.
        let missing = good.check_secrets_present(&|_| None);
        assert!(
            missing
                .iter()
                .any(|issue| issue.path == "telemetry.headers.authorization"),
            "{missing:?}"
        );
        let present =
            good.check_secrets_present(&|name| (name == "OTLP_AUTHORIZATION").then(|| "x".into()));
        assert!(
            !present
                .iter()
                .any(|issue| issue.path == "telemetry.headers.authorization"),
            "{present:?}"
        );
        // Bad values name their setting.
        for (bad, path) in [
            ("sample_ratio = 1.5", "telemetry.sample_ratio"),
            ("sample_ratio = -0.1", "telemetry.sample_ratio"),
            ("service_name = \"\"", "telemetry.service_name"),
            ("service_name = \"has space\"", "telemetry.service_name"),
            (
                "[telemetry.headers]\nAuthorization = { env = \"X\" }",
                "telemetry.headers.Authorization",
            ),
            (
                "[telemetry.headers]\nauthorization = \"Bearer pasted-inline\"",
                "(file)",
            ),
            ("surprise = true", "(file)"),
        ] {
            let found = issues(&with_telemetry(bad));
            assert!(has(&found, path), "{bad}: {found:?}");
            assert!(
                !found.iter().any(|line| line.contains("pasted-inline")),
                "never echoed: {found:?}"
            );
        }
        let bad_endpoint =
            MINIMAL.to_string() + "\n[telemetry]\notlp_endpoint = \"collector:4318\"";
        assert!(has(&issues(&bad_endpoint), "telemetry.otlp_endpoint"));
        let missing_endpoint = MINIMAL.to_string() + "\n[telemetry]\nservice_name = \"x\"";
        assert!(!issues(&missing_endpoint).is_empty());
    }

    #[test]
    fn telemetry_needs_the_serve_otel_build() {
        let config = ServeConfig::parse(&with_telemetry(""), Format::Toml).unwrap();
        let found = config.feature_issues();
        // Refused without the feature, accepted with it: a setting is never
        // parsed and then ignored.
        assert_eq!(
            found.iter().any(|issue| issue.path == "telemetry"),
            !cfg!(feature = "serve-otel"),
            "{found:?}"
        );
        assert!(ServeConfig::parse(MINIMAL, Format::Toml)
            .unwrap()
            .feature_issues()
            .is_empty());
    }

    #[test]
    fn spend_windows_and_queue_waits_have_upper_bounds() {
        let window = |secs: u64| {
            format!(
                "{MINIMAL}\n[[keys]]\nname = \"b\"\ntenant = \"other\"\n\
                 secret = {{ env = \"OTHER_KEY\" }}\nbudget_usd = 5.0\nbudget_window_secs = {secs}"
            )
        };
        assert!(issues(&window(MAX_BUDGET_WINDOW_SECS)).is_empty());
        // (TOML integers are signed 64-bit, so i64::MAX is the largest it can say.)
        for bad in [0, MAX_BUDGET_WINDOW_SECS + 1, i64::MAX as u64] {
            let found = issues(&window(bad));
            assert!(
                has(&found, "keys[1].budget_window_secs"),
                "{bad}: {found:?}"
            );
        }
        let queue = |ms: u64| format!("{MINIMAL}\n[limits]\nqueue_timeout_ms = {ms}");
        assert!(issues(&queue(0)).is_empty());
        assert!(issues(&queue(MAX_QUEUE_TIMEOUT_MS)).is_empty());
        for bad in [MAX_QUEUE_TIMEOUT_MS + 1, i64::MAX as u64] {
            let found = issues(&queue(bad));
            assert!(has(&found, "limits.queue_timeout_ms"), "{bad}: {found:?}");
        }
        let penalty = |secs: u64| format!("{MINIMAL}\n[limits]\npenalty_secs = {secs}");
        assert!(issues(&penalty(1)).is_empty());
        assert!(issues(&penalty(MAX_PENALTY_SECS)).is_empty());
        for bad in [0, MAX_PENALTY_SECS + 1, i64::MAX as u64] {
            let found = issues(&penalty(bad));
            assert!(has(&found, "limits.penalty_secs"), "{bad}: {found:?}");
        }
    }

    #[test]
    fn duplicate_caller_secrets_and_digests_are_refused() {
        let digest = "a".repeat(64);
        let text = format!(
            r#"
            version = 1
            [providers.openai]
            kind = "openai"
            api_key = {{ env = "OPENAI_API_KEY" }}
            [[keys]]
            name = "a"
            tenant = "t"
            secret = {{ env = "SAME" }}
            [[keys]]
            name = "b"
            tenant = "t"
            secret = {{ env = "SAME" }}
            [[keys]]
            name = "c"
            tenant = "t"
            sha256 = "{digest}"
            [[keys]]
            name = "d"
            tenant = "t"
            sha256 = "{digest}"
            "#
        );
        let found = issues(&text);
        assert!(has(&found, "keys[1].secret"), "{found:?}");
        assert!(has(&found, "keys[3].sha256"), "{found:?}");
    }

    #[test]
    fn reference_checks_name_the_setting_and_never_the_value() {
        let dir = std::env::temp_dir().join(format!("llmshim-serve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let present = dir.join("present");
        std::fs::write(&present, "file-secret-value\n").unwrap();
        let empty = dir.join("empty");
        std::fs::write(&empty, "\n").unwrap();
        let text = format!(
            r#"
            version = 1
            [providers.a]
            kind = "openai-compatible"
            base_url = "http://a.example/v1"
            api_key = {{ env = "SET" }}
            [providers.b]
            kind = "openai-compatible"
            base_url = "http://b.example/v1"
            api_key = {{ env = "UNSET" }}
            [providers.c]
            kind = "openai-compatible"
            base_url = "http://c.example/v1"
            api_key = {{ env = "BLANK" }}
            [providers.d]
            kind = "openai-compatible"
            base_url = "http://d.example/v1"
            api_key = {{ file = "{}" }}
            [providers.e]
            kind = "openai-compatible"
            base_url = "http://e.example/v1"
            api_key = {{ file = "{}" }}
            [providers.f]
            kind = "openai-compatible"
            base_url = "http://f.example/v1"
            api_key = {{ file = "{}" }}
            [[keys]]
            name = "k"
            tenant = "t"
            secret = {{ env = "UNSET_KEY" }}
            "#,
            present.display(),
            empty.display(),
            dir.join("absent").display(),
        );
        let config = ServeConfig::parse(&text, Format::Toml).unwrap();
        let env = |name: &str| match name {
            "SET" => Some("env-secret-value".to_string()),
            "BLANK" => Some("  ".to_string()),
            _ => None,
        };
        let found: Vec<String> = config
            .check_references(&env)
            .iter()
            .map(Issue::to_string)
            .collect();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found.len(), 5, "{found:#?}");
        for path in [
            "providers.b.api_key",
            "providers.c.api_key",
            "providers.e.api_key",
            "providers.f.api_key",
            "keys[0].secret",
        ] {
            assert!(has(&found, path), "missing {path} in {found:#?}");
        }
        let all = found.join("\n");
        assert!(!all.contains("env-secret-value") && !all.contains("file-secret-value"));
    }

    #[test]
    fn debug_output_shows_where_a_secret_lives_not_what_it_is() {
        let config = ServeConfig::parse(MINIMAL, Format::Toml).unwrap();
        let debug = format!("{config:?}");
        assert!(debug.contains("SecretRef(env OPENAI_API_KEY)"));
    }

    #[test]
    fn the_file_extension_selects_the_format() {
        let dir = std::env::temp_dir().join(format!("llmshim-serve-ext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let toml_path = dir.join("gateway.toml");
        std::fs::write(&toml_path, MINIMAL).unwrap();
        assert!(ServeConfig::load(&toml_path).is_ok());
        let yaml_path = dir.join("gateway.yaml");
        std::fs::write(&yaml_path, MINIMAL).unwrap();
        let missing = ServeConfig::load(&dir.join("absent.toml")).unwrap_err();
        let wrong = ServeConfig::load(&yaml_path).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(wrong[0].message.contains(".toml or .json"));
        assert!(missing[0].message.contains("cannot read"));
    }
}

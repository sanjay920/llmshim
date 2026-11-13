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

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// The only schema version this build reads.
pub const VERSION: u32 = 1;

/// Largest spend cap the ledger can represent exactly (nano-USD in an f64-safe
/// integer), matching the gateway's own bound.
const MAX_BUDGET_USD: f64 = 9_007_199.254_740_992;

/// One problem with a configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Dotted path of the offending setting, e.g. `providers.openai.api_key`.
    pub path: String,
    pub message: String,
}

impl Issue {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
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
pub enum Format {
    Toml,
    Json,
}

/// The whole file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ServeConfig {
    pub version: u32,
    #[serde(default)]
    pub server: Server,
    #[serde(default, deserialize_with = "unique_map")]
    pub providers: BTreeMap<String, Provider>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub keys: Vec<GatewayKey>,
    #[serde(default, deserialize_with = "unique_map")]
    pub routes: BTreeMap<String, Route>,
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
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Server {
    /// IP address to bind. Defaults to loopback; a container sets `0.0.0.0`.
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Serve callers with no key. Off by default: a file with no `[[keys]]`
    /// is refused unless this is set.
    #[serde(default)]
    pub allow_unauthenticated: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            allow_unauthenticated: false,
        }
    }
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    3000
}

/// Which adapter a provider entry uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
}

impl ProviderKind {
    fn requires_base_url(self) -> bool {
        matches!(self, Self::OpenaiCompatible)
    }
    fn requires_api_key(self) -> bool {
        !matches!(self, Self::OpenaiCompatible)
    }
}

/// One upstream. Its table name is the routing prefix (`<name>/<model>`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Provider {
    pub kind: ProviderKind,
    #[serde(default)]
    pub api_key: Option<SecretRef>,
    #[serde(default)]
    pub base_url: Option<String>,
}

/// Where a secret lives. Exactly one field is set; the value never appears in
/// the file, in `Debug` output, or in an [`Issue`].
#[derive(Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Limits {
    /// In-flight upstream requests per provider, per instance.
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    /// Longest a request may wait for capacity before a 503.
    #[serde(default)]
    pub queue_timeout_ms: Option<u64>,
    /// Requests per minute for every provider without its own entry.
    #[serde(default)]
    pub rpm: Option<u32>,
    /// Tokens per minute for every provider without its own entry.
    #[serde(default)]
    pub tpm: Option<u32>,
    #[serde(default, deserialize_with = "unique_map")]
    pub providers: BTreeMap<String, ProviderLimits>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ProviderLimits {
    #[serde(default)]
    pub rpm: Option<u32>,
    #[serde(default)]
    pub tpm: Option<u32>,
}

/// A caller credential and the identity it carries.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct GatewayKey {
    /// Operator-facing label; never sent on the wire.
    pub name: String,
    /// The bearer token, by reference. Mutually exclusive with `sha256`.
    #[serde(default)]
    pub secret: Option<SecretRef>,
    /// Lower-case hex SHA-256 of the bearer token, so the deployment holds no
    /// caller secret at all. Mutually exclusive with `secret`.
    #[serde(default)]
    pub sha256: Option<String>,
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
#[derive(Debug, Clone, Deserialize)]
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
        Self::parse(&text, format)
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
        self.validate_providers(&mut issues);
        self.validate_limits(&mut issues);
        self.validate_keys(&mut issues);
        self.validate_routes(&mut issues);
        issues
    }

    fn validate_server(&self, issues: &mut Vec<Issue>) {
        let host = self.server.host.as_str();
        let bare = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if bare.parse::<std::net::IpAddr>().is_err() {
            issues.push(Issue::new("server.host", "must be an IPv4 or IPv6 address"));
        }
        if self.keys.is_empty() && !self.server.allow_unauthenticated {
            issues.push(Issue::new(
                "keys",
                "no caller keys are defined; add [[keys]] or set server.allow_unauthenticated = true",
            ));
        }
        if !self.keys.is_empty() && self.server.allow_unauthenticated {
            issues.push(Issue::new(
                "server.allow_unauthenticated",
                "cannot be true while [[keys]] are defined",
            ));
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
        }
    }

    fn validate_limits(&self, issues: &mut Vec<Issue>) {
        if self.limits.max_concurrency == Some(0) {
            issues.push(Issue::new("limits.max_concurrency", "must be at least 1"));
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

    fn validate_keys(&self, issues: &mut Vec<Issue>) {
        let mut names = BTreeSet::new();
        let mut digests = BTreeSet::new();
        let mut references = BTreeSet::new();
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
            match (&key.secret, &key.sha256) {
                (Some(_), Some(_)) => {
                    issues.push(Issue::new(&at, "set either secret or sha256, not both"))
                }
                (None, None) => issues.push(Issue::new(&at, "set secret or sha256")),
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
            if let Some(budget) = key.budget_usd {
                if !budget.is_finite() || !(0.0..=MAX_BUDGET_USD).contains(&budget) {
                    issues.push(Issue::new(
                        format!("{at}.budget_usd"),
                        format!("must be between 0 and {MAX_BUDGET_USD}"),
                    ));
                }
            }
            if key.budget_window_secs == Some(0) {
                issues.push(Issue::new(
                    format!("{at}.budget_window_secs"),
                    "must be at least 1",
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
        providers.chain(keys).collect()
    }

    /// Check that every referenced secret exists and is non-empty, without
    /// returning or printing any value. `env` looks up one variable, so a
    /// caller (or a test) decides what the environment is.
    pub fn check_references(&self, env: &dyn Fn(&str) -> Option<String>) -> Vec<Issue> {
        let mut issues = Vec::new();
        let read = |secret: &SecretRef| -> Option<String> {
            let value = match (&secret.env, &secret.file) {
                (Some(name), _) => env(name)?,
                (None, Some(path)) => std::fs::read_to_string(path).ok()?,
                (None, None) => return None,
            };
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        };
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
                    Some(value) => (format!("keys[{index}].secret"), digest(&value)),
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
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or("must start with https:// or http://")?;
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("must not contain whitespace");
    }
    if rest.contains('?') || rest.contains('#') {
        return Err("must not contain a query string or fragment");
    }
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return Err("must name a host");
    }
    if authority.contains('@') {
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

    #[test]
    fn the_documented_example_is_valid() {
        let example = include_str!("../../docs/design/enterprise-gateway/example.toml");
        let config = ServeConfig::parse(example, Format::Toml).unwrap();
        assert_eq!(config.providers.len(), 3);
        assert_eq!(config.keys.len(), 2);
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
                let found = ServeConfig::parse(text, format).err().expect(text);
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
    fn base_urls_must_be_final_endpoints_without_credentials() {
        assert!(validate_base_url("https://llm.internal.example/v1").is_ok());
        assert!(validate_base_url("http://localhost:8000/v1").is_ok());
        for bad in [
            "llm.internal.example/v1",
            "ftp://example.com",
            "https://",
            "https://key@example.com/v1",
            "https://example.com/v1?key=1",
            "https://example.com/v1#frag",
            "https://exa mple.com",
        ] {
            assert!(validate_base_url(bad).is_err(), "{bad}");
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
            kind = "openai"
            api_key = {{ env = "SET" }}
            [providers.b]
            kind = "openai"
            api_key = {{ env = "UNSET" }}
            [providers.c]
            kind = "openai"
            api_key = {{ env = "BLANK" }}
            [providers.d]
            kind = "openai"
            api_key = {{ file = "{}" }}
            [providers.e]
            kind = "openai"
            api_key = {{ file = "{}" }}
            [providers.f]
            kind = "openai"
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

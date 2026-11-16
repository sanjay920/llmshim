//! Signed policy snapshots: a verifiable source of caller keys (0024).
//!
//! A snapshot is what `[[keys]]` is, in an envelope that can be checked: the
//! payload bytes are signed as they are (base64 inside the envelope, so no
//! re-serialization can change what was signed), with Ed25519. The data plane
//! verifies it against `[control_plane] verify_keys`, refuses one that does not
//! advance (so an old snapshot cannot un-revoke a key), and turns it into the same
//! [`GatewayKey`] list a file would have, so everything downstream (admission,
//! limits, budgets, logs, reload) is unchanged.
//!
//! A refused snapshot is reported by path with a fixed message: never its content,
//! never a signature or a digest of a secret.

use super::config::{ControlPlane, GatewayKey, Issue, JwtBinding};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// The envelope's `format` value.
pub const FORMAT: &str = "llmshim-policy-snapshot/1";
/// The payload's `version`.
pub const PAYLOAD_VERSION: u32 = 1;
const KEY_PREFIX: &str = "ed25519:";
const AT: &str = "control_plane.snapshot_file";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: String,
    key_id: String,
    payload: String,
    signature: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    pub version: u32,
    pub org: String,
    pub generation: u64,
    pub issued_at: String,
    #[serde(default)]
    pub tenants: Vec<SnapshotTenant>,
    #[serde(default)]
    pub keys: Vec<SnapshotKey>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotTenant {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotKey {
    pub id: String,
    pub name: String,
    pub tenant: String,
    /// `active`, `revoked` or `expired`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_san: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwt: Option<SnapshotJwt>,
    /// RFC 3339. A key already expired when the snapshot is read is not loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub tier: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_window_secs: Option<u64>,
    #[serde(default)]
    pub budget_allow_unpriced: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_cidrs: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotJwt {
    pub table: String,
    pub subject: String,
}

/// Why a snapshot was not applied. Every message is fixed text.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Unreadable,
    Malformed,
    UnknownFormat,
    UntrustedSigner,
    BadSignature,
    UnknownVersion,
    NotAdvancing,
    Conflicting,
    InvalidContent(String),
}

impl Refusal {
    fn message(&self) -> String {
        match self {
            Self::Unreadable => "the snapshot file cannot be read".into(),
            Self::Malformed => "the snapshot is not a well-formed envelope or payload".into(),
            Self::UnknownFormat => "the snapshot's format is not one this build reads".into(),
            Self::UntrustedSigner => {
                "the snapshot was signed by a key that is not in verify_keys".into()
            }
            Self::BadSignature => "the snapshot's signature does not verify".into(),
            Self::UnknownVersion => "the snapshot's version is not one this build reads".into(),
            Self::NotAdvancing => {
                "the snapshot's generation does not advance past the one in force".into()
            }
            Self::Conflicting => {
                "a snapshot with the generation in force but different content".into()
            }
            Self::InvalidContent(why) => why.clone(),
        }
    }

    fn issue(&self) -> Issue {
        Issue::new(AT, self.message())
    }
}

/// A verified snapshot, turned into what the server reads.
#[derive(Debug)]
pub struct Snapshot {
    pub generation: u64,
    /// SHA-256 of the signed payload bytes, to recognize the same snapshot again.
    pub payload_sha256: String,
    /// The signing key's id.
    pub signer: String,
    /// The keys that serve: state `active` and not yet expired.
    pub keys: Vec<GatewayKey>,
    /// How many keys the snapshot lists as revoked or expired (never loaded).
    pub inactive: usize,
}

/// What a replica remembers of the snapshot it last applied.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Floor {
    pub generation: u64,
    pub payload_sha256: String,
}

/// A public key as written in `verify_keys`: `ed25519:<base64 of 32 bytes>`.
pub fn parse_verify_key(text: &str) -> Result<Vec<u8>, &'static str> {
    let b64 = text
        .strip_prefix(KEY_PREFIX)
        .ok_or("must start with ed25519:")?;
    let bytes = B64
        .decode(b64)
        .map_err(|_| "is not valid base64 after ed25519:")?;
    if bytes.len() != 32 {
        return Err("must be a 32-byte Ed25519 public key");
    }
    Ok(bytes)
}

/// Verify `bytes` against `verify_keys` and read the payload. Order and floors are
/// judged by the caller ([`load`]).
fn verify_envelope(
    bytes: &[u8],
    verify_keys: &[String],
) -> Result<(Payload, String, String), Refusal> {
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| Refusal::Malformed)?;
    if envelope.format != FORMAT {
        return Err(Refusal::UnknownFormat);
    }
    let public = verify_keys
        .iter()
        .find(|key| **key == envelope.key_id)
        .and_then(|key| parse_verify_key(key).ok())
        .ok_or(Refusal::UntrustedSigner)?;
    let payload_bytes = B64
        .decode(&envelope.payload)
        .map_err(|_| Refusal::Malformed)?;
    let signature = B64
        .decode(&envelope.signature)
        .map_err(|_| Refusal::Malformed)?;
    UnparsedPublicKey::new(&ED25519, &public)
        .verify(&payload_bytes, &signature)
        .map_err(|_| Refusal::BadSignature)?;
    let payload: Payload =
        serde_json::from_slice(&payload_bytes).map_err(|_| Refusal::Malformed)?;
    if payload.version != PAYLOAD_VERSION {
        return Err(Refusal::UnknownVersion);
    }
    let digest = format!("{:x}", Sha256::digest(&payload_bytes));
    Ok((payload, digest, envelope.key_id))
}

fn to_keys(
    payload: &Payload,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(Vec<GatewayKey>, usize), Refusal> {
    let mut keys = Vec::new();
    let mut inactive = 0;
    for key in &payload.keys {
        match key.state.as_str() {
            "active" => {}
            "revoked" | "expired" => {
                inactive += 1;
                continue;
            }
            _ => {
                return Err(Refusal::InvalidContent(
                    "a snapshot key has a state this build does not know".into(),
                ))
            }
        }
        if let Some(expires) = &key.expires_at {
            let at = chrono::DateTime::parse_from_rfc3339(expires).map_err(|_| {
                Refusal::InvalidContent("a snapshot key's expires_at is not RFC 3339".into())
            })?;
            if at.with_timezone(&chrono::Utc) <= now {
                inactive += 1;
                continue;
            }
        }
        keys.push(GatewayKey {
            name: key.name.clone(),
            secret: None,
            sha256: key.sha256.clone(),
            client_san: key.client_san.clone(),
            jwt: key.jwt.as_ref().map(|jwt| JwtBinding {
                table: jwt.table.clone(),
                subject: jwt.subject.clone(),
            }),
            allowed_cidrs: key.allowed_cidrs.clone(),
            tenant: key.tenant.clone(),
            tier: key.tier,
            rpm: key.rpm,
            tpm: key.tpm,
            budget_usd: key.budget_usd,
            budget_window_secs: key.budget_window_secs,
            budget_allow_unpriced: key.budget_allow_unpriced,
        });
    }
    Ok((keys, inactive))
}

/// Read the floor a replica persisted, if any. A file that exists but cannot be
/// read as a floor is an error, not "no floor": a replica that cannot tell what it
/// applied could be rolled back.
pub fn read_floor(dir: &Path) -> Result<Option<Floor>, Issue> {
    let path = dir.join("applied-snapshot.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|_| {
            Issue::new(
                "state.policy_dir",
                "the applied-snapshot file in it is not readable as one; remove it only if you accept re-applying an older snapshot",
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Issue::new(
            "state.policy_dir",
            format!("cannot read the applied-snapshot file: {}", error.kind()),
        )),
    }
}

/// Remember `snapshot` as the one in force: written to a temporary file and renamed,
/// so a crash leaves the old floor or the new, never half of one.
pub fn write_floor(dir: &Path, floor: &Floor) -> Result<(), Issue> {
    let fail = |error: std::io::Error| {
        Issue::new(
            "state.policy_dir",
            format!("cannot record the applied snapshot: {}", error.kind()),
        )
    };
    std::fs::create_dir_all(dir).map_err(fail)?;
    let temp = dir.join(".applied-snapshot.tmp");
    let body = serde_json::to_vec(floor).map_err(|e| fail(e.into()))?;
    std::fs::write(&temp, body).map_err(fail)?;
    std::fs::rename(&temp, dir.join("applied-snapshot.json")).map_err(fail)
}

/// Read, verify and judge the snapshot `control_plane` names, against the floor.
/// Reads only; recording the generation in force is [`write_floor`], done once the
/// snapshot is applied.
pub fn load(
    control_plane: &ControlPlane,
    floor: Option<&Floor>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Snapshot, Vec<Issue>> {
    let refuse = |refusal: Refusal| vec![refusal.issue()];
    let bytes =
        std::fs::read(&control_plane.snapshot_file).map_err(|_| refuse(Refusal::Unreadable))?;
    let (payload, payload_sha256, signer) =
        verify_envelope(&bytes, &control_plane.verify_keys).map_err(refuse)?;
    let minimum = control_plane.min_generation.unwrap_or(0);
    if payload.generation < minimum {
        return Err(refuse(Refusal::NotAdvancing));
    }
    if let Some(floor) = floor {
        if payload.generation < floor.generation {
            return Err(refuse(Refusal::NotAdvancing));
        }
        if payload.generation == floor.generation && payload_sha256 != floor.payload_sha256 {
            return Err(refuse(Refusal::Conflicting));
        }
    }
    let (keys, inactive) = to_keys(&payload, now).map_err(refuse)?;
    Ok(Snapshot {
        generation: payload.generation,
        payload_sha256,
        signer,
        keys,
        inactive,
    })
}

/// Generate a signing key: its 32-byte seed (base64) and the `verify_keys` entry.
pub fn keygen() -> Result<(String, String), &'static str> {
    let mut seed = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut seed)
        .map_err(|_| "the system random source failed")?;
    let pair = Ed25519KeyPair::from_seed_unchecked(&seed).map_err(|_| "invalid seed")?;
    Ok((
        B64.encode(seed),
        format!("{KEY_PREFIX}{}", B64.encode(pair.public_key().as_ref())),
    ))
}

/// Sign `payload` (a JSON document) with the seed (base64) and return the envelope.
/// The payload is re-encoded canonically (sorted keys, no insignificant whitespace),
/// so a signer's formatting never matters; what is signed are those bytes.
pub fn sign(payload: &serde_json::Value, seed_b64: &str) -> Result<String, String> {
    let seed = B64
        .decode(seed_b64.trim())
        .map_err(|_| "the signing key is not valid base64".to_string())?;
    let pair = Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|_| "the signing key is not a 32-byte Ed25519 seed".to_string())?;
    // Reading it as the typed payload refuses a document the data plane would refuse.
    let typed: Payload = serde_json::from_value(payload.clone())
        .map_err(|error| format!("the payload is not a valid snapshot payload: {error}"))?;
    if typed.version != PAYLOAD_VERSION {
        return Err("the payload's version is not one this build writes".into());
    }
    let bytes = canonical(payload);
    let envelope = Envelope {
        format: FORMAT.into(),
        key_id: format!("{KEY_PREFIX}{}", B64.encode(pair.public_key().as_ref())),
        payload: B64.encode(&bytes),
        signature: B64.encode(pair.sign(&bytes).as_ref()),
    };
    serde_json::to_string_pretty(&envelope).map_err(|e| e.to_string())
}

/// Verify an envelope's text against `verify_keys` and describe it, for the
/// `snapshot verify` command. No floor is consulted.
pub fn describe(text: &[u8], verify_keys: &[String]) -> Result<String, String> {
    let (payload, digest, signer) =
        verify_envelope(text, verify_keys).map_err(|refusal| refusal.message())?;
    Ok(format!(
        "ok: org {}, generation {}, {} key(s), signed by {signer}, payload sha256 {digest}",
        payload.org,
        payload.generation,
        payload.keys.len()
    ))
}

/// Canonical JSON: object keys sorted, no insignificant whitespace. Written out by
/// hand so it does not depend on whether `serde_json` keeps insertion order (another
/// crate in the build may turn that feature on).
fn canonical(value: &serde_json::Value) -> Vec<u8> {
    fn write(value: &serde_json::Value, out: &mut Vec<u8>) {
        match value {
            serde_json::Value::Object(map) => {
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                out.push(b'{');
                for (index, (key, item)) in entries.into_iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    out.extend(serde_json::to_vec(key).expect("a string serializes"));
                    out.push(b':');
                    write(item, out);
                }
                out.push(b'}');
            }
            serde_json::Value::Array(items) => {
                out.push(b'[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    write(item, out);
                }
                out.push(b']');
            }
            other => out.extend(serde_json::to_vec(other).expect("a scalar serializes")),
        }
    }
    let mut out = Vec::new();
    write(value, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest(token: &str) -> String {
        format!("{:x}", Sha256::digest(token.as_bytes()))
    }

    fn payload(generation: u64, keys: serde_json::Value) -> serde_json::Value {
        json!({"version": 1, "org": "org_test", "generation": generation,
            "issued_at": "2026-11-16T12:00:00Z",
            "tenants": [{"id": "team_a", "name": "platform"}], "keys": keys})
    }

    fn active(name: &str, token: &str) -> serde_json::Value {
        json!({"id": format!("key_{name}"), "name": name, "tenant": "team_a",
            "state": "active", "sha256": digest(token)})
    }

    struct Fixture {
        dir: std::path::PathBuf,
        seed: String,
        public: String,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "llmshim-snapshot-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let (seed, public) = keygen().unwrap();
            Self { dir, seed, public }
        }

        fn control_plane(&self) -> ControlPlane {
            let text = format!(
                "verify_keys = [\"{}\"]\nsnapshot_file = \"{}\"\n",
                self.public,
                self.dir.join("snapshot.json").display()
            );
            toml::from_str(&text).unwrap()
        }

        fn write(&self, payload: &serde_json::Value) {
            std::fs::write(
                self.dir.join("snapshot.json"),
                sign(payload, &self.seed).unwrap(),
            )
            .unwrap();
        }

        fn load(&self, floor: Option<&Floor>) -> Result<Snapshot, Vec<Issue>> {
            load(&self.control_plane(), floor, chrono::Utc::now())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn refused(result: Result<Snapshot, Vec<Issue>>) -> String {
        let issues = result.expect_err("the snapshot must be refused");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, AT);
        issues[0].message.clone()
    }

    #[test]
    fn a_signed_snapshot_becomes_the_keys_a_file_would_have() {
        let fixture = Fixture::new();
        fixture.write(&payload(
            7,
            json!([active("ci", "token-one-0123456789abcdef"), {
                "id": "key_old", "name": "old", "tenant": "team_a",
                "state": "revoked", "sha256": digest("token-two-0123456789abcdef")
            }]),
        ));
        let snapshot = fixture.load(None).unwrap();
        assert_eq!(snapshot.generation, 7);
        assert_eq!(snapshot.signer, fixture.public);
        assert_eq!(snapshot.inactive, 1);
        assert_eq!(snapshot.keys.len(), 1);
        assert_eq!(snapshot.keys[0].name, "ci");
        assert_eq!(snapshot.keys[0].tenant, "team_a");
        assert_eq!(
            snapshot.keys[0].sha256.as_deref(),
            Some(digest("token-one-0123456789abcdef").as_str())
        );
    }

    #[test]
    fn a_tampered_payload_or_signature_is_refused_and_reports_no_content() {
        let fixture = Fixture::new();
        fixture.write(&payload(
            1,
            json!([active("ci", "token-one-0123456789abcdef")]),
        ));
        let path = fixture.dir.join("snapshot.json");
        let original = std::fs::read_to_string(&path).unwrap();
        let mut envelope: serde_json::Value = serde_json::from_str(&original).unwrap();
        // Change the signed bytes: a different generation, validly encoded.
        let forged = B64.encode(canonical(&payload(
            99,
            json!([active("ci", "token-one-0123456789abcdef")]),
        )));
        envelope["payload"] = json!(forged);
        std::fs::write(&path, envelope.to_string()).unwrap();
        let message = refused(fixture.load(None));
        assert!(message.contains("signature"), "{message}");
        assert!(
            !message.contains("99") && !message.contains(&digest("token-one-0123456789abcdef"))
        );
        // A signature that is not even base64.
        let mut envelope: serde_json::Value = serde_json::from_str(&original).unwrap();
        envelope["signature"] = json!("!!!");
        std::fs::write(&path, envelope.to_string()).unwrap();
        assert!(refused(fixture.load(None)).contains("well-formed"));
    }

    #[test]
    fn a_snapshot_from_a_key_that_is_not_trusted_is_refused() {
        let fixture = Fixture::new();
        let (other_seed, _) = keygen().unwrap();
        std::fs::write(
            fixture.dir.join("snapshot.json"),
            sign(
                &payload(1, json!([active("ci", "token-one-0123456789abcdef")])),
                &other_seed,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(refused(fixture.load(None)).contains("not in verify_keys"));
    }

    #[test]
    fn an_unknown_format_or_version_is_refused() {
        let fixture = Fixture::new();
        fixture.write(&payload(
            1,
            json!([active("ci", "token-one-0123456789abcdef")]),
        ));
        let path = fixture.dir.join("snapshot.json");
        let mut envelope: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        envelope["format"] = json!("llmshim-policy-snapshot/2");
        std::fs::write(&path, envelope.to_string()).unwrap();
        assert!(refused(fixture.load(None)).contains("format"));
        // A version the signer could not have written: signing refuses it.
        let mut bad = payload(1, json!([]));
        bad["version"] = json!(2);
        assert!(sign(&bad, &fixture.seed).is_err());
    }

    #[test]
    fn a_generation_must_advance_and_the_same_one_must_be_the_same_snapshot() {
        let fixture = Fixture::new();
        fixture.write(&payload(
            5,
            json!([active("ci", "token-one-0123456789abcdef")]),
        ));
        let first = fixture.load(None).unwrap();
        let floor = Floor {
            generation: first.generation,
            payload_sha256: first.payload_sha256.clone(),
        };
        // The same file again is the same snapshot: fine (a reload, a restart).
        assert!(fixture.load(Some(&floor)).is_ok());
        // An older generation, even validly signed, is refused: it could un-revoke.
        fixture.write(&payload(
            4,
            json!([active("ci", "token-one-0123456789abcdef")]),
        ));
        assert!(refused(fixture.load(Some(&floor))).contains("does not advance"));
        // The same generation with different content is refused.
        fixture.write(&payload(
            5,
            json!([active("other", "token-two-0123456789abcdef")]),
        ));
        assert!(refused(fixture.load(Some(&floor))).contains("different content"));
        // A higher one applies.
        fixture.write(&payload(
            6,
            json!([active("other", "token-two-0123456789abcdef")]),
        ));
        assert_eq!(fixture.load(Some(&floor)).unwrap().generation, 6);
    }

    #[test]
    fn min_generation_is_a_floor_for_a_replica_with_no_record() {
        let fixture = Fixture::new();
        fixture.write(&payload(
            3,
            json!([active("ci", "token-one-0123456789abcdef")]),
        ));
        let mut cp = fixture.control_plane();
        cp.min_generation = Some(4);
        assert!(load(&cp, None, chrono::Utc::now()).is_err());
        cp.min_generation = Some(3);
        assert!(load(&cp, None, chrono::Utc::now()).is_ok());
    }

    #[test]
    fn an_expired_key_is_not_loaded_and_an_unknown_state_is_refused() {
        let fixture = Fixture::new();
        let mut expired = active("late", "token-two-0123456789abcdef");
        expired["expires_at"] = json!("2020-01-01T00:00:00Z");
        let mut future = active("soon", "token-three-0123456789abcdef");
        future["expires_at"] = json!("2999-01-01T00:00:00Z");
        fixture.write(&payload(
            1,
            json!([active("ci", "token-one-0123456789abcdef"), expired, future]),
        ));
        let snapshot = fixture.load(None).unwrap();
        let names: Vec<_> = snapshot.keys.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(names, ["ci", "soon"]);
        assert_eq!(snapshot.inactive, 1);
        let mut odd = active("odd", "token-four-0123456789abcdef");
        odd["state"] = json!("suspended");
        fixture.write(&payload(2, json!([odd])));
        assert!(refused(fixture.load(None)).contains("state"));
    }

    #[test]
    fn signing_is_canonical_whatever_the_input_formatting() {
        let a = json!({"b": 1, "a": {"y": [2, 1], "x": null}});
        let b: serde_json::Value =
            serde_json::from_str(r#"{ "a" : { "x" : null, "y" : [2,1] }, "b" : 1 }"#).unwrap();
        assert_eq!(canonical(&a), canonical(&b));
        assert_eq!(
            String::from_utf8(canonical(&a)).unwrap(),
            r#"{"a":{"x":null,"y":[2,1]},"b":1}"#
        );
    }

    #[test]
    fn the_floor_is_written_atomically_read_back_and_an_unreadable_one_is_an_error() {
        let fixture = Fixture::new();
        let dir = fixture.dir.join("policy");
        assert_eq!(read_floor(&dir).unwrap(), None);
        let floor = Floor {
            generation: 9,
            payload_sha256: "ab".repeat(32),
        };
        write_floor(&dir, &floor).unwrap();
        assert_eq!(read_floor(&dir).unwrap(), Some(floor));
        std::fs::write(dir.join("applied-snapshot.json"), "not json").unwrap();
        let issue = read_floor(&dir).unwrap_err();
        assert_eq!(issue.path, "state.policy_dir");
    }

    #[test]
    fn verify_keys_are_parsed_strictly() {
        assert!(parse_verify_key("ed25519:AAAA").is_err());
        assert!(parse_verify_key("rsa:AAAA").is_err());
        assert!(parse_verify_key("ed25519:!!!").is_err());
        let (_, public) = keygen().unwrap();
        assert_eq!(parse_verify_key(&public).unwrap().len(), 32);
    }
}

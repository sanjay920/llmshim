//! Large payloads of a fleet's media routes, held in an object store (note 0050).
//!
//! A string of 32 KiB or more in a job or an answer is **sealed** (AES-256-GCM, a fresh key and
//! nonce per object, the object's name as associated data), put in the store, and replaced by a
//! small reference that carries the key and nonce. The store holds ciphertext under a name that says
//! nothing; deleting the reference is deleting the data. The backends are the [`BlobStore`] trait's:
//! a directory (`file`) and any S3-compatible service (`s3`).

pub mod file;
pub mod s3;
mod sigv4;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime};

use crate::serve::config::{MediaStore, MediaStoreKind};
use std::sync::Arc;

/// The store a server was configured with, and what the sweeper and the callers need beside it.
pub struct Handle {
    /// Names are `<date>/<random>`; the backend's own prefix (an S3 key prefix, a directory root) is
    /// added by the backend, never by the caller.
    pub store: Arc<dyn BlobStore>,
    /// An object older than this is an orphan.
    pub ttl: Duration,
}

/// The default and the most `max_object_bytes` may be.
pub const DEFAULT_MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// Build the store `[media_store]` names. Credentials are read here (a wrong path or an unset
/// variable fails at start, not at the first request); the error names the field, never a value.
pub fn from_config(
    config: &MediaStore,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Handle, String> {
    let max = config
        .max_object_bytes
        .map_or(DEFAULT_MAX_OBJECT_BYTES, |n| n as usize);
    let ttl = Duration::from_secs(config.ttl_secs.unwrap_or(DEFAULT_TTL_SECS));
    match config.kind {
        MediaStoreKind::File => {
            let path = config.path.as_deref().ok_or("path is required")?;
            Ok(Handle {
                store: Arc::new(file::FileStore::new(path, max)?),
                ttl,
            })
        }
        MediaStoreKind::S3 => {
            let ca = config
                .ca_file
                .as_ref()
                .map(|path| {
                    std::fs::read(path).map_err(|e| {
                        format!("ca_file: cannot read {}: {}", path.display(), e.kind())
                    })
                })
                .transpose()?;
            let prefix = config.prefix.clone().unwrap_or_default();
            let store = s3::S3Store::new(
                &s3::S3Settings {
                    bucket: config.bucket.as_deref().ok_or("bucket is required")?,
                    region: config.region.as_deref().ok_or("region is required")?,
                    endpoint: config.endpoint.as_deref(),
                    path_style: config.path_style,
                    prefix: &prefix,
                    access_key_id: config
                        .access_key_id
                        .as_ref()
                        .ok_or("access_key_id is required")?,
                    secret_access_key: config
                        .secret_access_key
                        .as_ref()
                        .ok_or("secret_access_key is required")?,
                    session_token: config.session_token.as_ref(),
                    ca_pem: ca.as_deref(),
                    max_object_bytes: max,
                },
                env,
            )?;
            Ok(Handle {
                store: Arc::new(store),
                ttl,
            })
        }
    }
}

/// A string at least this long moves to the store.
pub const THRESHOLD_BYTES: usize = 32 * 1024;
/// The most objects one job or answer moves.
pub const MAX_OBJECTS: usize = 4;
/// What sealing adds to a plaintext: the authentication tag.
const TAG_BYTES: usize = 16;
/// The key of the one-member object that stands for a moved string.
pub const REF_KEY: &str = "$blob";

/// Why a store call failed. A class, never a message that could carry a name, a credential or a
/// response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobError {
    /// No such object.
    NotFound,
    /// The store did not answer, or answered with a server error.
    Unavailable,
    /// The store refused the credentials or the action.
    Denied,
    /// More than the configured object size.
    TooLarge,
    /// An object that is not what its reference says (tampered, truncated, the wrong key).
    Corrupt,
    /// A name or a request the store cannot take.
    Invalid,
}

impl BlobError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
            Self::Denied => "denied",
            Self::TooLarge => "too_large",
            Self::Corrupt => "corrupt",
            Self::Invalid => "invalid",
        }
    }
}

/// One object found by [`BlobStore::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub key: String,
    pub modified: SystemTime,
}

/// Where sealed objects live. Names are the store's own (`prefix/date/random`): letters, digits and
/// `._-/`, validated by [`valid_name`] before any backend sees one.
#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<(), BlobError>;
    /// The object, or [`BlobError::TooLarge`] if it holds more than `max` bytes.
    async fn get(&self, name: &str, max: usize) -> Result<Vec<u8>, BlobError>;
    /// Deleting an object that is not there is not an error.
    async fn delete(&self, name: &str) -> Result<(), BlobError>;
    /// Every object under the store's prefix, for the sweeper.
    async fn list(&self) -> Result<Vec<Listed>, BlobError>;
    /// The most one object may hold (plaintext).
    fn max_object_bytes(&self) -> usize;
}

/// A name a backend may be given: 1 to 256 of `A-Za-z0-9._-/`, no empty segment, no `.` or `..`
/// segment, not starting with `/`.
pub fn valid_name(name: &str) -> bool {
    (1..=256).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// `<prefix><UTC date>/<128 random bits, hex>`. A backend adds its own prefix; callers pass `""`.
pub fn new_name(prefix: &str) -> String {
    let mut random = [0u8; 16];
    SystemRandom::new()
        .fill(&mut random)
        .expect("the system random source");
    let day = chrono::Utc::now().format("%Y%m%d");
    format!("{prefix}{day}/{}", sigv4::hex(&random))
}

/// A sealed object and what opens it.
struct Sealed {
    ciphertext: Vec<u8>,
    key: [u8; 32],
    nonce: [u8; 12],
}

fn seal(name: &str, plaintext: &[u8]) -> Sealed {
    let rng = SystemRandom::new();
    let (mut key, mut nonce) = ([0u8; 32], [0u8; 12]);
    rng.fill(&mut key).expect("the system random source");
    rng.fill(&mut nonce).expect("the system random source");
    let sealing = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).expect("a 256-bit key"));
    let mut buffer = plaintext.to_vec();
    sealing
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(name.as_bytes()),
            &mut buffer,
        )
        .expect("sealing a buffer");
    Sealed {
        ciphertext: buffer,
        key,
        nonce,
    }
}

fn open(
    name: &str,
    mut ciphertext: Vec<u8>,
    key: &[u8],
    nonce: &[u8],
) -> Result<Vec<u8>, BlobError> {
    let key = UnboundKey::new(&AES_256_GCM, key).map_err(|_| BlobError::Corrupt)?;
    let nonce: [u8; 12] = nonce.try_into().map_err(|_| BlobError::Corrupt)?;
    let plaintext = LessSafeKey::new(key)
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(name.as_bytes()),
            &mut ciphertext,
        )
        .map_err(|_| BlobError::Corrupt)?
        .len();
    ciphertext.truncate(plaintext);
    Ok(ciphertext)
}

/// What replaced a moved string: where it is, how long it was, and the key and nonce that open it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    pub name: String,
    pub len: usize,
    key: Vec<u8>,
    nonce: Vec<u8>,
}

impl BlobRef {
    pub fn to_value(&self) -> Value {
        json!({REF_KEY: {
            "v": 1, "key": self.name, "len": self.len,
            "k": STANDARD.encode(&self.key), "n": STANDARD.encode(&self.nonce),
        }})
    }

    /// The reference a value is, if it is exactly `{"$blob": {...}}` of version 1 with a valid name.
    pub fn from_value(value: &Value) -> Option<Self> {
        let outer = value.as_object().filter(|map| map.len() == 1)?;
        let inner = outer.get(REF_KEY)?.as_object()?;
        if inner.get("v")?.as_u64()? != 1 || inner.len() != 5 {
            return None;
        }
        let name = inner.get("key")?.as_str()?.to_owned();
        let len = usize::try_from(inner.get("len")?.as_u64()?).ok()?;
        let key = STANDARD.decode(inner.get("k")?.as_str()?).ok()?;
        let nonce = STANDARD.decode(inner.get("n")?.as_str()?).ok()?;
        (valid_name(&name) && key.len() == 32 && nonce.len() == 12).then_some(Self {
            name,
            len,
            key,
            nonce,
        })
    }
}

/// Seal `text` and put it under a new name in `store`.
pub async fn put_sealed(
    store: &dyn BlobStore,
    prefix: &str,
    text: &str,
) -> Result<BlobRef, BlobError> {
    if text.len() > store.max_object_bytes() {
        return Err(BlobError::TooLarge);
    }
    let name = new_name(prefix);
    let sealed = seal(&name, text.as_bytes());
    store.put(&name, sealed.ciphertext).await?;
    Ok(BlobRef {
        name,
        len: text.len(),
        key: sealed.key.to_vec(),
        nonce: sealed.nonce.to_vec(),
    })
}

/// Read, open and check an object: its length is the reference's, and it is text.
pub async fn get_sealed(store: &dyn BlobStore, reference: &BlobRef) -> Result<String, BlobError> {
    if reference.len > store.max_object_bytes() {
        return Err(BlobError::TooLarge);
    }
    let ciphertext = store
        .get(&reference.name, reference.len + TAG_BYTES)
        .await?;
    let plaintext = open(
        &reference.name,
        ciphertext,
        &reference.key,
        &reference.nonce,
    )?;
    if plaintext.len() != reference.len {
        return Err(BlobError::Corrupt);
    }
    String::from_utf8(plaintext).map_err(|_| BlobError::Corrupt)
}

/// The JSON pointers of every string in `value` that is at least `threshold` bytes.
fn large_strings(value: &Value, threshold: usize) -> Vec<String> {
    fn escape(part: &str) -> String {
        part.replace('~', "~0").replace('/', "~1")
    }
    fn walk(value: &Value, at: &mut String, out: &mut Vec<String>, threshold: usize) {
        match value {
            Value::String(text) if text.len() >= threshold => out.push(at.clone()),
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    let keep = at.len();
                    at.push_str(&format!("/{index}"));
                    walk(item, at, out, threshold);
                    at.truncate(keep);
                }
            }
            Value::Object(map) => {
                for (key, item) in map {
                    let keep = at.len();
                    at.push('/');
                    at.push_str(&escape(key));
                    walk(item, at, out, threshold);
                    at.truncate(keep);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(value, &mut String::new(), &mut out, threshold);
    out
}

/// Whether `value` holds a reference anywhere. A caller's own request must not: a reference is
/// only ever made by a replica, so one in a caller's body is refused before it can name an object.
pub fn contains_ref(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.contains_key(REF_KEY) || map.values().any(contains_ref),
        Value::Array(items) => items.iter().any(contains_ref),
        _ => false,
    }
}

/// The names a request has put in the store, so they can all be deleted when it ends.
#[derive(Debug, Default)]
pub struct Staged {
    names: Vec<String>,
}

impl Staged {
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Delete every object, best effort: an object that cannot be deleted is left to the sweeper.
    /// Returns how many could not be deleted.
    pub async fn release(self, store: &dyn BlobStore) -> usize {
        let mut failed = 0;
        for name in &self.names {
            if store.delete(name).await.is_err() {
                failed += 1;
            }
        }
        failed
    }
}

/// Move every string of `threshold` bytes or more in `value` to the store, leaving references. At
/// most [`MAX_OBJECTS`] move (a job with more is refused whole, with nothing left behind).
pub async fn externalize(
    store: &dyn BlobStore,
    prefix: &str,
    value: &mut Value,
    threshold: usize,
) -> Result<Staged, BlobError> {
    let pointers = large_strings(value, threshold);
    if pointers.len() > MAX_OBJECTS {
        return Err(BlobError::TooLarge);
    }
    let mut staged = Staged::default();
    for pointer in pointers {
        let Some(slot) = value.pointer_mut(&pointer) else {
            continue;
        };
        let text = slot.as_str().unwrap_or_default().to_owned();
        match put_sealed(store, prefix, &text).await {
            Ok(reference) => {
                staged.names.push(reference.name.clone());
                *slot = reference.to_value();
            }
            Err(error) => {
                // Nothing of this job may be left behind.
                staged.release(store).await;
                return Err(error);
            }
        }
    }
    Ok(staged)
}

/// Replace every reference in `value` with the string it stands for. The references found are
/// returned (the caller deletes the objects when the request ends); `max_total` bounds what is read.
pub async fn internalize(
    store: &dyn BlobStore,
    value: &mut Value,
    max_total: usize,
) -> Result<Vec<BlobRef>, BlobError> {
    fn find(value: &Value, at: &mut String, out: &mut Vec<(String, BlobRef)>) {
        if let Some(reference) = BlobRef::from_value(value) {
            out.push((at.clone(), reference));
            return;
        }
        match value {
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    let keep = at.len();
                    at.push_str(&format!("/{index}"));
                    find(item, at, out);
                    at.truncate(keep);
                }
            }
            Value::Object(map) => {
                for (key, item) in map {
                    let keep = at.len();
                    at.push('/');
                    at.push_str(&key.replace('~', "~0").replace('/', "~1"));
                    find(item, at, out);
                    at.truncate(keep);
                }
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    find(value, &mut String::new(), &mut found);
    if found.len() > MAX_OBJECTS || found.iter().map(|(_, r)| r.len).sum::<usize>() > max_total {
        return Err(BlobError::TooLarge);
    }
    let mut refs = Vec::new();
    for (pointer, reference) in found {
        let text = get_sealed(store, &reference).await?;
        if let Some(slot) = value.pointer_mut(&pointer) {
            *slot = Value::String(text);
        }
        refs.push(reference);
    }
    Ok(refs)
}

/// Delete the objects older than `ttl`: the orphans of a replica that died mid-request. Returns how
/// many were deleted; an object that cannot be deleted is tried again at the next sweep.
pub async fn sweep(store: &dyn BlobStore, ttl: Duration) -> Result<usize, BlobError> {
    let cutoff = SystemTime::now()
        .checked_sub(ttl)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut deleted = 0;
    for object in store.list().await? {
        if object.modified < cutoff
            && valid_name(&object.key)
            && store.delete(&object.key).await.is_ok()
        {
            deleted += 1;
        }
    }
    Ok(deleted)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// An in-memory store that can be told to fail and that records what was deleted.
    #[derive(Default)]
    pub(crate) struct Memory {
        pub objects: Mutex<BTreeMap<String, (Vec<u8>, SystemTime)>>,
        pub fail_puts_after: Mutex<Option<usize>>,
        pub deleted: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl BlobStore for Memory {
        async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<(), BlobError> {
            let mut limit = self.fail_puts_after.lock().unwrap();
            if let Some(left) = limit.as_mut() {
                if *left == 0 {
                    return Err(BlobError::Unavailable);
                }
                *left -= 1;
            }
            self.objects
                .lock()
                .unwrap()
                .insert(name.to_owned(), (bytes, SystemTime::now()));
            Ok(())
        }
        async fn get(&self, name: &str, max: usize) -> Result<Vec<u8>, BlobError> {
            let objects = self.objects.lock().unwrap();
            let (bytes, _) = objects.get(name).ok_or(BlobError::NotFound)?;
            (bytes.len() <= max)
                .then(|| bytes.clone())
                .ok_or(BlobError::TooLarge)
        }
        async fn delete(&self, name: &str) -> Result<(), BlobError> {
            self.deleted.lock().unwrap().push(name.to_owned());
            self.objects.lock().unwrap().remove(name);
            Ok(())
        }
        async fn list(&self) -> Result<Vec<Listed>, BlobError> {
            Ok(self
                .objects
                .lock()
                .unwrap()
                .iter()
                .map(|(key, (_, modified))| Listed {
                    key: key.clone(),
                    modified: *modified,
                })
                .collect())
        }
        fn max_object_bytes(&self) -> usize {
            1 << 20
        }
    }

    fn big(fill: char, bytes: usize) -> String {
        std::iter::repeat_n(fill, bytes).collect()
    }

    #[tokio::test]
    async fn a_sealed_object_opens_and_is_unreadable_in_the_store() {
        let store = Memory::default();
        let text = format!("secret caller audio {}", big('A', 40_000));
        let reference = put_sealed(&store, "media/", &text).await.unwrap();
        assert_eq!(get_sealed(&store, &reference).await.unwrap(), text);
        // The store holds ciphertext: no run of the plaintext, and a name that says nothing.
        let (stored, _) = store
            .objects
            .lock()
            .unwrap()
            .values()
            .next()
            .cloned()
            .unwrap();
        assert_eq!(stored.len(), text.len() + TAG_BYTES);
        assert!(!String::from_utf8_lossy(&stored).contains("secret caller audio"));
        assert!(reference.name.starts_with("media/") && valid_name(&reference.name));
        assert_eq!(reference.name.rsplit('/').next().unwrap().len(), 32);
        // A second object of the same text has another name, key and ciphertext.
        let again = put_sealed(&store, "media/", &text).await.unwrap();
        assert_ne!(again.name, reference.name);
        assert_ne!(again.key, reference.key);
    }

    #[tokio::test]
    async fn every_way_an_object_can_be_wrong_is_refused_not_returned() {
        let store = Memory::default();
        let reference = put_sealed(&store, "", "hello, sealed world").await.unwrap();
        let key = reference.name.clone();
        let original = store.objects.lock().unwrap()[&key].0.clone();
        let refuse = |label: &str, result: Result<String, BlobError>| {
            assert_eq!(result, Err(BlobError::Corrupt), "{label}");
        };
        // A flipped bit anywhere: the ciphertext or the tag.
        for index in [0, original.len() / 2, original.len() - 1] {
            let mut bytes = original.clone();
            bytes[index] ^= 1;
            store.objects.lock().unwrap().get_mut(&key).unwrap().0 = bytes;
            refuse("flipped bit", get_sealed(&store, &reference).await);
        }
        store.objects.lock().unwrap().get_mut(&key).unwrap().0 = original.clone();
        // The wrong key, the wrong nonce.
        let mut wrong = reference.clone();
        wrong.key[0] ^= 1;
        refuse("key", get_sealed(&store, &wrong).await);
        let mut wrong = reference.clone();
        wrong.nonce[0] ^= 1;
        refuse("nonce", get_sealed(&store, &wrong).await);
        // The same ciphertext under another name (a swapped object) does not open: the name is the
        // associated data.
        let moved = BlobRef {
            name: "other/name".into(),
            ..reference.clone()
        };
        store
            .objects
            .lock()
            .unwrap()
            .insert(moved.name.clone(), (original.clone(), SystemTime::now()));
        refuse("swapped name", get_sealed(&store, &moved).await);
        // A length that is not the object's.
        let mut wrong = reference.clone();
        wrong.len += 1;
        assert!(get_sealed(&store, &wrong).await.is_err());
        // A truncated object.
        store.objects.lock().unwrap().get_mut(&key).unwrap().0 = original[..5].to_vec();
        refuse("truncated", get_sealed(&store, &reference).await);
        // A missing one.
        store.objects.lock().unwrap().remove(&key);
        assert_eq!(
            get_sealed(&store, &reference).await,
            Err(BlobError::NotFound)
        );
    }

    #[test]
    fn a_reference_round_trips_and_only_the_exact_shape_is_one() {
        let reference = BlobRef {
            name: "p/20260101/ab".into(),
            len: 7,
            key: vec![1; 32],
            nonce: vec![2; 12],
        };
        assert_eq!(
            BlobRef::from_value(&reference.to_value()),
            Some(reference.clone())
        );
        for not in [
            json!("text"),
            json!({"$blob": {}}),
            json!({"$blob": 1}),
            // Another member beside it, a wrong version, a bad name, a short key or nonce.
            {
                let mut v = reference.to_value();
                v["extra"] = json!(1);
                v
            },
            {
                let mut v = reference.to_value();
                v["$blob"]["v"] = json!(2);
                v
            },
            {
                let mut v = reference.to_value();
                v["$blob"]["key"] = json!("../etc/passwd");
                v
            },
            {
                let mut v = reference.to_value();
                v["$blob"]["k"] = json!("AAAA");
                v
            },
            {
                let mut v = reference.to_value();
                v["$blob"]["n"] = json!("AAAA");
                v
            },
            {
                let mut v = reference.to_value();
                v["$blob"]["more"] = json!(1);
                v
            },
        ] {
            assert_eq!(BlobRef::from_value(&not), None, "{not}");
        }
        // A caller's request must not carry one anywhere.
        assert!(contains_ref(&json!({"a": [{"b": reference.to_value()}]})));
        assert!(contains_ref(&json!({"x": {"$blob": "anything"}})));
        assert!(!contains_ref(&json!({"a": ["$blob", {"b": 1}]})));
    }

    #[tokio::test]
    async fn only_large_strings_move_and_the_value_comes_back_whole() {
        let store = Memory::default();
        let audio = big('Q', THRESHOLD_BYTES);
        let image = big('Z', THRESHOLD_BYTES + 9);
        let original = json!({
            "model": "m", "small": big('s', THRESHOLD_BYTES - 1),
            "input": {"audio_b64": audio, "list": [1, "two", {"~odd/key": image}]},
        });
        let mut value = original.clone();
        let staged = externalize(&store, "media/", &mut value, THRESHOLD_BYTES)
            .await
            .unwrap();
        assert_eq!(staged.names().len(), 2);
        assert_eq!(
            value["small"], original["small"],
            "a string just under stays"
        );
        assert!(BlobRef::from_value(&value["input"]["audio_b64"]).is_some());
        assert!(!value.to_string().contains("QQQQQQQQ"));
        let refs = internalize(&store, &mut value, usize::MAX).await.unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(value, original);
        // The request ends: everything it put is deleted.
        assert_eq!(staged.release(&store).await, 0);
        assert!(store.objects.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_job_that_cannot_be_staged_leaves_nothing_behind() {
        let store = Memory::default();
        let mut value = json!({"a": big('a', THRESHOLD_BYTES), "b": big('b', THRESHOLD_BYTES)});
        *store.fail_puts_after.lock().unwrap() = Some(1);
        let before = value.clone();
        assert_eq!(
            externalize(&store, "", &mut value, THRESHOLD_BYTES)
                .await
                .err(),
            Some(BlobError::Unavailable)
        );
        // The first object was put and then deleted again.
        assert!(store.objects.lock().unwrap().is_empty());
        assert_eq!(store.deleted.lock().unwrap().len(), 1);
        let _ = before;
        // Too many large strings, and one larger than the store takes, are refused whole.
        let mut many = json!({"a": big('a', 10), "b": big('b', 10), "c": big('c', 10),
            "d": big('d', 10), "e": big('e', 10)});
        assert_eq!(
            externalize(&store, "", &mut many, 10).await.err(),
            Some(BlobError::TooLarge)
        );
        let mut huge = json!({"a": big('a', (1 << 20) + 1)});
        assert_eq!(
            externalize(&store, "", &mut huge, 10).await.err(),
            Some(BlobError::TooLarge)
        );
        assert!(store.objects.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn internalizing_is_bounded_and_a_foreign_reference_cannot_be_read() {
        let store = Memory::default();
        let mut value = json!({"a": big('a', THRESHOLD_BYTES)});
        externalize(&store, "", &mut value, THRESHOLD_BYTES)
            .await
            .unwrap();
        // More than the caller will accept is refused before anything is read.
        assert_eq!(
            internalize(&store, &mut value.clone(), 10).await.err(),
            Some(BlobError::TooLarge)
        );
        // A reference naming a real object with a guessed key does not open it.
        let reference = BlobRef::from_value(&value["a"]).unwrap();
        let forged = BlobRef {
            key: vec![9; 32],
            ..reference
        };
        let mut attack = json!({"x": forged.to_value()});
        assert_eq!(
            internalize(&store, &mut attack, usize::MAX).await.err(),
            Some(BlobError::Corrupt)
        );
    }

    #[tokio::test]
    async fn the_sweeper_deletes_only_what_is_older_than_the_ttl() {
        let store = Arc::new(Memory::default());
        let old = SystemTime::now() - Duration::from_secs(7200);
        store
            .objects
            .lock()
            .unwrap()
            .insert("p/20260101/old".into(), (vec![1], old));
        store
            .objects
            .lock()
            .unwrap()
            .insert("p/20260101/new".into(), (vec![1], SystemTime::now()));
        assert_eq!(
            sweep(store.as_ref(), Duration::from_secs(3600)).await,
            Ok(1)
        );
        let left: Vec<String> = store.objects.lock().unwrap().keys().cloned().collect();
        assert_eq!(left, ["p/20260101/new"]);
    }

    #[test]
    fn a_name_is_plain_and_cannot_climb() {
        for ok in ["a", "p/20260101/ab12", "x.y_z-1/a"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "/a",
            "a/",
            "a//b",
            "../a",
            "a/../b",
            "a/./b",
            "a b",
            "a\\b",
            "é",
            &"a".repeat(257),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}

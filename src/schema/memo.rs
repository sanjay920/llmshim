//! Process-local schema memoization. Never retain whole requests or credentials.
use super::{budget, Normalization, Options};
use serde_json::Value;
use std::{
    collections::{hash_map::RandomState, HashMap},
    hash::{BuildHasher, Hash, Hasher},
    sync::{Arc, LazyLock, Mutex},
};

pub(super) static CACHE: LazyLock<Memo> = LazyLock::new(|| Memo::new(512, 16 * 1024 * 1024));

pub(super) struct Cached {
    pub value: Value,
    pub report: Normalization,
    pub unchanged: bool,
    pub footprint: budget::Footprint,
}
struct Entry {
    input: Value,
    options: Options,
    result: Arc<Cached>,
    last_used: u64,
    bytes: usize,
}
#[derive(Default)]
struct Entries {
    values: HashMap<u64, Entry>,
    clock: u64,
    bytes: usize,
}
pub(super) struct Memo {
    entries: Mutex<Entries>,
    hasher: RandomState,
    max_entries: usize,
    max_bytes: usize,
}
pub(super) struct Key {
    hash: u64,
    input_bytes: usize,
}

impl Memo {
    pub(super) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: Mutex::new(Entries::default()),
            hasher: RandomState::new(),
            max_entries,
            max_bytes,
        }
    }

    pub(super) fn key(&self, options: &Options, schema: &Value) -> Option<Key> {
        let mut hasher = self.hasher.build_hasher();
        options.hash(&mut hasher);
        let input_bytes = fingerprint(schema, &mut hasher)?;
        Some(Key {
            hash: hasher.finish(),
            input_bytes,
        })
    }

    pub(super) fn get(&self, key: &Key, options: &Options, schema: &Value) -> Option<Arc<Cached>> {
        let mut entries = self.entries.lock().ok()?;
        entries.clock = entries.clock.saturating_add(1);
        let clock = entries.clock;
        let entry = entries.values.get_mut(&key.hash)?;
        // Hash collisions may cost a miss, but can never change the result.
        if entry.options != *options || !same_input(&entry.input, schema) {
            return None;
        }
        entry.last_used = clock;
        Some(entry.result.clone())
    }

    pub(super) fn insert(
        &self,
        key: Key,
        options: Options,
        input: Value,
        value: &Value,
        report: Normalization,
    ) {
        let Some(output_bytes) = fingerprint(value, &mut self.hasher.build_hasher()) else {
            return;
        };
        let Ok(footprint) = budget::measure(value) else {
            return;
        };
        // Conservative owned-value estimate plus entry/hash-table bookkeeping.
        let bytes = key
            .input_bytes
            .saturating_add(output_bytes)
            .saturating_add(512);
        if self.max_entries == 0 || bytes > self.max_bytes {
            return;
        }
        let unchanged = same_input(&input, value);
        let entry = Entry {
            input,
            options,
            result: Arc::new(Cached {
                value: value.clone(),
                report,
                unchanged,
                footprint,
            }),
            last_used: 0,
            bytes,
        };
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if let Some(old) = entries.values.remove(&key.hash) {
            entries.bytes -= old.bytes;
        }
        while entries.values.len() >= self.max_entries
            || entries.bytes.saturating_add(bytes) > self.max_bytes
        {
            let oldest = entries
                .values
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key);
            let Some(oldest) = oldest else {
                break;
            };
            let old = entries.values.remove(&oldest).unwrap();
            entries.bytes -= old.bytes;
        }
        entries.clock = entries.clock.saturating_add(1);
        let mut entry = entry;
        entry.last_used = entries.clock;
        entries.bytes += bytes;
        entries.values.insert(key.hash, entry);
    }
}

/// Bound key construction independently of caller-selected walker limits. Larger
/// inputs simply take the ordinary normalization path. Hash object iteration
/// order too, for builds with serde_json's preserve_order feature enabled.
fn fingerprint(schema: &Value, hasher: &mut impl Hasher) -> Option<usize> {
    let mut pending = vec![(schema, 0usize)];
    let mut nodes = 0usize;
    let mut literal_bytes = 0usize;
    let mut owned_bytes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if depth > 96 || nodes > 32_768 {
            return None;
        }
        owned_bytes = owned_bytes.saturating_add(64);
        match value {
            Value::Object(object) => {
                0u8.hash(hasher);
                object.len().hash(hasher);
                if nodes + pending.len() + object.len() > 32_768 {
                    return None;
                }
                owned_bytes = owned_bytes.saturating_add(576);
                for (key, value) in object.iter().rev() {
                    literal_bytes = literal_bytes.saturating_add(key.len());
                    if literal_bytes > 8 * 1024 * 1024 {
                        return None;
                    }
                    key.hash(hasher);
                    owned_bytes = owned_bytes.saturating_add(key.len()).saturating_add(32);
                    pending.push((value, depth + 1));
                }
            }
            Value::Array(array) => {
                1u8.hash(hasher);
                array.len().hash(hasher);
                if nodes + pending.len() + array.len() > 32_768 {
                    return None;
                }
                pending.extend(array.iter().rev().map(|value| (value, depth + 1)));
            }
            Value::String(text) => {
                2u8.hash(hasher);
                literal_bytes = literal_bytes.saturating_add(text.len());
                if literal_bytes > 8 * 1024 * 1024 {
                    return None;
                }
                text.hash(hasher);
                owned_bytes = owned_bytes.saturating_add(text.len()).saturating_add(32);
            }
            scalar => {
                3u8.hash(hasher);
                scalar.hash(hasher);
                if let Value::Number(number) = scalar {
                    // Value equality treats signed zero as equal, but spill-to-
                    // description preserves its spelling. Include the sign bit.
                    number.as_f64().map(f64::to_bits).hash(hasher);
                }
            }
        }
        if literal_bytes > 8 * 1024 * 1024 {
            return None;
        }
    }
    Some(owned_bytes)
}

fn same_input(a: &Value, b: &Value) -> bool {
    let mut pending = vec![(a, b)];
    while let Some((a, b)) = pending.pop() {
        match (a, b) {
            (Value::Object(a), Value::Object(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                for ((ak, av), (bk, bv)) in a.iter().zip(b) {
                    if ak != bk {
                        return false;
                    }
                    pending.push((av, bv));
                }
            }
            (Value::Array(a), Value::Array(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                pending.extend(a.iter().zip(b));
            }
            (Value::Number(a), Value::Number(b))
                if a != b || a.as_f64().map(f64::to_bits) != b.as_f64().map(f64::to_bits) =>
            {
                return false;
            }
            _ if a != b => return false,
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{normalize_uncached, Nullable, Target};
    use serde_json::json;

    fn run(
        cache: &Memo,
        options: &Options,
        input: &Value,
        builds: &mut usize,
    ) -> (Value, Normalization) {
        let key = cache.key(options, input).unwrap();
        if let Some(hit) = cache.get(&key, options, input) {
            return (hit.value.clone(), hit.report);
        }
        *builds += 1;
        let mut value = input.clone();
        let report = normalize_uncached(options, &mut value);
        cache.insert(key, options.clone(), input.clone(), &value, report);
        (value, report)
    }

    #[test]
    fn signed_zero_defaults_keep_their_own_spilled_descriptions() {
        let cache = Memo::new(8, 1024 * 1024);
        let mut options = Options::for_target(Target::OpenAiResponses);
        options.strict = true;
        let positive = json!({"type":"object","properties":{"n":{"type":"number","default":0.0}}});
        let negative = json!({"type":"object","properties":{"n":{"type":"number","default":-0.0}}});
        let mut builds = 0;
        let first = run(&cache, &options, &positive, &mut builds);
        let second = run(&cache, &options, &negative, &mut builds);
        assert_ne!(
            first.0["properties"]["n"]["description"],
            second.0["properties"]["n"]["description"]
        );
        assert_eq!(builds, 2);
        let collision = cache.key(&options, &positive).unwrap();
        assert!(cache.get(&collision, &options, &negative).is_none());
    }

    #[test]
    fn repeated_inputs_reuse_work_and_return_independent_values_and_reports() {
        let cache = Memo::new(512, 16 * 1024 * 1024);
        let mut options = Options::for_target(Target::OpenAiResponses);
        options.strict = true;
        let input = json!({"type":"object","properties":{"x":{"type":"string","default":"value"}}});
        let mut builds = 0;
        let expected = run(&cache, &options, &input, &mut builds);
        let mut returned = run(&cache, &options, &input, &mut builds);
        returned.0["properties"]["x"] = json!({"type":"number"});
        assert_eq!(run(&cache, &options, &input, &mut builds), expected);
        assert_eq!(builds, 1);
        assert!(expected.1.changed && expected.1.strict);
        let normalized = run(&cache, &options, &expected.0, &mut builds);
        assert!(!normalized.1.changed);
        assert_eq!(builds, 2);
        let invalid = json!({"$ref":"https://example.invalid/external"});
        let fallback = run(&cache, &options, &invalid, &mut builds);
        assert!(fallback.1.used_fallback && !fallback.1.strict);
        assert_eq!(run(&cache, &options, &invalid, &mut builds), fallback);
        assert_eq!(builds, 3);
    }

    #[test]
    fn every_policy_field_and_input_changes_the_key_and_collisions_miss() {
        let cache = Memo::new(32, 1024 * 1024);
        let options = Options::for_target(Target::OpenAiResponses);
        let schema = json!({"type":"object"});
        let key = cache.key(&options, &schema).unwrap();
        let original_hash = key.hash;
        cache.insert(
            key,
            options.clone(),
            schema.clone(),
            &schema,
            Normalization::default(),
        );
        let mut variants = Vec::new();
        let mut value = options.clone();
        value.strict = true;
        variants.push(value);
        let mut value = options.clone();
        value.target = Target::Anthropic;
        variants.push(value);
        let mut value = options.clone();
        value.nullable = Nullable::Marker;
        variants.push(value);
        let mut value = options.clone();
        value.rewrite_one_of = false;
        variants.push(value);
        let mut value = options.clone();
        value.strip_lookarounds = false;
        variants.push(value);
        let mut value = options.clone();
        value.bypass = true;
        variants.push(value);
        let mut value = options.clone();
        value.max_depth -= 1;
        variants.push(value);
        let mut value = options.clone();
        value.max_nodes -= 1;
        variants.push(value);
        let mut value = options.clone();
        value.max_literal_bytes -= 1;
        variants.push(value);
        for variant in variants {
            assert_ne!(cache.key(&variant, &schema).unwrap().hash, original_hash);
            assert!(cache
                .get(
                    &Key {
                        hash: original_hash,
                        input_bytes: 0
                    },
                    &variant,
                    &schema
                )
                .is_none());
        }
        let different = json!({"type":"string"});
        assert!(cache
            .get(
                &Key {
                    hash: original_hash,
                    input_bytes: 0
                },
                &options,
                &different
            )
            .is_none());
    }

    #[test]
    fn eviction_enforces_entry_and_byte_limits_and_keeps_recent_entries() {
        let cache = Memo::new(2, 1024 * 1024);
        let options = Options::for_target(Target::Mcp);
        let mut builds = 0;
        let a = json!({"type":"string"});
        let b = json!({"type":"number"});
        let c = json!({"type":"boolean"});
        run(&cache, &options, &a, &mut builds);
        run(&cache, &options, &b, &mut builds);
        run(&cache, &options, &a, &mut builds);
        run(&cache, &options, &c, &mut builds);
        assert!(cache
            .get(&cache.key(&options, &b).unwrap(), &options, &b)
            .is_none());
        assert_eq!(builds, 3);
        assert!(cache
            .get(&cache.key(&options, &a).unwrap(), &options, &a)
            .is_some());
        let charge = cache.key(&options, &a).unwrap().input_bytes * 2 + 512;
        let small = Memo::new(10, charge);
        run(&small, &options, &a, &mut builds);
        run(&small, &options, &b, &mut builds);
        let entries = small.entries.lock().unwrap();
        assert!(entries.bytes <= charge);
        assert_eq!(entries.values.len(), 1);
        drop(entries);
        let tiny = Memo::new(10, 1);
        run(&tiny, &options, &a, &mut builds);
        assert!(tiny.entries.lock().unwrap().values.is_empty());
    }

    #[test]
    fn key_work_is_bounded_and_shared_hits_are_thread_safe() {
        let cache = Memo::new(2, 1024 * 1024);
        let options = Options::for_target(Target::Mcp);
        let input = json!({"type":"string"});
        let mut builds = 0;
        let expected = run(&cache, &options, &input, &mut builds);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let cache = &cache;
                let options = &options;
                let input = &input;
                let expected = &expected;
                scope.spawn(move || {
                    let mut local_builds = 0;
                    for _ in 0..50 {
                        assert_eq!(&run(cache, options, input, &mut local_builds), expected);
                    }
                    assert_eq!(local_builds, 0);
                });
            }
        });
        let mut deep = Value::Null;
        for _ in 0..100 {
            deep = json!([deep]);
        }
        assert!(cache.key(&options, &deep).is_none());
        assert!(cache.key(&options, &json!(vec![0; 32_769])).is_none());
    }
}

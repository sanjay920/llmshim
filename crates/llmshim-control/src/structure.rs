//! Prompt structure and the cache debugger (design note 0052, slice 5a).
//!
//! A gateway whose client opted in sends, with each usage record, the prompt cut into segments: a kind, a keyed
//! fingerprint and a size. Nothing readable. This module stores those lists beside the request ledger and
//! compares two requests' lists to find where the cached prefix first changed. The fingerprints are keyed with a
//! secret this crate never has: it can tell two segments are equal and can never compute one.

use crate::db::{i, t};
use crate::{Result, Store};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The longest list one request may carry.
pub const MAX_SEGMENTS: usize = 2048;
/// The kinds a segment can have.
pub const KINDS: [&str; 7] = [
    "tool",
    "system",
    "user",
    "assistant",
    "tool_result",
    "image",
    "other",
];
/// How far back a request's predecessor may be: a provider's longest prompt-cache lifetime.
pub const WINDOW_MS: i64 = 3_600_000;
/// The shorter lifetime (five minutes): a predecessor older than this may have expired from the cache already.
pub const SHORT_WINDOW_MS: i64 = 300_000;
/// The most stored lists one aggregate reads.
const MAX_SCAN: i64 = 20_000;

/// The table, for a fresh database and for the migration to version 7.
pub(crate) fn schema(pg: bool) -> String {
    let int = if pg { "BIGINT" } else { "INTEGER" };
    format!(
        "
CREATE TABLE IF NOT EXISTS request_structure (
    attempt_id TEXT PRIMARY KEY,
    ts_ms {int} NOT NULL,
    key_name TEXT NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL,
    segments TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS request_structure_by_client ON request_structure (key_name, provider, model, ts_ms);
CREATE INDEX IF NOT EXISTS request_structure_by_time ON request_structure (ts_ms);
"
    )
}

/// One part of a prompt: what it is, its keyed fingerprint, its size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Segment {
    pub kind: String,
    pub hash: String,
    pub bytes: u64,
}

/// The segments of a record's `structure` field, or `None` when it is absent or not what a gateway sends (a
/// different version, a hand-made record). The row it rides on is still stored: spend must not be lost to a
/// field that is only for debugging.
pub(crate) fn parse(value: Option<&Value>) -> Option<Vec<Segment>> {
    let items = value?.as_array()?;
    if items.len() > MAX_SEGMENTS {
        return None;
    }
    items
        .iter()
        .map(|item| {
            let object = item.as_object()?;
            let kind = object.get("kind")?.as_str()?;
            let hash = object.get("hash")?.as_str()?;
            let bytes = object.get("bytes")?.as_u64()?;
            let valid = KINDS.contains(&kind)
                && hash.len() == 16
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && bytes <= 1_000_000_000_000;
            valid.then(|| Segment {
                kind: kind.to_owned(),
                hash: hash.to_owned(),
                bytes,
            })
        })
        .collect()
}

pub(crate) fn encode(segments: &[Segment]) -> String {
    Value::Array(
        segments
            .iter()
            .map(|s| json!([s.kind, s.hash, s.bytes]))
            .collect(),
    )
    .to_string()
}

fn decode(text: &str) -> Vec<Segment> {
    serde_json::from_str::<Vec<(String, String, u64)>>(text)
        .unwrap_or_default()
        .into_iter()
        .map(|(kind, hash, bytes)| Segment { kind, hash, bytes })
        .collect()
}

/// Where in a prompt a segment is, in words.
fn label(segments: &[Segment], index: usize) -> String {
    let kind = segments[index].kind.as_str();
    let ordinal = segments[..=index].iter().filter(|s| s.kind == kind).count();
    match kind {
        "tool" => format!("tool definition {ordinal}"),
        "system" if ordinal == 1 => "system prompt".to_owned(),
        "system" => format!("system block {ordinal}"),
        "user" => format!("user message (segment {})", index + 1),
        "assistant" => format!("assistant message (segment {})", index + 1),
        "tool_result" => format!("tool result (segment {})", index + 1),
        "image" => format!("image (segment {})", index + 1),
        _ => format!("segment {}", index + 1),
    }
}

/// How a request's prompt relates to the one before it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diff {
    /// `identical`, `appended` (it only added to the end: the normal growth of a conversation), `shorter` (it is a
    /// prefix of the earlier one) or `changed` (an earlier segment differs: a cache miss from that point on).
    pub relation: &'static str,
    pub common_segments: usize,
    pub common_bytes: u64,
    pub total_segments: usize,
    pub total_bytes: u64,
    /// For `changed`: the first segment that differs.
    pub first_change: Option<Change>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Change {
    pub index: usize,
    pub kind: String,
    pub label: String,
}

/// Compare `current` with the request `previous` before it.
pub fn diff(previous: &[Segment], current: &[Segment]) -> Diff {
    let common = previous
        .iter()
        .zip(current)
        .take_while(|(a, b)| a.kind == b.kind && a.hash == b.hash)
        .count();
    let relation = if common == previous.len() && common == current.len() {
        "identical"
    } else if common == previous.len() {
        "appended"
    } else if common == current.len() {
        "shorter"
    } else {
        "changed"
    };
    Diff {
        relation,
        common_segments: common,
        common_bytes: current[..common].iter().map(|s| s.bytes).sum(),
        total_segments: current.len(),
        total_bytes: current.iter().map(|s| s.bytes).sum(),
        first_change: (relation == "changed").then(|| Change {
            index: common,
            kind: current[common].kind.clone(),
            label: label(current, common),
        }),
    }
}

/// A stored list with the request it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct Stored {
    pub attempt_id: String,
    pub ts_ms: i64,
    pub key_name: String,
    pub provider: String,
    pub model: String,
    pub segments: Vec<Segment>,
}

impl Store {
    /// The structure stored for an attempt, if its client captured one and it has not aged out.
    pub fn request_structure(&mut self, attempt_id: &str) -> Result<Option<Stored>> {
        self.backend.transaction(false, |tx| {
            let rows = tx.query(
                "SELECT attempt_id, ts_ms, key_name, provider, model, segments FROM request_structure WHERE attempt_id = ?1",
                &[t(attempt_id)],
            )?;
            rows.first().map(stored).transpose()
        })
    }

    /// The request of the same client, provider and model just before `at`, within [`WINDOW_MS`].
    pub fn previous_structure(&mut self, at: &Stored) -> Result<Option<Stored>> {
        self.backend.transaction(false, |tx| {
            let rows = tx.query(
                "SELECT attempt_id, ts_ms, key_name, provider, model, segments FROM request_structure \
                 WHERE key_name = ?1 AND provider = ?2 AND model = ?3 AND ts_ms >= ?4 \
                 AND (ts_ms < ?5 OR (ts_ms = ?5 AND attempt_id < ?6)) \
                 ORDER BY ts_ms DESC, attempt_id DESC LIMIT 1",
                &[t(&at.key_name), t(&at.provider), t(&at.model), i(at.ts_ms - WINDOW_MS), i(at.ts_ms), t(&at.attempt_id)],
            )?;
            rows.first().map(stored).transpose()
        })
    }

    /// Where cache misses start: every consecutive pair of one client's requests (same provider and model, within
    /// [`WINDOW_MS`]) in the window, classified, and the `changed` ones counted by the kind of the first segment
    /// that differs.
    pub fn structure_misses(&mut self, from_ms: i64, to_ms: i64) -> Result<Misses> {
        let rows = self.backend.transaction(false, |tx| {
            tx.query(
                "SELECT attempt_id, ts_ms, key_name, provider, model, segments FROM request_structure \
                 WHERE ts_ms >= ?1 AND ts_ms < ?2 ORDER BY key_name, provider, model, ts_ms, attempt_id LIMIT ?3",
                &[i(from_ms), i(to_ms), i(MAX_SCAN + 1)],
            )?
            .iter()
            .map(stored)
            .collect::<Result<Vec<_>>>()
        })?;
        let truncated = rows.len() as i64 > MAX_SCAN;
        let mut total = Counts::default();
        let mut groups: BTreeMap<(String, String), Counts> = BTreeMap::new();
        let mut previous: Option<&Stored> = None;
        for row in rows.iter().take(MAX_SCAN as usize) {
            if let Some(before) = previous {
                let same = before.key_name == row.key_name
                    && before.provider == row.provider
                    && before.model == row.model;
                if same && row.ts_ms - before.ts_ms <= WINDOW_MS {
                    let d = diff(&before.segments, &row.segments);
                    total.add(&d);
                    groups
                        .entry((row.key_name.clone(), row.model.clone()))
                        .or_default()
                        .add(&d);
                }
            }
            previous = Some(row);
        }
        let mut groups: Vec<MissGroup> = groups
            .into_iter()
            .map(|((key, model), counts)| MissGroup { key, model, counts })
            .collect();
        groups.sort_by(|a, b| {
            b.counts
                .changed
                .cmp(&a.counts.changed)
                .then(b.counts.pairs.cmp(&a.counts.pairs))
        });
        groups.truncate(20);
        Ok(Misses {
            total,
            groups,
            truncated,
        })
    }

    /// Delete structures older than `before_ms`.
    pub(crate) fn purge_structure(&mut self, before_ms: i64) -> Result<u64> {
        self.backend.transaction(true, |tx| {
            tx.exec(
                "DELETE FROM request_structure WHERE ts_ms < ?1",
                &[i(before_ms)],
            )
        })
    }
}

fn stored(row: &crate::db::Row) -> Result<Stored> {
    Ok(Stored {
        attempt_id: row.text(0)?,
        ts_ms: row.int(1)?,
        key_name: row.text(2)?,
        provider: row.text(3)?,
        model: row.text(4)?,
        segments: decode(&row.text(5)?),
    })
}

/// How many pairs of consecutive requests fell in each relation.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Counts {
    pub pairs: u64,
    pub identical: u64,
    pub appended: u64,
    pub shorter: u64,
    pub changed: u64,
    /// For the `changed` pairs: how many first differed at a segment of each kind.
    pub changed_by_kind: BTreeMap<String, u64>,
}

impl Counts {
    fn add(&mut self, d: &Diff) {
        self.pairs += 1;
        match d.relation {
            "identical" => self.identical += 1,
            "appended" => self.appended += 1,
            "shorter" => self.shorter += 1,
            _ => {
                self.changed += 1;
                if let Some(change) = &d.first_change {
                    *self.changed_by_kind.entry(change.kind.clone()).or_default() += 1;
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MissGroup {
    pub key: String,
    pub model: String,
    #[serde(flatten)]
    pub counts: Counts,
}

/// The answer of [`Store::structure_misses`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Misses {
    #[serde(flatten)]
    pub total: Counts,
    /// The clients and models with the most changed prefixes, at most 20.
    pub groups: Vec<MissGroup>,
    /// More stored lists than one aggregate reads: the answer covers the first clients by name, in full, and
    /// none of the later ones. Narrow the window.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(kind: &str, hash: &str, bytes: u64) -> Segment {
        Segment {
            kind: kind.into(),
            hash: format!("{hash:0>16}"),
            bytes,
        }
    }

    fn prompt(system: &str, turns: &[&str]) -> Vec<Segment> {
        let mut out = vec![
            seg("tool", "t1", 100),
            seg("tool", "t2", 100),
            seg("system", system, 400),
        ];
        out.extend(turns.iter().map(|h| seg("user", h, 50)));
        out
    }

    #[test]
    fn a_conversation_that_only_grows_is_appended_and_an_earlier_change_is_a_miss_from_there() {
        let before = prompt("s1", &["a", "b"]);
        let d = diff(&before, &prompt("s1", &["a", "b", "c"]));
        assert_eq!(
            (d.relation, d.common_segments, d.total_segments),
            ("appended", 5, 6)
        );
        assert!(d.first_change.is_none());
        assert_eq!(d.common_bytes, 100 + 100 + 400 + 50 + 50);
        let d = diff(&before, &prompt("s2", &["a", "b", "c"]));
        assert_eq!((d.relation, d.common_segments), ("changed", 2));
        let change = d.first_change.unwrap();
        assert_eq!(
            (change.index, change.kind.as_str(), change.label.as_str()),
            (2, "system", "system prompt")
        );
        // The same prompt twice, and a shorter one (a prefix of the earlier request).
        assert_eq!(diff(&before, &before).relation, "identical");
        assert_eq!(diff(&before, &before[..4]).relation, "shorter");
        // A tool definition that moved is named by its place among the tools.
        let mut moved = before.clone();
        moved[1] = seg("tool", "t9", 100);
        assert_eq!(
            diff(&before, &moved).first_change.unwrap().label,
            "tool definition 2"
        );
        // A change in the middle of the messages is named by its segment.
        let mut edited = before.clone();
        edited[4] = seg("user", "z", 50);
        assert_eq!(
            diff(&before, &edited).first_change.unwrap().label,
            "user message (segment 5)"
        );
        // Empty lists are comparable and identical.
        assert_eq!(diff(&[], &[]).relation, "identical");
        assert_eq!(diff(&[], &before).relation, "appended");
    }

    #[test]
    fn parse_accepts_what_a_gateway_sends_and_nothing_else() {
        let good = json!([{"kind": "tool", "hash": "0123456789abcdef", "bytes": 12}, {"kind": "user", "hash": "fedcba9876543210", "bytes": 0}]);
        assert_eq!(parse(Some(&good)).unwrap().len(), 2);
        assert_eq!(parse(Some(&json!([]))).unwrap(), vec![]);
        for bad in [
            json!("not a list"),
            json!([{"kind": "poem", "hash": "0123456789abcdef", "bytes": 1}]),
            json!([{"kind": "tool", "hash": "0123456789ABCDEF", "bytes": 1}]),
            json!([{"kind": "tool", "hash": "0123456789abcde", "bytes": 1}]),
            json!([{"kind": "tool", "hash": "0123456789abcdef", "bytes": -1}]),
            json!([{"kind": "tool", "hash": "0123456789abcdef", "bytes": 1e13}]),
            json!([{"kind": "tool", "hash": "0123456789abcdef"}]),
            json!([["tool", "0123456789abcdef", 1]]),
            Value::Array(
                (0..=MAX_SEGMENTS)
                    .map(|_| json!({"kind": "tool", "hash": "0123456789abcdef", "bytes": 1}))
                    .collect(),
            ),
        ] {
            assert!(parse(Some(&bad)).is_none(), "{bad}");
        }
        assert!(parse(None).is_none());
    }

    #[test]
    fn encode_and_decode_round_trip() {
        let list = prompt("s1", &["a"]);
        assert_eq!(decode(&encode(&list)), list);
        assert_eq!(decode("garbage"), vec![]);
    }
}

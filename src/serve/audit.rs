//! The audit log (note 0047): an append-only, hash-chained record of what changed the running
//! gateway, who or what changed it, and when.
//!
//! One JSON line per event, written and flushed to disk before the call returns. Each record carries
//! the hash of the one before it (`prev`) and its own (`hash`, a SHA-256 over the record's canonical
//! form without the hash), so an edited, removed, inserted or reordered record breaks the chain at
//! a line that `verify` names. Removing records from the **end** of the file is not detectable from
//! the file alone: anchor the head (`head()`, also on the admin status) somewhere the log's writer
//! cannot reach.
//!
//! Never recorded: a secret, a token, a credential digest, a prompt, a completion or any request
//! body. Key names, provider names, configuration section names, hashes and counts are.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The `prev` of the first record: a chain starts here.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// How much of the end of an existing file is read to find its last record.
const TAIL_BYTES: u64 = 1 << 20;

/// The last record written: how many records the chain holds and the hash a next one must carry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Head {
    pub records: u64,
    pub head: String,
}

struct Inner {
    file: File,
    records: u64,
    head: String,
    /// A write failed part way: the file may end in a partial record, so nothing more is appended.
    poisoned: bool,
}

/// An open audit log.
pub struct AuditLog {
    path: PathBuf,
    instance: String,
    inner: Mutex<Inner>,
}

/// `value` with object keys sorted at every depth, as compact JSON: the one form a record is hashed
/// in, so a reader that orders keys differently computes the same hash.
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (n, key) in keys.into_iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The hash a record must carry: SHA-256 of its canonical form without `hash`.
fn digest(record: &Map<String, Value>) -> String {
    let mut body = record.clone();
    body.remove("hash");
    let mut text = String::new();
    canonical(&Value::Object(body), &mut text);
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Parse one line as a record and check it is internally consistent, returning its fields.
fn parse_record(line: &str) -> Result<Map<String, Value>, String> {
    let Value::Object(record) =
        serde_json::from_str::<Value>(line).map_err(|_| "is not a JSON record".to_string())?
    else {
        return Err("is not a JSON object".into());
    };
    let text = |key: &str| record.get(key).and_then(Value::as_str);
    if record.get("seq").and_then(Value::as_u64).is_none()
        || text("prev").is_none()
        || text("hash").is_none()
        || text("event").is_none()
        || text("ts").is_none()
    {
        return Err("is missing a field of the record".into());
    }
    if text("hash") != Some(digest(&record).as_str()) {
        return Err("does not match its own hash (it was altered)".into());
    }
    Ok(record)
}

impl AuditLog {
    /// Open (creating it, owner-only) the log at `path` and continue its chain. Refuses a symlink, a
    /// directory, a file whose last record is incomplete or does not match its own hash: start from
    /// a new file, keeping the old one, after `llmshim audit verify` has told you what happened.
    pub fn open(path: &Path, instance: &str) -> Result<Self, String> {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() {
                return Err(format!("{} is a symbolic link", path.display()));
            }
            if !meta.is_file() {
                return Err(format!("{} is not a regular file", path.display()));
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .map_err(|error| format!("cannot open {}: {}", path.display(), error.kind()))?;
        let (records, head) = Self::tail(&mut file, path)?;
        Ok(Self {
            path: path.to_path_buf(),
            instance: instance.to_owned(),
            inner: Mutex::new(Inner {
                file,
                records,
                head,
                poisoned: false,
            }),
        })
    }

    /// The record count and head of the chain a file already holds, from its last record.
    fn tail(file: &mut File, path: &Path) -> Result<(u64, String), String> {
        let unreadable =
            |error: std::io::Error| format!("cannot read {}: {}", path.display(), error.kind());
        let length = file.metadata().map_err(unreadable)?.len();
        if length == 0 {
            return Ok((0, GENESIS.to_owned()));
        }
        let start = length.saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(start)).map_err(unreadable)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(unreadable)?;
        if bytes.last() != Some(&b'\n') {
            return Err(format!(
                "{} ends in an incomplete record: run `llmshim audit verify`, keep the file and start a new one",
                path.display()
            ));
        }
        let body = &bytes[..bytes.len() - 1];
        let line_start = body
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        if line_start == 0 && start > 0 {
            return Err(format!(
                "the last record of {} is larger than {TAIL_BYTES} bytes",
                path.display()
            ));
        }
        let line = std::str::from_utf8(&body[line_start..])
            .map_err(|_| format!("the last record of {} is not text", path.display()))?;
        let record = parse_record(line)
            .map_err(|reason| format!("the last record of {} {reason}", path.display()))?;
        let seq = record["seq"].as_u64().unwrap_or(0);
        let hash = record["hash"].as_str().unwrap_or_default().to_owned();
        Ok((seq, hash))
    }

    /// Append one record and flush it to disk before returning. `detail` is a JSON object of what
    /// the event is about: names, counts, hashes, never a secret or a body.
    pub fn record(
        &self,
        event: &str,
        actor: &str,
        outcome: &str,
        detail: Value,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.poisoned {
            return Err("the audit log failed part way and takes no more records; restart".into());
        }
        let mut record = Map::new();
        record.insert("seq".into(), json!(inner.records + 1));
        record.insert(
            "ts".into(),
            json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
        record.insert("event".into(), json!(event));
        record.insert("actor".into(), json!(actor));
        record.insert("outcome".into(), json!(outcome));
        record.insert("instance".into(), json!(self.instance));
        record.insert("detail".into(), detail);
        record.insert("prev".into(), json!(inner.head));
        let hash = digest(&record);
        record.insert("hash".into(), json!(hash));
        let mut line = Value::Object(record).to_string();
        line.push('\n');
        let written = inner
            .file
            .write_all(line.as_bytes())
            .and_then(|()| inner.file.sync_data());
        if let Err(error) = written {
            inner.poisoned = true;
            return Err(format!(
                "cannot write {}: {}",
                self.path.display(),
                error.kind()
            ));
        }
        inner.records += 1;
        inner.head = hash;
        Ok(())
    }

    /// How many records the chain holds and the hash the next one carries.
    pub fn head(&self) -> Head {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Head {
            records: inner.records,
            head: inner.head.clone(),
        }
    }
}

/// What `verify` found in an intact chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub records: u64,
    pub head: String,
}

/// The first place a chain is broken: the 1-based line and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broken {
    pub line: u64,
    pub reason: String,
}

/// Check every record of the file at `path`: each is well formed and matches its own hash, carries
/// the hash of the one before (the first carries [`GENESIS`]) and the next sequence number.
pub fn verify(path: &Path) -> Result<Verified, Broken> {
    let text = std::fs::read_to_string(path).map_err(|error| Broken {
        line: 0,
        reason: format!("cannot read {}: {}", path.display(), error.kind()),
    })?;
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(Broken {
            line: text.lines().count() as u64,
            reason: "the file ends in an incomplete record".into(),
        });
    }
    let mut records = 0u64;
    let mut head = GENESIS.to_owned();
    for (index, line) in text.lines().enumerate() {
        let number = index as u64 + 1;
        let broken = |reason: String| Broken {
            line: number,
            reason,
        };
        let record = parse_record(line).map_err(|reason| broken(format!("the record {reason}")))?;
        if record["seq"].as_u64() != Some(records + 1) {
            return Err(broken(format!(
                "sequence number {} where {} was expected (a record was removed, inserted or reordered)",
                record["seq"],
                records + 1
            )));
        }
        if record["prev"].as_str() != Some(head.as_str()) {
            return Err(broken(
                "does not follow the record before it (a record was removed, inserted or reordered)"
                    .into(),
            ));
        }
        head = record["hash"].as_str().unwrap_or_default().to_owned();
        records += 1;
    }
    Ok(Verified { records, head })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llmshim-audit-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("audit.jsonl")
    }

    fn written(path: &Path, events: usize) -> AuditLog {
        let log = AuditLog::open(path, "gw-test").unwrap();
        for n in 0..events {
            log.record(
                "config.reloaded",
                "operator",
                "applied",
                json!({"n": n, "changes": ["keys"]}),
            )
            .unwrap();
        }
        log
    }

    #[test]
    fn a_chain_verifies_and_resumes_after_a_restart_with_the_same_head() {
        let path = temp("resume");
        let log = written(&path, 3);
        let head = log.head();
        assert_eq!(head.records, 3);
        assert_eq!(
            verify(&path).unwrap(),
            Verified {
                records: 3,
                head: head.head.clone()
            }
        );
        drop(log);
        // A new process continues the chain where it ended.
        let reopened = AuditLog::open(&path, "gw-next").unwrap();
        assert_eq!(reopened.head(), head);
        reopened
            .record("server.started", "process", "ok", json!({}))
            .unwrap();
        assert_eq!(verify(&path).unwrap().records, 4);
        // An empty log is a valid, empty chain.
        let empty = temp("empty");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(verify(&empty).unwrap().head, GENESIS);
    }

    #[test]
    fn every_kind_of_tampering_is_named_at_its_line() {
        let path = temp("tamper");
        written(&path, 4);
        let original = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = original.lines().collect();
        let broken_at = |text: String| {
            let probe = temp("probe");
            std::fs::write(&probe, text).unwrap();
            verify(&probe).unwrap_err()
        };
        // An edited field: its own hash no longer matches.
        let edited = lines[1].replace("applied", "refused");
        let found = broken_at(format!(
            "{}\n{}\n{}\n{}\n",
            lines[0], edited, lines[2], lines[3]
        ));
        assert_eq!(found.line, 2, "{found:?}");
        // A removed record: the sequence jumps at the next line.
        let found = broken_at(format!("{}\n{}\n{}\n", lines[0], lines[2], lines[3]));
        assert_eq!(found.line, 2, "{found:?}");
        // A reordered pair.
        let found = broken_at(format!(
            "{}\n{}\n{}\n{}\n",
            lines[0], lines[2], lines[1], lines[3]
        ));
        assert_eq!(found.line, 2, "{found:?}");
        // A duplicated record.
        let found = broken_at(format!(
            "{}\n{}\n{}\n{}\n{}\n",
            lines[0], lines[1], lines[1], lines[2], lines[3]
        ));
        assert_eq!(found.line, 3, "{found:?}");
        // The head removed (the first record): the chain no longer starts at genesis.
        let found = broken_at(format!("{}\n{}\n{}\n", lines[1], lines[2], lines[3]));
        assert_eq!(found.line, 1, "{found:?}");
        // A record rewritten with a self-consistent hash still does not follow its predecessor.
        let mut forged: Map<String, Value> = serde_json::from_str(lines[1]).unwrap();
        forged.insert("outcome".into(), json!("refused"));
        let hash = digest(&forged);
        forged.insert("hash".into(), json!(hash));
        let forged_line = Value::Object(forged).to_string();
        let found = broken_at(format!(
            "{}\n{}\n{}\n{}\n",
            lines[0], forged_line, lines[2], lines[3]
        ));
        assert_eq!(
            found.line, 3,
            "the next record's prev exposes it: {found:?}"
        );
        // A line that is not a record, and a file that ends in a half record.
        assert_eq!(broken_at(format!("{}\nnot json\n", lines[0])).line, 2);
        assert_eq!(
            broken_at(format!("{}\n{}", lines[0], &lines[1][..20])).line,
            2
        );
    }

    #[test]
    fn editing_any_single_field_of_a_record_is_detected() {
        let path = temp("fields");
        written(&path, 3);
        let original = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = original.lines().collect();
        let record: Map<String, Value> = serde_json::from_str(lines[1]).unwrap();
        for field in [
            "seq", "ts", "event", "actor", "outcome", "instance", "detail", "prev", "hash",
        ] {
            // The field replaced by a different value of the same kind.
            let mut edited = record.clone();
            let replaced = match &record[field] {
                Value::String(text) => json!(format!("{text}x")),
                Value::Number(number) => json!(number.as_u64().unwrap() + 40),
                _ => json!({"n": 99999, "changes": ["providers"]}),
            };
            edited.insert(field.into(), replaced);
            let probe = temp("field-probe");
            std::fs::write(
                &probe,
                format!("{}\n{}\n{}\n", lines[0], Value::Object(edited), lines[2]),
            )
            .unwrap();
            let broken = verify(&probe).unwrap_err();
            assert!(
                broken.line <= 3,
                "{field}: an edit of the second record was not detected"
            );
        }
        // A field added to a record is part of what it hashes.
        let mut extra = record.clone();
        extra.insert("note".into(), json!("added"));
        let probe = temp("field-extra");
        std::fs::write(&probe, format!("{}\n{}\n", lines[0], Value::Object(extra))).unwrap();
        assert_eq!(verify(&probe).unwrap_err().line, 2);
    }

    #[test]
    fn truncating_the_end_still_verifies_which_is_why_the_head_is_anchored() {
        let path = temp("truncate");
        let log = written(&path, 4);
        let anchored = log.head();
        let text = std::fs::read_to_string(&path).unwrap();
        let kept: Vec<&str> = text.lines().take(2).collect();
        std::fs::write(&path, format!("{}\n", kept.join("\n"))).unwrap();
        // The file alone is an intact chain of two records ...
        let found = verify(&path).unwrap();
        assert_eq!(found.records, 2);
        // ... and only the anchored head shows that two were removed.
        assert_ne!(found.head, anchored.head);
    }

    #[test]
    fn a_log_that_cannot_be_continued_safely_is_refused_at_open() {
        // A partial last record (a crash mid-write), an altered last record, and a symlink.
        let path = temp("partial");
        written(&path, 2);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"seq\":3,\"ts\"");
        std::fs::write(&path, &text).unwrap();
        let error = AuditLog::open(&path, "gw").err().unwrap();
        assert!(error.contains("incomplete record"), "{error}");
        let path = temp("altered");
        written(&path, 2);
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("applied", "refused");
        std::fs::write(&path, text).unwrap();
        let error = AuditLog::open(&path, "gw").err().unwrap();
        assert!(error.contains("altered"), "{error}");
        #[cfg(unix)]
        {
            let target = temp("target");
            std::fs::write(&target, "").unwrap();
            let link = temp("link").with_file_name("link.jsonl");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let error = AuditLog::open(&link, "gw").err().unwrap();
            assert!(error.contains("symbolic link"), "{error}");
        }
    }

    #[test]
    fn concurrent_writers_keep_one_unbroken_chain_and_a_failed_write_closes_the_log() {
        let path = temp("threads");
        let log = std::sync::Arc::new(AuditLog::open(&path, "gw").unwrap());
        let handles: Vec<_> = (0..8)
            .map(|worker| {
                let log = log.clone();
                std::thread::spawn(move || {
                    for n in 0..25 {
                        log.record(
                            "config.reloaded",
                            "operator",
                            "applied",
                            json!({"w": worker, "n": n}),
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(verify(&path).unwrap().records, 200);
        // After a failed write nothing more is appended (the file may end in half a record).
        log.inner.lock().unwrap().poisoned = true;
        let error = log.record("x", "y", "z", json!({})).unwrap_err();
        assert!(error.contains("takes no more records"), "{error}");
        assert_eq!(verify(&path).unwrap().records, 200);
    }

    #[test]
    fn the_hash_does_not_depend_on_the_order_keys_were_written_in() {
        let a: Map<String, Value> =
            serde_json::from_str(r#"{"seq":1,"detail":{"b":1,"a":[{"y":2,"x":1}]},"event":"e"}"#)
                .unwrap();
        let b: Map<String, Value> =
            serde_json::from_str(r#"{"event":"e","detail":{"a":[{"x":1,"y":2}],"b":1},"seq":1}"#)
                .unwrap();
        assert_eq!(digest(&a), digest(&b));
    }
}
